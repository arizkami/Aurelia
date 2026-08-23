//! # SphereKit Music Player
//!
//! A music player whose entire interface is React 19 running inside SphereKit's
//! own V8 isolate, rendered on the GPU, with realtime visualisation fed straight
//! from the audio being played.
//!
//! ```text
//! cargo run -p musicplayer --release -- "C:\Users\you\Music"
//! ```
//!
//! With no argument it scans your `Music` folder.
//!
//! ## Where each decision is made
//!
//! ```text
//!   React (V8)          Rust
//!   ----------          ----
//!   what to show   -->  commits a tree, resolved through styles/app.css
//!   what to do     -->  player.* API methods
//!                  <--  player.state event, rate-limited
//!
//!   <spectrum/>    -->  a native realtime_canvas that reads the audio ring
//!                       directly. No bridge traffic on the paint path at all.
//! ```
//!
//! The state event is deliberately not per-frame. A commit re-serialises and
//! re-lays-out the entire tree, so pushing one sixty times a second costs more
//! than the audio and the visualisers put together. The visualisers do not care:
//! they never go through React at all.
//!
//! That last line is the point of the whole application. A visualiser that
//! asked JavaScript for its pixels sixty times a second would put the isolate
//! between the audio and the screen. Instead React places the element and Rust
//! owns what it draws, so the audio path and the UI path only ever meet in the
//! layout tree.
//!
//! ## Threads
//!
//! Rodio decodes and mixes on its own thread and must never block, so it does
//! exactly one thing for the display: pushes already-produced samples into a
//! lock-free ring. Everything expensive — the FFT, meter ballistics, the whole
//! React commit — happens here, where being late costs a frame rather than a
//! dropout. See [`audio`].

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analysis;
mod audio;
mod browser;
mod library;
mod player;
mod visualiser;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};
use spherekit::audio::dsp::MIN_DB;
use spherekit::core::{Color, px, size};
use spherekit::css::{MatchPath, Node as CssNode, StyleContext, Stylesheet};
use spherekit::platform::{
    App, AppContext, AppHandler, Theme as PlatformTheme, WindowAttributes, WindowEvent, WindowId,
};
use spherekit::ui::{
    AnyElement, InputTranslator, IntoElement, ParentElement, Styled, Theme, div, label,
};
use spherekit::{SphereKitSurface, SurfaceOptions};
use spherekit_bridge::JsBridge;
use spherekit_react::EventQueue;

use audio::{AudioEngine, AudioTap};
use player::{Player, SharedPlayer};
use visualiser::{SharedVisuals, VisualPipeline};

/// The stylesheet both React and the native chrome resolve through.
const STYLESHEET: &str = include_str!("../styles/app.css");

/// Web globals a bare V8 context does not have and React's modules expect.
const PRELUDE: &str = spherekit_react::PRELUDE;

/// The bundled renderer, produced by `build.rs`.
const RENDERER: &str = include_str!(concat!(env!("OUT_DIR"), "/app.js"));

/// Which renderer `build.rs` managed to produce.
const RENDERER_KIND: &str = env!("SPHEREKIT_MUSICPLAYER_BUNDLE");

/// Samples pulled from the ring per frame.
///
/// One FFT window plus headroom, so a frame that arrives late still has a full
/// window to transform rather than a zero-padded fragment.
const DRAIN_SAMPLES: usize = analysis::FFT_SIZE * 4;

fn main() {
    if let Err(error) = App::new(MusicPlayer::new()).run() {
        eprintln!("event loop failed: {error}");
        std::process::exit(1);
    }
}

/// The application shell.
struct MusicPlayer {
    window: Option<Arc<spherekit::platform::Window>>,
    surface: Option<SphereKitSurface>,
    js: Option<JsBridge>,
    /// Native widget callbacks deposit events here for JavaScript.
    events: EventQueue,
    /// Playback state, shared with the bridge method closures.
    player: SharedPlayer,
    /// What the visualisers read.
    visuals: SharedVisuals,
    pipeline: VisualPipeline,
    /// The UI side of the audio ring.
    tap: Option<AudioTap>,
    stylesheet: Stylesheet,
    input: InputTranslator,
    started: Instant,
    last_frame: Instant,
    frames: u64,
    /// Set by the `app.quit` method; read on the next event turn, because a
    /// bridge method has no `AppContext` to exit with.
    quit: Rc<Cell<bool>>,
    js_error: Option<String>,
    /// The last snapshot sent to React, for the change check in
    /// [`MusicPlayer::state_is_worth_sending`].
    last_sent_state: Option<Value>,
}

impl MusicPlayer {
    fn new() -> Self {
        let root =
            std::env::args_os().nth(1).map(PathBuf::from).or_else(library::default_music_directory);

        let tracks = root.as_deref().map(library::scan).unwrap_or_default();

        // A machine with no output device still gets a window and a playlist.
        // Refusing to start would make the UI untestable on exactly the
        // machines that most need to run it headless.
        let (engine, tap, device_error) = match AudioEngine::new() {
            Ok((engine, tap)) => (Some(engine), Some(tap), None),
            Err(error) => {
                eprintln!("audio unavailable: {error}");
                (None, None, Some(error.to_string()))
            }
        };

        let mut initial_error = device_error;
        if tracks.is_empty() && initial_error.is_none() {
            initial_error = Some(match root.as_deref() {
                Some(path) => format!("no playable audio under {}", path.display()),
                None => "no music folder found; pass one as an argument".to_owned(),
            });
        }

        let (pipeline, visuals) = VisualPipeline::new();

        Self {
            window: None,
            surface: None,
            js: None,
            events: EventQueue::new(),
            player: Rc::new(RefCell::new(Player::new(engine, tracks, initial_error))),
            visuals,
            pipeline,
            tap,
            stylesheet: Stylesheet::parse(STYLESHEET).expect("the bundled stylesheet parses"),
            input: InputTranslator::new(),
            started: Instant::now(),
            last_frame: Instant::now(),
            frames: 0,
            quit: Rc::new(Cell::new(false)),
            js_error: None,
            last_sent_state: None,
        }
    }

    /// Boots V8, installs the stylesheet and host types, and loads the bundle.
    fn start_javascript(&mut self) {
        let mut js = match JsBridge::new() {
            Ok(js) => js,
            Err(error) => {
                self.js_error = Some(error.to_string());
                return;
            }
        };

        {
            let mut bridge = js.bridge_mut();
            let host = bridge.host_mut();
            if let Err(error) = host.set_stylesheet(STYLESHEET) {
                self.js_error = Some(format!("stylesheet: {error}"));
                return;
            }
            // The visualisers become native elements from here on.
            visualiser::register(host, &self.visuals);
            self.register_api(&mut bridge);
        }

        if let Err(error) = js.install_prelude(PRELUDE) {
            self.js_error = Some(format!("prelude: {error}"));
            return;
        }
        if let Err(error) = js.load(RENDERER, "app.js") {
            self.js_error = Some(format!("renderer: {error}"));
            return;
        }

        for line in js.drain_console() {
            println!("js: {line}");
        }
        println!("renderer: {RENDERER_KIND}");
        self.js = Some(js);
    }

    /// Registers every method the React side can call.
    fn register_api(&self, bridge: &mut spherekit_bridge::ApiBridge) {
        /// Reads a numeric parameter, defaulting rather than failing the call.
        fn number(params: &Value, name: &str) -> f64 {
            params.get(name).and_then(Value::as_f64).unwrap_or(0.0)
        }

        let player = Rc::clone(&self.player);
        bridge
            .register_method("player.library", move |_host, _params| Ok(player.borrow().library()));

        let player = Rc::clone(&self.player);
        bridge.register_method("player.state", move |_host, _params| Ok(player.borrow().state()));

        let player = Rc::clone(&self.player);
        bridge.register_method("player.select", move |_host, params| {
            let index = number(&params, "index").max(0.0) as usize;
            player.borrow_mut().select(index);
            Ok(player.borrow().state())
        });

        let player = Rc::clone(&self.player);
        bridge.register_method("player.toggle", move |_host, _params| {
            player.borrow_mut().toggle();
            Ok(player.borrow().state())
        });

        let player = Rc::clone(&self.player);
        bridge.register_method("player.skip", move |_host, params| {
            let delta = number(&params, "delta") as i64;
            player.borrow_mut().skip(if delta == 0 { 1 } else { delta });
            Ok(player.borrow().state())
        });

        let player = Rc::clone(&self.player);
        bridge.register_method("player.seek", move |_host, params| {
            player.borrow_mut().seek(number(&params, "seconds"));
            Ok(player.borrow().state())
        });

        let player = Rc::clone(&self.player);
        bridge.register_method("player.setVolume", move |_host, params| {
            player.borrow_mut().set_volume(number(&params, "volume"));
            Ok(player.borrow().state())
        });

        bridge.register_method("browser.roots", move |_host, _params| Ok(browser::roots_json()));

        bridge.register_method("browser.list", move |_host, params| {
            let path = params.get("path").and_then(Value::as_str).unwrap_or_default();
            Ok(browser::list(std::path::Path::new(path)))
        });

        let player = Rc::clone(&self.player);
        bridge.register_method("browser.open", move |_host, params| {
            let path = params.get("path").and_then(Value::as_str).unwrap_or_default();
            player.borrow_mut().rescan(std::path::Path::new(path));
            Ok(player.borrow().library())
        });

        let quit = Rc::clone(&self.quit);
        bridge.register_method("app.quit", move |_host, _params| {
            quit.set(true);
            Ok(Value::Null)
        });
    }

    /// Pulls audio, advances the analysis, and lets JavaScript catch up.
    fn tick(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().clamp(0.0, 0.25);
        self.last_frame = now;

        // Audio first: the visualisers read the result during this frame's
        // paint, so analysing after the commit would draw last frame's audio.
        let (channels, sample_rate) = match self.tap.as_mut() {
            Some(tap) => {
                let channels = tap.channels();
                let sample_rate = tap.sample_rate();
                let samples = tap.drain(DRAIN_SAMPLES);
                self.pipeline.update(samples, channels, sample_rate, dt);
                (channels, sample_rate)
            }
            None => {
                self.pipeline.update(&[], 2, 44_100.0, dt);
                (2, 44_100.0)
            }
        };
        let _ = (channels, sample_rate);

        let advanced = self.player.borrow_mut().poll_track_finished();

        // Decided before the engine is borrowed, because the check reads
        // `self` and the emit needs `&mut self.js`.
        //
        // Rate-limited on purpose. Every state event makes React re-render, and
        // this renderer commits the *whole* tree — a full serialise, parse,
        // validate, lower and layout of every node, including each playlist
        // row. At sixty of those a second the transport alone costs more than
        // the audio and the visualisers together, which is exactly what the lag
        // was.
        //
        // The visualisers are unaffected: they are native elements reading the
        // audio ring directly, so they stay at frame rate regardless.
        let state = self.player.borrow().state();
        let send_state = self.state_is_worth_sending(&state);
        if send_state {
            self.last_sent_state = Some(state.clone());
        }
        let advanced_index = advanced.then(|| self.player.borrow().state()["index"].clone());

        let Some(js) = self.js.as_mut() else { return };

        js.bridge_mut().pump_events(&self.events);
        if let Err(error) = js.pump_events() {
            eprintln!("event delivery failed: {error}");
        }

        if send_state && let Err(error) = js.emit("player.state", None, Some(state)) {
            eprintln!("state event failed: {error}");
        }
        if let Some(index) = advanced_index {
            // The playlist selection moved without anyone clicking, so the UI
            // is told explicitly rather than inferring it from the state event.
            let _ = js.emit("player.advanced", None, Some(json!({ "index": index })));
        }

        if let Err(error) = js.tick() {
            eprintln!("javascript tick failed: {error}");
        }
        for line in js.drain_console() {
            println!("js: {line}");
        }
    }

    /// Whether this state differs from the last one JavaScript was told about.
    ///
    /// Everything except the position is compared exactly, so a play, a pause,
    /// a track change or a volume nudge reaches React on the very next frame.
    /// Only the clock is quantised, and only to a quarter second — finer than a
    /// listener can see on a scrubber, and 240 times cheaper than every frame.
    fn state_is_worth_sending(&self, state: &Value) -> bool {
        let Some(previous) = self.last_sent_state.as_ref() else { return true };

        for key in ["hasDevice", "playing", "index", "duration", "volume", "error"] {
            if previous.get(key) != state.get(key) {
                return true;
            }
        }

        let position = state.get("position").and_then(Value::as_f64).unwrap_or(0.0);
        let sent = previous.get("position").and_then(Value::as_f64).unwrap_or(0.0);
        (position - sent).abs() >= 0.25
    }

    /// Whether anything on screen is still moving.
    ///
    /// A media player is not an idle document: while a track plays, the meters,
    /// the spectrum and the scrubber all change with no input at all, so the
    /// frame loop has to drive itself. It stops once the audio stops and the
    /// meters have fallen, rather than spinning the GPU on a still window.
    fn needs_animation(&self) -> bool {
        let visuals = self.visuals.borrow();
        if visuals.active {
            return true;
        }
        // Keep going while the meters and spectrum are still decaying, or they
        // freeze part-way down and look stuck.
        visuals.left.level_db > MIN_DB + 1.0
            || visuals.right.level_db > MIN_DB + 1.0
            || visuals.spectrum.iter().any(|magnitude| *magnitude > 1e-4)
    }

    /// Builds one frame's native element tree.
    fn build(&self) -> AnyElement {
        let content = match self.js.as_ref() {
            Some(js) => js.bridge().host().ui_element_with_events(&self.events),
            None => self.unavailable_panel(),
        };

        div()
            .flex_col()
            .full()
            .bg(Color::hex(0x0F1115))
            .child(div().flex_col().flex_1().child(content))
            .child(self.status_bar())
            .into_element()
    }

    /// The native status bar, resolved through the same stylesheet as React.
    fn status_bar(&self) -> AnyElement {
        let context = self.style_context();
        let bar = CssNode::new("view").with_classes("status-bar");
        let ancestors = [bar];

        let item = |text: String, classes: &str| {
            let node = CssNode::new("text").with_classes(classes);
            let path = MatchPath::with_ancestors(&ancestors, node);
            let resolved = self.stylesheet.resolve_in(&path, None, &context);
            resolved.apply_to(resolved.text.apply_to_label(label(text))).into_element()
        };

        let nodes = self.js.as_ref().map(|js| js.bridge().host().node_count()).unwrap_or(0);
        let tracks = self.player.borrow().tracks().len();
        let live = self.visuals.borrow().active;

        self.stylesheet
            .resolve_in(&MatchPath::new(bar), None, &context)
            .apply_to(div())
            .child(item(
                if live { "audio live".to_owned() } else { "silent".to_owned() },
                if live { "status-item live" } else { "status-item" },
            ))
            .child(item(format!("{tracks} tracks"), "status-item"))
            .child(item(format!("{nodes} react nodes"), "status-item"))
            .child(item(format!("frame {}", self.frames), "status-item"))
            .child(item(format!("renderer: {RENDERER_KIND}"), "status-item"))
            .into_element()
    }

    /// Shown when V8 is unavailable on this target.
    fn unavailable_panel(&self) -> AnyElement {
        let reason = self
            .js_error
            .clone()
            .unwrap_or_else(|| "the JavaScript runtime did not start".to_owned());
        div()
            .flex_col()
            .gap(px(8.0))
            .p(px(24.0))
            .child(label("JavaScript runtime unavailable").text_size(px(20.0)))
            .child(label(reason).text_size(px(13.0)).text_color(Color::hex(0x8B93A5)))
            .child(
                label("The V8 prebuilt is Windows x86_64 only; see docs/javascript.md.")
                    .text_size(px(12.0))
                    .text_color(Color::hex(0x5A6070)),
            )
            .into_element()
    }

    fn style_context(&self) -> StyleContext {
        let viewport =
            self.surface.as_ref().map(|s| s.viewport()).unwrap_or(size(px(0.0), px(0.0)));
        StyleContext { viewport, ..StyleContext::default() }
    }

    fn draw(&mut self) {
        self.tick();
        let root = self.build();
        let Some(surface) = self.surface.as_mut() else { return };
        match surface.render(root, Color::hex(0x0F1115)) {
            Ok(Some(_)) => self.frames += 1,
            Ok(None) => {}
            Err(error) => eprintln!("frame failed: {error}"),
        }

        // Drive the next frame from this one. Without this the window only
        // repaints when an event happens to arrive, so the meters sit frozen
        // and every control feels a mouse-move behind.
        if self.needs_animation()
            && let Some(window) = self.window.as_ref()
        {
            window.request_redraw();
        }
    }
}

impl AppHandler for MusicPlayer {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        if self.surface.is_some() {
            return;
        }

        let attrs = WindowAttributes::new("SphereKit — Music")
            .with_inner_size(size(px(1040.0), px(680.0)))
            .with_min_inner_size(size(px(720.0), px(480.0)))
            .with_visible(false);
        let window = match cx.create_window(&attrs) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("failed to create a window: {error}");
                cx.exit();
                return;
            }
        };

        match pollster::block_on(SphereKitSurface::new(
            Arc::clone(&window),
            window.physical_size(),
            window.scale_factor(),
            SurfaceOptions::default(),
        )) {
            Ok(mut surface) => {
                println!("adapter: {}", surface.adapter_name());
                surface.set_theme(match window.theme().unwrap_or(PlatformTheme::Dark) {
                    PlatformTheme::Light => Theme::light(),
                    PlatformTheme::Dark => Theme::dark(),
                });
                self.surface = Some(surface);
            }
            Err(error) => {
                eprintln!("failed to create a GPU surface: {error}");
                cx.exit();
                return;
            }
        }

        self.start_javascript();
        if let Some(error) = self.js_error.as_ref() {
            eprintln!("javascript runtime unavailable: {error}");
        }

        self.draw();
        window.set_visible(true);
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, cx: &mut AppContext<'_>, _id: WindowId, event: WindowEvent) {
        self.input.set_time(self.started.elapsed().as_millis() as u64);
        if self.quit.get() {
            cx.exit();
            return;
        }

        match &event {
            WindowEvent::CloseRequested => {
                cx.exit();
                return;
            }
            WindowEvent::Resized(new_size) => {
                if let (Some(surface), Some(window)) = (self.surface.as_mut(), self.window.as_ref())
                {
                    let _ = surface.resize(*new_size, window.scale_factor());
                }
                self.draw();
                return;
            }
            WindowEvent::ScaleFactorChanged(scale) => {
                if let (Some(surface), Some(window)) = (self.surface.as_mut(), self.window.as_ref())
                {
                    let _ = surface.resize(window.physical_size(), *scale);
                }
                self.draw();
                return;
            }
            WindowEvent::RedrawRequested => {
                self.draw();
                return;
            }
            _ => {}
        }

        for ui_event in self.input.translate(&event) {
            if let Some(surface) = self.surface.as_mut() {
                surface.dispatch(&ui_event);
            }
        }

        // A player redraws continuously whatever the input did: the meters, the
        // spectrum and the scrubber all move on their own. Asking whether an
        // event dirtied anything would be answering the wrong question.
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn exiting(&mut self, _cx: &mut AppContext<'_>) {
        // The surface borrows the window, and the isolate owns callbacks that
        // borrow the bridge. Both die before what they point at.
        self.surface = None;
        self.js = None;
        self.window = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_stylesheet_parses_and_carries_rules() {
        // `styles/app.css` is `include_str!`d, so a typo is a runtime panic in
        // `MusicPlayer::new` rather than a compile error. This is what turns it
        // back into a build failure.
        let sheet = Stylesheet::parse(STYLESHEET).expect("stylesheet parses");
        assert!(sheet.rule_count() > 20, "only {} rules survived", sheet.rule_count());
    }

    #[test]
    fn the_visualiser_classes_the_renderer_uses_are_all_styled() {
        // A visualiser with no size resolves to a zero-height box and paints
        // nothing, which looks exactly like a broken audio path.
        let sheet = Stylesheet::parse(STYLESHEET).expect("stylesheet parses");
        let context = StyleContext::default();
        for class in ["spectrum", "wave", "meters"] {
            let node = CssNode::new("view").with_classes(class);
            let resolved = sheet.resolve_in(&MatchPath::new(node), None, &context);
            assert!(
                resolved.background.is_some(),
                "`.{class}` took no background from the stylesheet"
            );
        }
    }

    #[test]
    fn the_renderer_bundle_is_present_and_non_trivial() {
        assert!(
            RENDERER.len() > 200,
            "build.rs produced a {}-byte renderer, which cannot be either variant",
            RENDERER.len()
        );
        assert!(matches!(RENDERER_KIND, "react" | "fallback"), "unknown renderer {RENDERER_KIND}");
    }

    #[test]
    fn the_visualisers_resolve_to_boxes_with_a_size() {
        // A `realtime_canvas` with no width or height paints nothing and looks
        // exactly like a broken audio path — which is what a missing `flex` or
        // `height` rule produces. Asserting the background is not enough: a
        // zero-sized box has one too.
        let sheet = Stylesheet::parse(STYLESHEET).expect("stylesheet parses");
        let context = StyleContext::default();

        let spectrum = sheet.resolve_in(
            &MatchPath::new(CssNode::new("spectrum").with_classes("spectrum")),
            None,
            &context,
        );
        assert!(spectrum.layout.flex_grow > 0.0, "`.spectrum` will collapse to nothing");

        let wave = sheet.resolve_in(
            &MatchPath::new(CssNode::new("waveform").with_classes("wave")),
            None,
            &context,
        );
        assert!(
            !matches!(wave.layout.size.height, spherekit::core::Length::Auto),
            "`.wave` has no height, so the waveform has nowhere to draw"
        );

        let meter = sheet.resolve_in(
            &MatchPath::new(CssNode::new("level-meter").with_classes("meter")),
            None,
            &context,
        );
        assert!(meter.layout.flex_grow > 0.0, "`.meter` will collapse to nothing");
    }

    #[test]
    fn a_playing_player_keeps_asking_for_frames() {
        // The frame loop drives itself while audio is arriving. Without that
        // the window only repaints when an event happens to turn up, so the
        // meters freeze and every control feels a mouse-move behind — which is
        // exactly how "the visualiser is missing" and "the UI is laggy"
        // presented.
        let player = MusicPlayer::new();
        assert!(!player.needs_animation(), "an idle player should let the window sleep");

        player.visuals.borrow_mut().active = true;
        assert!(player.needs_animation(), "a playing player must schedule the next frame");

        player.visuals.borrow_mut().active = false;
        player.visuals.borrow_mut().spectrum = vec![0.5; 8];
        assert!(
            player.needs_animation(),
            "the meters must keep drawing while they fall, or they stop part-way down"
        );
    }

    #[test]
    fn the_transport_is_not_pushed_to_react_sixty_times_a_second() {
        // Every state event re-commits the whole tree, playlist rows included.
        // Sending one per frame is what made the UI lag, so the change check is
        // load-bearing rather than an optimisation.
        let mut player = MusicPlayer::new();
        let base = json!({
            "hasDevice": true, "playing": true, "index": 0,
            "position": 10.0, "duration": 200.0, "volume": 0.8, "error": null,
        });

        assert!(player.state_is_worth_sending(&base), "the first state must always be sent");
        player.last_sent_state = Some(base.clone());

        let mut nudged = base.clone();
        nudged["position"] = json!(10.05);
        assert!(!player.state_is_worth_sending(&nudged), "a 50 ms move is not worth a full commit");

        let mut moved = base.clone();
        moved["position"] = json!(10.3);
        assert!(player.state_is_worth_sending(&moved), "the scrubber must still track the clock");

        let mut paused = base.clone();
        paused["playing"] = json!(false);
        assert!(player.state_is_worth_sending(&paused), "a pause must reach React immediately");

        let mut skipped = base.clone();
        skipped["index"] = json!(3);
        assert!(player.state_is_worth_sending(&skipped), "a track change must not wait");
    }

    #[test]
    fn the_renderer_places_the_native_visualiser_elements() {
        // The host types only exist because the renderer asks for them. If the
        // bundle stops emitting them the visualisers silently vanish, so this
        // pins the contract from the JavaScript side.
        for host_type in ["spectrum", "level-meter", "waveform"] {
            assert!(RENDERER.contains(host_type), "the renderer stopped placing `{host_type}`");
        }
    }
}
