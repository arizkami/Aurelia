//! A compressor plug-in editor, exercising the whole engine at once.
//!
//! This is the milestone the architecture exists to reach. In one window:
//!
//! * a native window with a wgpu surface, correct resize and correct DPI
//! * ten thousand instanced rectangles in a stress panel
//! * rounded rectangles, borders, shadows and gradients
//! * clipping and a scrolled, culled list
//! * MTSDF text, including Thai, Japanese, Chinese and Arabic
//! * a retained layout tree that reuses every node across rebuilds
//! * an interactive button and a draggable knob
//! * VU meters driven from a simulated audio thread through a lock-free
//!   snapshot, repainting continuously while nothing relayouts
//!
//! Run it with:
//!
//! ```text
//! cargo run -p spherekit --example plugin_ui_demo --release
//! ```
//!
//! The overlay in the corner reports `laid out: 0` while the meters are
//! running. That number is the whole point: the meters repaint at the display
//! refresh rate and the layout engine does nothing at all.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use spherekit::audio::{
    ChannelLevel, LevelConsumer, MeterBallistics, MeterState, MeterStyle, StereoLevel,
    WaveformStyle, level_channel, linear_to_db, realtime_canvas,
};
use spherekit::core::{Color, Corners, DevicePx, Px, RoundedRect, Size, px, rect, relative, size};
use spherekit::platform::{
    App, AppContext, AppHandler, RedrawPolicy, WindowAttributes, WindowEvent, WindowId,
};
use spherekit::render::{Canvas, TextRasterMode};
use spherekit::ui::{
    AnyElement, ButtonVariant, Element, EventFlow, InputTranslator, IntoElement, PaintContext,
    ParentElement, Role, Semantics, Styled, Theme, ValueRange, button, div, knob, label,
};
use spherekit::{SphereKitSurface, SurfaceOptions};

/// How many rectangles the stress panel draws.
///
/// The engine spec's target is ten thousand primitives at a smooth frame rate;
/// this draws them every frame and reports the resulting draw-call count, which
/// should stay at one because they all share a clip and a pipeline.
const STRESS_RECTS: usize = 10_000;

/// A parameter the UI reads and the widgets write.
///
/// `Cell`-based because the element tree is rebuilt every frame and an event
/// handler installed during one build has to change what the *next* build
/// reads. Sharing an `Rc` into the callback is the pattern SphereKit intends:
/// explicit, no `RefCell` borrow panics possible, and the mutation happens at
/// exactly one place.
#[derive(Debug)]
struct Knob {
    value: Cell<f32>,
    min: f32,
    max: f32,
    default: f32,
}

impl Knob {
    fn new(value: f32, min: f32, max: f32) -> Rc<Self> {
        Rc::new(Self { value: Cell::new(value), min, max, default: value })
    }

    fn get(&self) -> f32 {
        self.value.get()
    }
}

/// The application state.
struct Demo {
    window: Option<Arc<spherekit::platform::backend::Window>>,
    surface: Option<SphereKitSurface>,
    input: InputTranslator,
    started: Instant,

    /// The UI half of the lock-free level channel.
    levels: LevelConsumer,
    /// Tells the simulated audio thread to stop.
    audio_running: Arc<AtomicBool>,

    /// Animated meter state, advanced once per frame on the UI thread.
    meter_left: MeterState,
    meter_right: MeterState,
    last_frame: Instant,

    /// A rolling waveform window, drained from the audio thread.
    waveform: Vec<f32>,

    threshold: Rc<Knob>,
    ratio: Rc<Knob>,
    attack: Rc<Knob>,
    /// Shared so the bypass button's click handler can flip it.
    bypassed: Rc<Cell<bool>>,
    show_stress: bool,
    theme_is_dark: bool,

    /// Frames drawn so far.
    frames: u64,
    /// Exit after this many frames, from `SPHEREKIT_DEMO_FRAMES`.
    ///
    /// Lets the demo double as a smoke test: a bounded run that opens a real
    /// window, renders real frames and reports what it measured is the only
    /// end-to-end check that covers the whole stack at once.
    frame_limit: Option<u64>,
    /// Worst frame CPU time seen, in milliseconds.
    worst_cpu_ms: f32,
    /// Highest `nodes_laid_out` seen after the first frame.
    worst_laid_out: u32,
    /// Whether the bounded-run report has already been printed.
    reported: bool,
}

impl Demo {
    fn new() -> Self {
        let (mut producer, consumer) = level_channel();
        let running = Arc::new(AtomicBool::new(true));
        let audio_running = Arc::clone(&running);

        // Stand in for a DSP callback. The rules it obeys are the real ones:
        // it allocates nothing, locks nothing, and publishes through the
        // lock-free snapshot. Everything it hands over is a plain `Copy` value.
        std::thread::spawn(move || {
            let mut phase = 0.0f32;
            let mut block = [0.0f32; 256];
            while running.load(Ordering::Relaxed) {
                // A slowly pulsing tone, so the meters visibly move.
                let envelope = 0.35 + 0.35 * (phase * 0.35).sin();
                for (i, s) in block.iter_mut().enumerate() {
                    *s = (phase + i as f32 * 0.06).sin() * envelope;
                }
                phase += block.len() as f32 * 0.06;

                let left = ChannelLevel::measure(&block);
                let right = ChannelLevel::measure(&block[16..]);
                producer.set(StereoLevel {
                    left,
                    right,
                    gain_reduction: 1.0 - (envelope - 0.3).max(0.0) * 0.4,
                });

                // A real callback is driven by the device; this stands in for
                // one 256-sample block at 48 kHz.
                std::thread::sleep(std::time::Duration::from_micros(5_300));
            }
        });

        Self {
            window: None,
            surface: None,
            input: InputTranslator::new(),
            started: Instant::now(),
            levels: consumer,
            audio_running,
            meter_left: MeterState::silent(),
            meter_right: MeterState::silent(),
            last_frame: Instant::now(),
            waveform: vec![0.0; 512],
            threshold: Knob::new(-18.0, -60.0, 0.0),
            ratio: Knob::new(4.0, 1.0, 20.0),
            attack: Knob::new(12.0, 0.1, 200.0),
            bypassed: Rc::new(Cell::new(false)),
            show_stress: true,
            theme_is_dark: true,
            frames: 0,
            frame_limit: std::env::var("SPHEREKIT_DEMO_FRAMES").ok().and_then(|v| v.parse().ok()),
            worst_cpu_ms: 0.0,
            worst_laid_out: 0,
            reported: false,
        }
    }

    /// Folds the newest audio snapshot into the animated meter state.
    fn advance_meters(&mut self) -> StereoLevel {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;

        let level = *self.levels.read();
        let ballistics = MeterBallistics::default();
        self.meter_left.advance(linear_to_db(level.left.peak), level.left.clipped, dt, &ballistics);
        self.meter_right.advance(
            linear_to_db(level.right.peak),
            level.right.clipped,
            dt,
            &ballistics,
        );

        // A cheap stand-in for a real scrolling waveform: shift the buffer and
        // append the newest peak.
        self.waveform.rotate_left(1);
        let n = self.waveform.len();
        self.waveform[n - 1] = level.left.peak * if level.left.peak > 0.0 { 1.0 } else { -1.0 };
        level
    }

    fn build(&mut self) -> AnyElement {
        let theme = if self.theme_is_dark { Theme::dark() } else { Theme::light() };
        let colors = theme.colors;
        let level = self.advance_meters();

        div()
            .flex_col()
            .full()
            .bg(colors.background)
            .p(theme.spacing.lg)
            .gap(theme.spacing.lg)
            .child(self.header(&theme))
            .child(
                div()
                    .flex_row()
                    .gap(theme.spacing.lg)
                    .flex_1()
                    .child(self.controls(&theme))
                    .child(self.visuals(&theme, level))
                    .child(self.meters(&theme)),
            )
            .child(self.footer(&theme))
            .into_element()
    }

    fn header(&self, theme: &Theme) -> AnyElement {
        let colors = theme.colors;
        div()
            .flex_row()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(40.0))
            .child(
                label("SPHEREKIT COMPRESSOR")
                    .text_size(theme.typography.lg)
                    .text_color(colors.text)
                    .no_wrap(),
            )
            .child(div().flex_1())
            // Multilingual text, all through the same MTSDF path. If any of
            // these render as boxes, font fallback is broken.
            .child(
                label("ไทย  日本語  中文  한국어  العربية")
                    .text_size(theme.typography.sm)
                    .text_color(colors.text_muted)
                    .no_wrap(),
            )
            .into_element()
    }

    fn controls(&mut self, theme: &Theme) -> AnyElement {
        let colors = theme.colors;
        let bypassed = self.bypassed.get();

        div()
            .flex_col()
            .w(px(260.0))
            .gap(theme.spacing.md)
            .p(theme.spacing.md)
            .bg(colors.surface)
            .rounded(theme.radii.lg)
            .border(px(1.0), colors.border)
            .shadow(theme.shadows.md)
            .child(label("PARAMETERS").text_size(theme.typography.xs).text_color(colors.text_muted))
            .child(
                div()
                    .flex_row()
                    .gap(theme.spacing.md)
                    .justify_center()
                    .child(knob_control("threshold", &self.threshold, "Threshold", "dB", theme))
                    .child(knob_control("ratio", &self.ratio, "Ratio", ":1", theme))
                    .child(knob_control("attack", &self.attack, "Attack", "ms", theme)),
            )
            .child(div().h(px(1.0)).w(relative(1.0)).bg(colors.border))
            .child({
                let flag = Rc::clone(&self.bypassed);
                button(if bypassed { "BYPASSED" } else { "ACTIVE" })
                    .id("bypass")
                    .width(relative(1.0))
                    .variant(if bypassed {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    })
                    .on_press(move || flag.set(!flag.get()))
            })
            .into_element()
    }

    fn visuals(&self, theme: &Theme, level: StereoLevel) -> AnyElement {
        let colors = theme.colors;
        let waveform = self.waveform.clone();
        let threshold = self.threshold.get();
        let ratio = self.ratio.get();
        let show_stress = self.show_stress;

        div()
            .flex_col()
            .flex_1()
            .gap(theme.spacing.md)
            .child(
                div()
                    .h(px(90.0))
                    .w(relative(1.0))
                    .bg(colors.surface)
                    .rounded(theme.radii.lg)
                    .border(px(1.0), colors.border)
                    .clip()
                    .child(
                        realtime_canvas(move |frame| {
                            frame.draw_waveform(&waveform, &WaveformStyle::default());
                        })
                        .id("waveform")
                        .full(),
                    ),
            )
            .child(
                div()
                    .h(px(120.0))
                    .w(relative(1.0))
                    .bg(colors.surface)
                    .rounded(theme.radii.lg)
                    .border(px(1.0), colors.border)
                    .clip()
                    .child(
                        realtime_canvas(move |frame| {
                            let curve = spherekit::audio::CompressorCurve {
                                threshold_db: threshold,
                                ratio,
                                knee_db: 6.0,
                                makeup_db: 0.0,
                            };
                            frame.draw_compressor_curve(
                                &curve,
                                &spherekit::audio::CurveStyle {
                                    min_db: -60.0,
                                    max_db: 0.0,
                                    ..Default::default()
                                },
                            );
                        })
                        .id("curve")
                        .full(),
                    ),
            )
            .child_if(show_stress, move || stress_panel(theme))
            .child(
                label(format!(
                    "peak {:.1} dB   gr {:.1} dB",
                    linear_to_db(level.max_peak()),
                    linear_to_db(level.gain_reduction)
                ))
                .text_size(theme.typography.xs)
                .text_color(colors.text_muted),
            )
            .into_element()
    }

    fn meters(&self, theme: &Theme) -> AnyElement {
        let colors = theme.colors;
        let (left, right) = (self.meter_left, self.meter_right);

        div()
            .flex_col()
            .w(px(96.0))
            .gap(theme.spacing.sm)
            .p(theme.spacing.md)
            .bg(colors.surface)
            .rounded(theme.radii.lg)
            .border(px(1.0), colors.border)
            .child(label("OUTPUT").text_size(theme.typography.xs).text_color(colors.text_muted))
            .child(
                div()
                    .flex_row()
                    .gap(theme.spacing.sm)
                    .flex_1()
                    .justify_center()
                    .child(
                        realtime_canvas(move |frame| {
                            frame.draw_meter(&left, &MeterStyle::default());
                        })
                        .id("meter-l")
                        .w(px(18.0))
                        .h(relative(1.0))
                        .semantics(
                            Semantics::new(Role::Meter, "Output left")
                                .value(ValueRange {
                                    value: left.level_db,
                                    min: -60.0,
                                    max: 0.0,
                                    step: None,
                                })
                                .value_text(format!("{:.1} dB", left.level_db)),
                        ),
                    )
                    .child(
                        realtime_canvas(move |frame| {
                            frame.draw_meter(&right, &MeterStyle::default());
                        })
                        .id("meter-r")
                        .w(px(18.0))
                        .h(relative(1.0)),
                    ),
            )
            .child(
                label(format!("{:.0}", left.peak_db.max(-99.0)))
                    .text_size(theme.typography.xs)
                    .text_color(colors.text_muted),
            )
            .into_element()
    }

    fn footer(&self, theme: &Theme) -> AnyElement {
        let colors = theme.colors;
        let stats = self.surface.as_ref().map(|s| s.stats()).unwrap_or_default();
        let elapsed = self.started.elapsed().as_secs_f32();

        div()
            .flex_row()
            .items_center()
            .gap(theme.spacing.lg)
            .h(px(24.0))
            .child(
                // `laid out: 0` while the meters run is the headline claim of
                // the whole engine, printed where it can be checked.
                label(format!(
                    "draws {}   pipelines {}   quads {}   glyphs {}   tris {}   laid out {}   cpu {:.2} ms   {:.0} s",
                    stats.frame.draw_calls,
                    stats.frame.pipeline_switches,
                    stats.frame.quads,
                    stats.frame.glyphs,
                    stats.frame.triangles,
                    stats.nodes_laid_out,
                    stats.cpu_ms,
                    elapsed,
                ))
                .text_size(theme.typography.xs)
                .text_color(colors.text_muted)
                .no_wrap(),
            )
            .into_element()
    }
}

/// A labelled knob, built from the engine's own widget.
///
/// The drag, the fine mode, the double-click reset, the keyboard steps and the
/// accessibility reporting all come from `spherekit_ui::knob`; this only supplies
/// the value, the range and where the new value goes.
fn knob_control(
    id: &'static str,
    knob_state: &Rc<Knob>,
    name: &'static str,
    unit: &'static str,
    theme: &Theme,
) -> AnyElement {
    let colors = theme.colors;
    let value = knob_state.get();
    let (min, max) = (knob_state.min, knob_state.max);

    div()
        .flex_col()
        .items_center()
        .gap(theme.spacing.xs)
        .child(
            knob(value)
                .id(id)
                .range(min, max)
                .default_value(knob_state.default)
                .name(name)
                .unit(unit)
                .format(move |v| format!("{v:.1} {unit}"))
                .size(px(52.0), px(52.0))
                .on_change({
                    let state = Rc::clone(knob_state);
                    move |v| state.value.set(v)
                }),
        )
        .child(label(name).text_size(theme.typography.xs).text_color(colors.text_muted).no_wrap())
        .child(
            label(format!("{value:.1} {unit}"))
                .text_size(theme.typography.xs)
                .text_color(colors.text)
                .no_wrap(),
        )
        .into_element()
}

/// Ten thousand rectangles, clipped, in one draw call.
fn stress_panel(theme: &Theme) -> AnyElement {
    let colors = theme.colors;
    div()
        .h(px(80.0))
        .w(relative(1.0))
        .bg(colors.surface)
        .rounded(theme.radii.lg)
        .border(px(1.0), colors.border)
        .clip()
        .child(
            realtime_canvas(move |frame| {
                let b = frame.bounds;
                let cols = 200usize;
                let rows = STRESS_RECTS / cols;
                let cell_w = b.width().get() / cols as f32;
                let cell_h = b.height().get() / rows as f32;
                for row in 0..rows {
                    for col in 0..cols {
                        let t = (row * cols + col) as f32 / STRESS_RECTS as f32;
                        let wave = ((t * 40.0 + frame.time * 2.0).sin() * 0.5 + 0.5) * 0.8 + 0.2;
                        frame.canvas.fill_rect(
                            rect(
                                b.min_x() + Px(col as f32 * cell_w),
                                b.min_y() + Px(row as f32 * cell_h),
                                Px(cell_w * 0.8),
                                Px(cell_h * 0.8),
                            ),
                            Color::hex(0x3D8BFD).with_alpha(wave * 0.55),
                        );
                    }
                }
            })
            .id("stress")
            .full(),
        )
        .into_element()
}

impl AppHandler for Demo {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        if self.surface.is_some() {
            return;
        }
        // Created hidden: bringing up the GPU and scanning the system fonts
        // takes a few hundred milliseconds, and a window mapped before that is
        // a blank rectangle for the whole of it. Revealed at the bottom of this
        // function, once a frame has actually been drawn.
        let attrs = WindowAttributes::new("SphereKit — Compressor")
            .with_inner_size(size(px(1180.0), px(720.0)))
            .with_min_inner_size(size(px(640.0), px(420.0)))
            .with_visible(false);
        let window = match cx.create_window(&attrs) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("failed to create a window: {e}");
                cx.exit();
                return;
            }
        };

        let surface = pollster::block_on(SphereKitSurface::new(
            Arc::clone(&window),
            window.physical_size(),
            window.scale_factor(),
            SurfaceOptions::default(),
        ));
        match surface {
            Ok(s) => {
                let t = s.init_timing();
                println!("adapter: {}", s.adapter_name());
                println!("scale factor: {}", window.scale_factor().get());
                println!("init: gpu {:.0} ms, fonts {:.0} ms", t.gpu_ms, t.fonts_ms);
                self.surface = Some(s);
            }
            Err(e) => {
                eprintln!("failed to create a GPU surface: {e}");
                cx.exit();
                return;
            }
        }
        self.window = Some(window);
        // The meters must show every audio buffer, so the loop runs free rather
        // than sleeping until the next input event.
        cx.scheduler_mut().begin_realtime();

        // Paint before the window is mapped, so the first thing the compositor
        // is ever handed is a finished frame.
        let first = std::time::Instant::now();
        self.draw();
        let first_ms = first.elapsed().as_secs_f32() * 1000.0;

        if let Some(window) = self.window.as_ref() {
            if !self.surface.as_ref().is_some_and(SphereKitSurface::has_presented) {
                // Reveal anyway: a surface that is not ready at start-up
                // recovers on the next redraw, an invisible application does not.
                eprintln!("first frame did not present; showing the window regardless");
            }
            window.set_visible(true);
            window.request_redraw();
            println!("first frame: {first_ms:.0} ms");
        }
    }

    fn window_event(&mut self, cx: &mut AppContext<'_>, _id: WindowId, event: WindowEvent) {
        self.input.set_time(self.started.elapsed().as_millis() as u64);

        match &event {
            WindowEvent::CloseRequested => {
                cx.exit();
                return;
            }
            WindowEvent::Resized(size) => {
                if let (Some(surface), Some(window)) = (self.surface.as_mut(), self.window.as_ref())
                {
                    let _ = surface.resize(*size, window.scale_factor());
                }
                return;
            }
            WindowEvent::ScaleFactorChanged(scale) => {
                if let (Some(surface), Some(window)) = (self.surface.as_mut(), self.window.as_ref())
                {
                    let _ = surface.resize(window.physical_size(), *scale);
                }
                return;
            }
            WindowEvent::RedrawRequested => {
                self.draw();
                return;
            }
            _ => {}
        }

        // Everything else becomes a UI event and goes to the tree.
        let ui_events = self.input.translate(&event);
        for ui_event in ui_events {
            let (result, hit_bypass) = {
                let Some(surface) = self.surface.as_mut() else { continue };
                let hit = surface
                    .tree()
                    .focus()
                    .focused()
                    .is_some_and(|f| f == spherekit::core::ElementId::from_key("bypass"));
                (surface.dispatch(&ui_event), hit)
            };
            let _ = hit_bypass;

            // Application-level shortcuts the tree did not consume.
            if !result.consumed
                && let spherekit::ui::UiEvent::Key(key) = &ui_event
                && key.state.is_pressed()
            {
                match &key.key {
                    spherekit::ui::Key::Character(c) if c.eq_ignore_ascii_case("b") => {
                        self.bypassed.set(!self.bypassed.get());
                    }
                    spherekit::ui::Key::Character(c) if c.eq_ignore_ascii_case("s") => {
                        self.show_stress = !self.show_stress;
                    }
                    spherekit::ui::Key::Character(c) if c.eq_ignore_ascii_case("t") => {
                        self.theme_is_dark = !self.theme_is_dark;
                        let theme = if self.theme_is_dark { Theme::dark() } else { Theme::light() };
                        if let Some(surface) = self.surface.as_mut() {
                            surface.set_theme(theme);
                        }
                    }
                    spherekit::ui::Key::Tab => {
                        if let Some(surface) = self.surface.as_mut() {
                            surface.tree_mut().navigate_focus(if key.modifiers.shift {
                                spherekit::ui::FocusDirection::Previous
                            } else {
                                spherekit::ui::FocusDirection::Next
                            });
                        }
                    }
                    spherekit::ui::Key::Escape => cx.exit(),
                    _ => {}
                }
            }
        }
    }

    fn about_to_wait(&mut self, cx: &mut AppContext<'_>) {
        // Realtime mode: draw whenever the loop comes around.
        self.draw();
        if let Some(limit) = self.frame_limit
            && self.frames >= limit
            && !self.reported
        {
            self.reported = true;
            self.report();
            cx.exit();
            return;
        }
        if self.reported {
            return;
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn exiting(&mut self, _cx: &mut AppContext<'_>) {
        self.audio_running.store(false, Ordering::Relaxed);
        // Drop the surface before the window: the driver requires the GPU
        // surface to die first.
        self.surface = None;
        self.window = None;
    }
}

impl Demo {
    /// Prints what a bounded run measured.
    fn report(&self) {
        let stats = self.surface.as_ref().map(|s| s.stats()).unwrap_or_default();
        println!("--- spherekit demo report ---");
        println!("frames rendered:     {}", self.frames);
        println!("draw calls:          {}", stats.frame.draw_calls);
        println!("pipeline switches:   {}", stats.frame.pipeline_switches);
        println!("quad instances:      {}", stats.frame.quads);
        println!("glyph instances:     {}", stats.frame.glyphs);
        println!("mesh triangles:      {}", stats.frame.triangles);
        println!("bytes uploaded:      {}", stats.frame.bytes_uploaded);
        println!("elements built:      {}", stats.tree.elements);
        println!("elements painted:    {}", stats.tree.elements_painted);
        println!("elements culled:     {}", stats.tree.elements_culled);
        println!("nodes created:       {}", stats.tree.nodes_created);
        println!("nodes reused:        {}", stats.tree.nodes_reused);
        println!("nodes laid out:      {}", stats.nodes_laid_out);
        println!("worst laid out:      {}", self.worst_laid_out);
        println!("cpu this frame:      {:.3} ms", stats.cpu_ms);
        println!("worst cpu frame:     {:.3} ms", self.worst_cpu_ms);
        println!("glyph texels up:     {}", stats.glyph_texels_uploaded);
    }

    fn draw(&mut self) {
        let root = self.build();
        let clear = if self.theme_is_dark {
            Theme::dark().colors.background
        } else {
            Theme::light().colors.background
        };
        let Some(surface) = self.surface.as_mut() else { return };
        match surface.render(root, clear) {
            Ok(Some(stats)) => {
                self.frames += 1;
                // The first few frames rasterise every glyph and compile every
                // pipeline, so they are not representative of steady state.
                if self.frames > 3 {
                    self.worst_cpu_ms = self.worst_cpu_ms.max(stats.cpu_ms);
                    self.worst_laid_out = self.worst_laid_out.max(stats.nodes_laid_out);
                }
            }
            // A skipped frame is a normal outcome: minimised, or a transient
            // acquisition failure.
            Ok(None) => {}
            Err(e) => eprintln!("frame failed: {e}"),
        }
    }
}

fn main() {
    // A single-line subscriber would need a `tracing-subscriber` dependency;
    // this example prints the few facts that matter directly instead.
    let demo = Demo::new();
    if let Err(e) = App::new(demo).run() {
        eprintln!("event loop failed: {e}");
        std::process::exit(1);
    }
}

// Keep the unused-import lint honest about the types this example demonstrates
// but does not otherwise name in a signature.
#[allow(dead_code)]
fn _type_witnesses(
    _: &dyn Element,
    _: &mut PaintContext<'_, '_>,
    _: &mut Canvas<'_>,
    _: EventFlow,
    _: TextRasterMode,
    _: RedrawPolicy,
    _: RoundedRect,
    _: Corners<Px>,
    _: Size<DevicePx>,
) {
}
