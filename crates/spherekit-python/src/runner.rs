//! The window, the frame loop, and the two calls back into Python.
//!
//! Structurally the same shell as `app/reactdemo`, with one substitution: where
//! that one asks a V8 isolate for the committed tree, this one asks a Python
//! callable. Everything downstream of the commit — lowering, the cascade,
//! layout, paint — is identical, which is the point.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use pyo3::prelude::*;
use spherekit::core::{px, relative, size};
use spherekit::platform::{
    App, AppContext, AppHandler, RedrawPolicy, WindowAttributes, WindowEvent, WindowId,
};
use spherekit::ui::{AnyElement, InputTranslator, IntoElement, ParentElement, Styled, Theme, div};
use spherekit::{SphereKitSurface, SurfaceOptions};
use spherekit_react::{EventQueue, ReactHost};

/// The node types the host lowers to a real widget.
///
/// Anything else still commits — an unknown type becomes a plain container
/// rather than an error, so a Python script written against a newer engine
/// degrades to boxes instead of failing — but these are the ones with
/// behaviour attached.
pub const NODE_TYPES: [&str; 14] = [
    "view",
    "text",
    "button",
    "slider",
    "knob",
    "fader",
    "toggle",
    "checkbox",
    "progress",
    "separator",
    "panel",
    "scroll-view",
    "text-field",
    "avatar",
];

/// Set by `quit()` from inside a Python callback, read by the loop.
///
/// A process-wide flag because there is one event loop per process: a window
/// runs on the thread that called `run`, and a second `run` on another thread
/// is a platform error long before it is an ambiguity about which window this
/// flag meant.
static QUIT: AtomicBool = AtomicBool::new(false);

/// Asks the running loop to close.
pub(crate) fn request_quit() {
    QUIT.store(true, Ordering::Relaxed);
}

/// Clears the flag before a run, so a previous window cannot close this one.
pub(crate) fn clear_quit() {
    QUIT.store(false, Ordering::Relaxed);
}

/// What `run` was asked for.
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// The window title.
    pub title: String,
    /// Logical width.
    pub width: f32,
    /// Logical height.
    pub height: f32,
    /// The stylesheet, installed before the first commit.
    pub css: String,
    /// Whether to use the dark theme.
    pub dark: bool,
    /// Stop after this many frames. For tests and screenshots.
    pub frame_limit: Option<u64>,
}

/// Runs the event loop. Returns a message rather than a Python error type so
/// the caller decides which exception it becomes.
pub(crate) fn run_loop(
    config: RunConfig,
    render: Py<PyAny>,
    on_event: Py<PyAny>,
) -> Result<(), String> {
    let app = PythonApp::new(config, render, on_event);
    App::new(app).run().map_err(|error| format!("event loop failed: {error}"))
}

/// The application shell that drives a Python tree.
pub struct PythonApp {
    config: RunConfig,
    render: Py<PyAny>,
    on_event: Py<PyAny>,
    window: Option<Arc<spherekit::platform::Window>>,
    surface: Option<SphereKitSurface>,
    host: ReactHost,
    /// Where the lowered widgets deposit what the user did.
    events: EventQueue,
    input: InputTranslator,
    started: Instant,
    /// The JSON of the last accepted commit.
    ///
    /// A frame identical to the last one is not committed, which is what makes
    /// an idle window cost a `render()` call and a string comparison instead of
    /// a lowering pass, a cascade and a layout.
    last_tree: String,
    /// The revision counter, owned here rather than by Python.
    ///
    /// The host refuses a commit whose revision did not advance, and a Python
    /// script that forgot to increment would silently stop updating. Counting
    /// on this side means the application cannot get it wrong.
    revision: u64,
    frames: u64,
    /// Set when Python raised; the loop stops rather than calling it again.
    failure: Option<String>,
}

impl PythonApp {
    fn new(config: RunConfig, render: Py<PyAny>, on_event: Py<PyAny>) -> Self {
        Self {
            config,
            render,
            on_event,
            window: None,
            surface: None,
            host: ReactHost::new(),
            events: EventQueue::new(),
            input: InputTranslator::new(),
            started: Instant::now(),
            last_tree: String::new(),
            revision: 0,
            frames: 0,
            failure: None,
        }
    }

    fn theme(&self) -> Theme {
        if self.config.dark { Theme::dark() } else { Theme::light() }
    }

    /// Asks Python for this frame and commits it if it changed.
    ///
    /// The revision is stamped here, on a tree Python did not have to number.
    fn pull_tree(&mut self) {
        if self.failure.is_some() {
            return;
        }
        let json = Python::attach(|python| -> Result<String, String> {
            let produced = self
                .render
                .bind(python)
                .call0()
                .map_err(|error| format!("render() raised: {}", format_error(python, error)))?;
            produced
                .extract::<String>()
                .map_err(|_| "render() must return a JSON string".to_owned())
        });
        let json = match json {
            Ok(json) => json,
            Err(error) => {
                self.failure = Some(error);
                return;
            }
        };
        if json == self.last_tree {
            return;
        }

        self.revision += 1;
        // Stamped rather than trusted. Python builds the children; the number
        // that decides whether a commit is accepted is this side's business.
        let stamped = stamp_revision(&json, self.revision);
        match self.host.commit_json(&stamped) {
            Ok(()) => self.last_tree = json,
            Err(error) => {
                // Not fatal: the previous tree stays on screen, which is a far
                // better failure than a blank window, and the message says
                // exactly which commit was refused.
                eprintln!("spherekit: commit refused: {error}");
            }
        }
    }

    /// Hands everything the widgets reported to Python.
    fn drain_events(&mut self) {
        if self.events.is_empty() || self.failure.is_some() {
            return;
        }
        let queued = self.events.drain();
        let result = Python::attach(|python| -> Result<(), String> {
            for event in queued {
                let payload = match &event.payload {
                    Some(value) => crate::value_to_python(python, value)
                        .map_err(|error| format_error(python, error))?,
                    None => python.None().into_bound(python),
                };
                self.on_event
                    .bind(python)
                    .call1((event.event.as_str(), event.node_id, payload))
                    .map_err(|error| {
                        format!("on_event() raised: {}", format_error(python, error))
                    })?;
            }
            Ok(())
        });
        if let Err(error) = result {
            self.failure = Some(error);
        }
    }

    /// Builds one frame's element tree.
    fn build(&self) -> AnyElement {
        let theme = self.theme();
        div()
            .flex_col()
            .w(relative(1.0))
            .h(relative(1.0))
            .bg(theme.colors.background)
            .child(
                div()
                    .flex_col()
                    .flex_1()
                    // The floor every scrolling column needs: without it the
                    // pane grows to its content instead of the window.
                    .min_h(px(0.0))
                    .child(self.host.ui_element_with_events(&self.events)),
            )
            .into_element()
    }

    fn draw(&mut self) {
        self.pull_tree();
        let root = self.build();
        let clear = self.theme().colors.background;
        let Some(surface) = self.surface.as_mut() else { return };
        match surface.render(root, clear) {
            Ok(Some(_)) => self.frames += 1,
            Ok(None) => {}
            Err(error) => eprintln!("spherekit: frame failed: {error}"),
        }
        self.drain_events();
    }

    /// Whether the loop should stop, and why.
    fn should_exit(&self) -> bool {
        QUIT.load(Ordering::Relaxed)
            || self.failure.is_some()
            || self.config.frame_limit.is_some_and(|limit| self.frames >= limit)
    }

    fn report_failure(&self) {
        if let Some(error) = self.failure.as_ref() {
            eprintln!("spherekit: {error}");
        }
    }
}

impl AppHandler for PythonApp {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        if self.surface.is_some() {
            return;
        }
        // The stylesheet goes in before the first commit. Installed after, the
        // first frame paints unstyled and corrects itself a frame later, which
        // is visible as a flash on every start-up.
        if !self.config.css.is_empty()
            && let Err(error) = self.host.set_stylesheet(&self.config.css)
        {
            self.failure = Some(format!("stylesheet: {error}"));
            cx.exit();
            return;
        }

        let attrs = WindowAttributes::new(&self.config.title)
            .with_inner_size(size(px(self.config.width), px(self.config.height)))
            .with_min_inner_size(size(px(320.0), px(240.0)))
            .with_visible(false);
        let window = match cx.create_window(&attrs) {
            Ok(window) => window,
            Err(error) => {
                self.failure = Some(format!("failed to create a window: {error}"));
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
                surface.set_theme(self.theme());
                self.surface = Some(surface);
            }
            Err(error) => {
                self.failure = Some(format!("failed to create a GPU surface: {error}"));
                cx.exit();
                return;
            }
        }

        self.window = Some(window);
        cx.scheduler_mut().set_floor(RedrawPolicy::Idle);
        self.draw();
        if let Some(window) = self.window.as_ref() {
            window.set_visible(true);
            window.request_redraw();
        }
    }

    fn window_event(&mut self, cx: &mut AppContext<'_>, _id: WindowId, event: WindowEvent) {
        self.input.set_time(self.started.elapsed().as_millis() as u64);

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

        let mut needs_redraw = false;
        for ui_event in self.input.translate(&event) {
            if let Some(surface) = self.surface.as_mut() {
                let result = surface.dispatch(&ui_event);
                needs_redraw |= result.repaint || result.relayout || result.focus_changed;
            }
        }
        // Before the redraw, not after: a press has to reach Python and change
        // its state in time for the frame that press caused, or every
        // interaction lands one frame late.
        self.drain_events();
        if self.should_exit() {
            self.report_failure();
            cx.exit();
            return;
        }
        if (needs_redraw || !self.events.is_empty())
            && let Some(window) = self.window.as_ref()
        {
            window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, cx: &mut AppContext<'_>) {
        if self.should_exit() {
            self.report_failure();
            cx.exit();
            return;
        }
        // A frame limit means a demo or a test, and neither has anyone to
        // produce input: draw continuously so it finishes.
        if self.config.frame_limit.is_some() {
            self.draw();
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
            return;
        }
        let animating = self
            .surface
            .as_ref()
            .is_some_and(|surface| surface.stats().animating || surface.needs_paint());
        if animating && let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn exiting(&mut self, _cx: &mut AppContext<'_>) {
        // The GPU surface borrows the window and must die first.
        self.surface = None;
        self.window = None;
    }
}

/// Puts `revision` into a tree JSON string the Python side did not number.
///
/// Textual rather than a parse-and-re-serialise round trip, because the string
/// is about to be parsed by the host anyway and doing it twice per frame is the
/// kind of cost that only shows up on a big tree.
fn stamp_revision(json: &str, revision: u64) -> String {
    // A tree that already names a revision has to be rewritten properly. Two
    // `revision` keys are legal JSON and the *last* one wins, so prepending
    // onto a tree that has its own would leave the number Python chose in
    // charge — and a Python side that never incremented it would look frozen
    // with nothing to point at.
    if json.contains("\"revision\"") {
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json)
            && let Some(object) = value.as_object_mut()
        {
            object.insert("revision".to_owned(), serde_json::Value::from(revision));
            return value.to_string();
        }
        return json.to_owned();
    }
    let trimmed = json.trim_start();
    match trimmed.strip_prefix('{') {
        // The common path, and the cheap one: the object is about to be parsed
        // by the host anyway, so parsing it here as well would double the cost
        // of every frame on a large tree.
        Some(rest) => format!("{{\"revision\":{revision},{rest}"),
        // Not an object at all: hand it on untouched and let the host produce
        // the error, which will name the real problem.
        None => json.to_owned(),
    }
}

/// Renders a Python exception, with its traceback when there is one.
fn format_error(python: Python<'_>, error: PyErr) -> String {
    let message = error.to_string();
    match error.traceback(python).and_then(|tb| tb.format().ok()) {
        Some(traceback) => format!("{message}\n{traceback}"),
        None => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_is_stamped_onto_a_tree_that_has_none() {
        let stamped = stamp_revision(r#"{"children":[]}"#, 7);
        assert_eq!(stamped, r#"{"revision":7,"children":[]}"#);
        let value: serde_json::Value = serde_json::from_str(&stamped).expect("still parses");
        assert_eq!(value["revision"], 7);
    }

    #[test]
    fn stamping_survives_leading_whitespace() {
        let stamped = stamp_revision("  \n{\"children\":[]}", 2);
        let value: serde_json::Value = serde_json::from_str(&stamped).expect("still parses");
        assert_eq!(value["revision"], 2);
    }

    #[test]
    fn a_revision_python_supplied_is_replaced_rather_than_duplicated() {
        // Two `revision` keys are legal JSON and the last one wins. Prepending
        // onto a tree that carries its own would leave Python in charge of the
        // number the host uses to accept commits, and a script that never
        // incremented it would freeze the window with nothing to point at.
        let stamped = stamp_revision(r#"{"revision":99,"children":[]}"#, 3);
        let value: serde_json::Value = serde_json::from_str(&stamped).expect("parses");
        assert_eq!(value["revision"], 3);
    }

    #[test]
    fn something_that_is_not_an_object_is_left_for_the_host_to_reject() {
        assert_eq!(stamp_revision("[]", 1), "[]");
    }
}
