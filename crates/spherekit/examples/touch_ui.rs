//! Touch: finger scrolling, gestures, and the on-screen keyboard.
//!
//! Everything here works with a mouse too — a finger drives the same pointer
//! path — so it is runnable on a machine with no touchscreen, and the gesture
//! readout simply stays empty.
//!
//! What it demonstrates:
//!
//! * **Drag to scroll.** The list tracks the fingertip one-to-one and coasts
//!   when released. Touching it again catches it.
//! * **A tap is not a scroll.** Rows report a press; a flick through them
//!   reports nothing, because the press is cancelled the moment the finger
//!   passes the slop.
//! * **Gestures.** Swipe, pinch and long press are read off the events and
//!   shown in the status strip.
//! * **The on-screen keyboard.** Presses are applied to whichever field the
//!   application says is being edited, and the keyboard never takes focus.
//!
//! ```bash
//! cargo run -p spherekit --example touch_ui --release
//! SPHEREKIT_DEMO_FRAMES=120 cargo run -p spherekit --example touch_ui --release
//! ```

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use spherekit::core::{px, relative, size};
use spherekit::platform::{
    App, AppContext, AppHandler, RedrawPolicy, WindowAttributes, WindowEvent, WindowId,
};
use spherekit::ui::{
    AnyElement, ButtonVariant, InputTranslator, IntoElement, KeyPress, ParentElement, Styled,
    TextEdit, Theme, UiEvent, button, div, label, scroll_view, text_field, virtual_keyboard,
};
use spherekit::{SphereKitSurface, SurfaceOptions};

/// How many rows the scrolling list has.
const ROWS: usize = 40;

struct State {
    name: RefCell<TextEdit>,
    /// Whether the keyboard is showing. Its own hide key clears it.
    keyboard: Cell<bool>,
    /// The last row a tap landed on, or `None` when nothing has been tapped.
    tapped: Cell<Option<usize>>,
    /// The last gesture, for the status strip.
    gesture: RefCell<String>,
    /// Accumulated pinch scale, so a zoom reads as one continuous number.
    zoom: Cell<f32>,
}

impl State {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            name: RefCell::new(TextEdit::from_text("Take 3")),
            keyboard: Cell::new(true),
            tapped: Cell::new(None),
            gesture: RefCell::new("Drag the list. Flick it. Pinch it.".into()),
            zoom: Cell::new(1.0),
        })
    }

    fn say(&self, message: impl Into<String>) {
        *self.gesture.borrow_mut() = message.into();
    }
}

struct TouchApp {
    window: Option<Arc<spherekit::platform::backend::Window>>,
    surface: Option<SphereKitSurface>,
    input: InputTranslator,
    started: Instant,
    state: Rc<State>,
    frames: u64,
    frame_limit: Option<u64>,
    reported: bool,
}

impl TouchApp {
    fn new() -> Self {
        Self {
            window: None,
            surface: None,
            input: InputTranslator::new(),
            started: Instant::now(),
            state: State::new(),
            frames: 0,
            frame_limit: std::env::var("SPHEREKIT_DEMO_FRAMES").ok().and_then(|v| v.parse().ok()),
            reported: false,
        }
    }

    fn build(&mut self) -> AnyElement {
        let theme = Theme::dark();
        let c = theme.colors;
        let state = Rc::clone(&self.state);

        let mut list = scroll_view()
            .id("rows")
            .flex_1()
            // The rule that catches everyone: without a zero floor the pane
            // grows to fit its content instead of scrolling it.
            .min_h(px(0.0))
            .w(relative(1.0));
        for row in 0..ROWS {
            let s = Rc::clone(&state);
            list = list.child(
                button(format!("Row {row}"))
                    .id(("row", row))
                    .variant(ButtonVariant::Ghost)
                    .width(relative(1.0))
                    .height(px(48.0))
                    // A flex item shrinks to fit by default, and forty rows in
                    // a short pane would each end up a pixel tall.
                    .shrink(0.0)
                    .on_press(move || {
                        s.tapped.set(Some(row));
                        s.say(format!("Tapped row {row}"));
                    }),
            );
        }

        let mut root = div()
            .flex_col()
            .w(relative(1.0))
            .h(relative(1.0))
            .bg(c.background)
            .child(
                div()
                    .flex_row()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .px_(theme.spacing.md)
                    .py_(theme.spacing.sm)
                    .w(relative(1.0))
                    .child({
                        // The field owns a copy for the frame and hands back
                        // what the user did to it; the on-screen keyboard edits
                        // the same original, so the two never disagree.
                        let s = Rc::clone(&state);
                        text_field(state.name.borrow().clone())
                            .id("name")
                            .flex_1()
                            .placeholder("Session name")
                            .on_change(move |edit| *s.name.borrow_mut() = edit.clone())
                    })
                    .child({
                        let s = Rc::clone(&state);
                        button(if state.keyboard.get() { "Hide keys" } else { "Show keys" })
                            .id("keys")
                            .on_press(move || s.keyboard.set(!s.keyboard.get()))
                    }),
            )
            .child(list)
            .child(
                div()
                    .flex_row()
                    .items_center()
                    .h(px(28.0))
                    .px_(theme.spacing.md)
                    .w(relative(1.0))
                    .bg(c.surface)
                    .child(
                        label(format!(
                            "{}   ·   zoom {:.2}×",
                            state.gesture.borrow(),
                            state.zoom.get()
                        ))
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                    ),
            );

        if state.keyboard.get() {
            let s = Rc::clone(&state);
            root = root.child(virtual_keyboard().id("keyboard").height(px(230.0)).on_key(
                move |press| match press {
                    KeyPress::Hide => s.keyboard.set(false),
                    KeyPress::Enter => {
                        let name = s.name.borrow().committed_text().into_owned();
                        s.say(format!("Committed \"{name}\""));
                    }
                    // Everything else edits the field the application says
                    // is being edited. The keyboard never had to know.
                    other => {
                        other.apply(&mut s.name.borrow_mut());
                    }
                },
            ));
        }
        root.into_element()
    }

    /// Reads the gestures the widgets do not handle themselves.
    fn note_gesture(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Swipe(swipe) => {
                self.state.say(format!("Swipe {:?}", swipe.direction));
            }
            UiEvent::LongPress(_) => self.state.say("Long press"),
            UiEvent::Pinch(pinch) => {
                // Total scale, not the per-event delta: the gesture reports
                // where it is relative to where it began.
                self.state.zoom.set((pinch.scale).clamp(0.25, 8.0));
                self.state.say("Pinch");
            }
            _ => {}
        }
    }

    fn draw(&mut self) {
        let root = self.build();
        let clear = Theme::dark().colors.background;
        let Some(surface) = self.surface.as_mut() else { return };
        match surface.render(root, clear) {
            Ok(Some(_)) => self.frames += 1,
            Ok(None) => {}
            Err(e) => eprintln!("frame failed: {e}"),
        }
    }

    fn report(&self) {
        let stats = self.surface.as_ref().map(|s| s.stats()).unwrap_or_default();
        println!("--- touch demo report ---");
        println!("frames rendered:   {}", self.frames);
        println!("nodes laid out:    {}", stats.nodes_laid_out);
        println!("cpu this frame:    {:.3} ms", stats.cpu_ms);
    }
}

impl AppHandler for TouchApp {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        if self.surface.is_some() {
            return;
        }
        let attrs = WindowAttributes::new("SphereKit — Touch")
            .with_inner_size(size(px(480.0), px(820.0)))
            .with_min_inner_size(size(px(360.0), px(480.0)))
            .with_visible(false);
        let window = match cx.create_window(&attrs) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("failed to create a window: {e}");
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
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("failed to create a GPU surface: {e}");
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
            self.note_gesture(&ui_event);
            let result = match self.surface.as_mut() {
                Some(surface) => surface.dispatch(&ui_event),
                None => continue,
            };
            needs_redraw |= result.repaint || result.relayout || result.focus_changed;

            if !result.consumed
                && let UiEvent::Key(key) = &ui_event
                && key.state.is_pressed()
                && matches!(key.key, spherekit::ui::Key::Escape)
            {
                cx.exit();
            }
        }
        // A finger held perfectly still produces no further platform events, so
        // a long press can only be noticed by asking.
        for ui_event in self.input.tick() {
            self.note_gesture(&ui_event);
            if let Some(surface) = self.surface.as_mut() {
                let result = surface.dispatch(&ui_event);
                needs_redraw |= result.repaint;
            }
        }
        if needs_redraw && let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, cx: &mut AppContext<'_>) {
        // A fling keeps moving after the finger is gone, so the window has to
        // keep drawing while the tree says it owes motion.
        self.input.set_time(self.started.elapsed().as_millis() as u64);
        let ticks = self.input.tick();
        let mut needs_redraw = !ticks.is_empty();
        for ui_event in ticks {
            self.note_gesture(&ui_event);
            if let Some(surface) = self.surface.as_mut() {
                needs_redraw |= surface.dispatch(&ui_event).repaint;
            }
        }
        if let Some(surface) = self.surface.as_ref() {
            // `animating` rather than `needs_paint`: a fling is advanced *by*
            // rendering, so by the time a frame is on screen the dirty flag it
            // set has already been consumed. The stat is the only thing that
            // still says motion is owed.
            needs_redraw |= surface.stats().animating || surface.needs_paint();
        }

        match self.frame_limit {
            Some(limit) if self.frames >= limit => {
                if !self.reported {
                    self.reported = true;
                    self.report();
                    cx.exit();
                }
                return;
            }
            Some(_) => {
                self.draw();
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
                return;
            }
            None => {}
        }
        if needs_redraw && let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn exiting(&mut self, _cx: &mut AppContext<'_>) {
        // The GPU surface must die before the window it borrows.
        self.surface = None;
        self.window = None;
    }
}

fn main() {
    if let Err(e) = App::new(TouchApp::new()).run() {
        eprintln!("event loop failed: {e}");
        std::process::exit(1);
    }
}
