//! # SphereKit React demo
//!
//! React 19 running inside SphereKit's own V8 isolate, styled by the shared
//! CSS runtime, rendered on the GPU. No browser, no WebView, no Node.
//!
//! ```text
//! cargo run -p reactdemo --release
//! ```
//!
//! ## What is actually happening
//!
//! ```text
//!   App.tsx  --bun-->  one IIFE script
//!                          |
//!                          v
//!   prelude.js  ->  V8 isolate  ->  __spherekitSend(frame)
//!                                        |
//!                                        v
//!                                   ApiBridge  ->  ReactHost.commit
//!                                        |              |
//!                                        |              v
//!                                        |         spherekit-css cascade
//!                                        |              |
//!                                        |              v
//!                                        |         spherekit-ui elements
//!                                        |              |
//!                                        |              v
//!                                        |         layout -> paint -> wgpu
//!                                        |
//!   __spherekitReceive(frame)  <---------+  native events, by node id
//! ```
//!
//! Every arrow is synchronous and in-process. The React reconciler hands the
//! host a *complete* tree at commit time rather than a mutation stream, so the
//! native side never observes a half-built tree and the whole boundary is one
//! serialisable value that a test can assert on.
//!
//! ## The stylesheet is shared, and that is the point
//!
//! `styles/app.css` is installed once. `ReactHost` resolves every committed
//! React node through it, and the status bar at the bottom of this window —
//! which no React component knows exists — is a native `div()` resolved
//! through the very same rules. One cascade, two producers.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

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

/// The stylesheet both the React tree and the native chrome are resolved through.
const STYLESHEET: &str = include_str!("../styles/app.css");

/// Web globals the bare V8 context does not have and React's modules expect.
const PRELUDE: &str = include_str!("../../../crates/spherekit-react/runtime/prelude.js");

/// The bundled renderer, produced by `build.rs`.
const RENDERER: &str = include_str!(concat!(env!("OUT_DIR"), "/app.js"));

/// Which renderer `build.rs` managed to produce.
const RENDERER_KIND: &str = env!("SPHEREKIT_REACTDEMO_BUNDLE");

fn main() {
    if let Err(error) = App::new(ReactDemo::new()).run() {
        eprintln!("event loop failed: {error}");
        std::process::exit(1);
    }
}

/// The application shell.
struct ReactDemo {
    window: Option<Arc<spherekit::platform::Window>>,
    surface: Option<SphereKitSurface>,
    /// The V8 isolate and the API Bridge it drives, absent when the JavaScript
    /// runtime could not start on this platform.
    js: Option<JsBridge>,
    /// Where native widget callbacks deposit events for JavaScript.
    events: EventQueue,
    /// The same stylesheet the React host uses, kept here for the native chrome.
    stylesheet: Stylesheet,
    input: InputTranslator,
    started: Instant,
    frames: u64,
    /// Set by the `app.quit` bridge method; read on the next event turn, because
    /// a bridge method has no `AppContext` to exit with.
    quit: Rc<Cell<bool>>,
    /// Why the JavaScript runtime is unavailable, when it is.
    js_error: Option<String>,
}

impl ReactDemo {
    fn new() -> Self {
        Self {
            window: None,
            surface: None,
            js: None,
            events: EventQueue::new(),
            stylesheet: Stylesheet::parse(STYLESHEET).expect("the bundled stylesheet parses"),
            input: InputTranslator::new(),
            started: Instant::now(),
            frames: 0,
            quit: Rc::new(Cell::new(false)),
            js_error: None,
        }
    }

    /// Boots V8, installs the prelude and the stylesheet, and evaluates the bundle.
    ///
    /// Order matters: the stylesheet has to be in place before the first commit,
    /// or the first frame paints unstyled and then corrects itself a frame later.
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
            if let Err(error) = bridge.host_mut().set_stylesheet(STYLESHEET) {
                self.js_error = Some(format!("stylesheet: {error}"));
                return;
            }
            let quit = Rc::clone(&self.quit);
            bridge.register_method("app.quit", move |_host, _params| {
                quit.set(true);
                Ok(serde_json::Value::Null)
            });
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

    /// Advances the JavaScript side by one frame and drains what it produced.
    fn tick_javascript(&mut self) {
        let Some(js) = self.js.as_mut() else { return };

        // Native events first: a press from the previous frame should be
        // visible to React before its timers and promises run, so a handler and
        // the effect it schedules land in the same commit.
        js.bridge_mut().pump_events(&self.events);
        if let Err(error) = js.pump_events() {
            eprintln!("event delivery failed: {error}");
        }
        if let Err(error) = js.tick() {
            eprintln!("javascript tick failed: {error}");
        }
        for line in js.drain_console() {
            println!("js: {line}");
        }
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
            .bg(Color::hex(0x202124))
            .child(div().flex_col().flex_1().child(content))
            .child(self.status_bar())
            .into_element()
    }

    /// The native status bar, styled by the *same* stylesheet as the React tree.
    ///
    /// Nothing about this element came from React. It exists to show that the
    /// cascade is a shared runtime rather than a React implementation detail.
    fn status_bar(&self) -> AnyElement {
        let context = self.style_context();
        let bar = CssNode::new("view").with_classes("status-bar");
        let ancestors = [bar];

        let item = |text: String, classes: &str| {
            let node = CssNode::new("text").with_classes(classes);
            let path = MatchPath::with_ancestors(&ancestors, node);
            let resolved = self.stylesheet.resolve_in(&path, None, &context);
            let element = resolved.text.apply_to_label(label(text));
            resolved.apply_to(element).into_element()
        };

        let nodes = self.js.as_ref().map(|js| js.bridge().host().node_count()).unwrap_or(0);
        let bar_style =
            self.stylesheet.resolve_in(&MatchPath::new(bar), None, &context).apply_to(div());

        bar_style
            .child(item(format!("frame {}", self.frames), "status-item live"))
            .child(item(format!("{nodes} react nodes"), "status-item"))
            .child(item(format!("renderer: {RENDERER_KIND}"), "status-item"))
            .child(item(format!("v8 {}", spherekit_bridge::v8_version()), "status-item"))
            .into_element()
    }

    /// Shown when V8 is not available on this target.
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
            .child(label(reason).text_size(px(13.0)).text_color(Color::hex(0xACADB0)))
            .child(
                label("The V8 prebuilt is Windows x86_64 only today; see docs/javascript.md.")
                    .text_size(px(12.0))
                    .text_color(Color::hex(0x78797C)),
            )
            .into_element()
    }

    /// The units and media-query context the cascade resolves against.
    fn style_context(&self) -> StyleContext {
        let viewport =
            self.surface.as_ref().map(|s| s.viewport()).unwrap_or(size(px(0.0), px(0.0)));
        StyleContext { viewport, ..StyleContext::default() }
    }

    fn draw(&mut self) {
        self.tick_javascript();
        let root = self.build();
        let Some(surface) = self.surface.as_mut() else { return };
        match surface.render(root, Color::hex(0x202124)) {
            Ok(Some(_)) => self.frames += 1,
            Ok(None) => {}
            Err(error) => eprintln!("frame failed: {error}"),
        }

        // Tell JavaScript a frame happened. This is the native -> JS direction
        // with no React component involved: the header's counter is driven by
        // an event the Rust side raises on its own.
        if let Some(js) = self.js.as_mut() {
            let payload = serde_json::json!({ "count": self.frames });
            if let Err(error) = js.emit("frame", None, Some(payload)) {
                eprintln!("frame event failed: {error}");
            }
        }
    }
}

impl AppHandler for ReactDemo {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        if self.surface.is_some() {
            return;
        }

        let attrs = WindowAttributes::new("SphereKit — React")
            .with_inner_size(size(px(760.0), px(620.0)))
            .with_min_inner_size(size(px(480.0), px(400.0)))
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

        let mut redraw = false;
        for ui_event in self.input.translate(&event) {
            let Some(surface) = self.surface.as_mut() else { continue };
            let result = surface.dispatch(&ui_event);
            redraw |= result.repaint || result.relayout || result.focus_changed;
        }

        // A widget callback that fired above has queued an event. React has to
        // see it and re-commit before the next paint, so any dispatch that
        // produced one owes a frame even if nothing native changed.
        if !self.events.is_empty() {
            redraw = true;
        }

        if redraw && let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn exiting(&mut self, _cx: &mut AppContext<'_>) {
        // The GPU surface borrows the window, and the isolate owns callbacks
        // that borrow the bridge. Both die before what they point at.
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
        // `styles/app.css` is `include_str!`d, so a typo in it is a runtime
        // panic in `ReactDemo::new` rather than a compile error. This is the
        // test that turns it back into a build failure.
        let sheet = Stylesheet::parse(STYLESHEET).expect("stylesheet parses");
        assert!(sheet.rule_count() > 10, "only {} rules survived", sheet.rule_count());
    }

    #[test]
    fn the_native_status_bar_resolves_through_the_shared_stylesheet() {
        // The claim the demo exists to make: a native element that React has
        // never heard of picks up the same cascade.
        let demo = ReactDemo::new();
        let context = StyleContext::default();
        let bar = CssNode::new("view").with_classes("status-bar");
        let resolved = demo.stylesheet.resolve_in(&MatchPath::new(bar), None, &context);
        assert!(resolved.background.is_some(), "the status bar took no background from the sheet");
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

    /// Boots the whole pipeline headlessly: V8, the prelude, the bundle, the
    /// stylesheet and the React host. No window and no GPU.
    #[cfg(windows)]
    fn boot() -> JsBridge {
        let mut js = JsBridge::new().expect("V8 starts");
        js.bridge_mut().host_mut().set_stylesheet(STYLESHEET).expect("stylesheet installs");
        js.install_prelude(PRELUDE).expect("prelude evaluates");
        js.load(RENDERER, "app.js").expect("the renderer bundle evaluates");
        js
    }

    #[cfg(windows)]
    #[test]
    fn the_renderer_commits_a_tree_that_lowers_to_native_elements() {
        // The end-to-end claim, with no window in it: JavaScript running in V8
        // produces a committed tree, and that tree becomes real SphereKit
        // elements through the shared stylesheet.
        let js = boot();
        let bridge = js.bridge();
        let host = bridge.host();
        assert!(host.tree().revision >= 1, "the renderer never committed");
        assert!(host.node_count() > 5, "only {} nodes committed", host.node_count());

        let mut tree = spherekit::ui::UiTree::new();
        tree.build(host.ui_element());
        assert!(tree.stats().elements > 5, "lowered to {} elements", tree.stats().elements);
    }

    #[cfg(windows)]
    #[test]
    fn a_native_event_drives_a_react_re_render() {
        // This is a regression test for a duplicate-React install, which is the
        // one failure mode a renderer package invites and the hardest to read
        // when it happens: `react-reconciler` installs the hook dispatcher onto
        // its own copy of `ReactSharedInternals`, an application component reads
        // a different copy's null one, and the first update originating outside
        // a synchronous render dies with `Cannot read properties of null
        // (reading 'useState')`. Mounting alone does not catch it. Only an
        // update from outside React's own flush cycle does — which is exactly
        // what a native event is.
        let mut js = boot();
        let before = js.bridge().host().tree().revision;

        js.emit("frame", None, Some(serde_json::json!({ "count": 1 }))).expect("event delivers");
        js.tick().expect("tick runs");
        js.tick().expect("second tick runs");

        let after = js.bridge().host().tree().revision;
        assert!(
            after > before,
            "a native event produced no new commit ({before} -> {after}); React is very likely \
             installed twice — check that `react` resolves to one instance across the workspace"
        );
    }

    #[test]
    fn the_prelude_defines_what_a_bare_v8_context_lacks() {
        // React's module evaluation reaches for these before any component
        // renders. Losing one turns into a `ReferenceError` at load time, which
        // is a long way from the line that removed it.
        for global in ["setTimeout", "clearTimeout", "queueMicrotask", "performance", "console"] {
            assert!(PRELUDE.contains(global), "the prelude stopped defining {global}");
        }
    }
}
