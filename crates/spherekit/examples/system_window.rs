//! A conventional window: the platform draws the title bar.
//!
//! The companion to `desktop_app`, which draws its own. Everything above the
//! platform layer is identical — same widgets, same layout, same text stack —
//! and that is the point of the example: choosing who draws the frame is one
//! line in [`WindowAttributes`], not an architecture.
//!
//! It is also the configuration to reach for first. A system title bar gets
//! snap layouts, the window menu, accessibility, and whatever the next Windows
//! release does to captions, for free. Draw your own only when the caption is
//! part of the interface — a tab strip, a transport, a search field.
//!
//! ```bash
//! cargo run -p spherekit --example system_window --release
//! SPHEREKIT_DEMO_FRAMES=120 cargo run -p spherekit --example system_window --release
//! ```

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use spherekit::core::{Px, px, relative, size};
use spherekit::platform::{
    App, AppContext, AppHandler, RedrawPolicy, WindowAttributes, WindowChrome, WindowEvent,
    WindowId,
};
use spherekit::ui::{
    AnyElement, ButtonVariant, InputTranslator, IntoElement, ParentElement, Styled, TextEdit,
    Theme, button, checkbox, div, label, slider, text_field,
};
use spherekit::{SphereKitSurface, SurfaceOptions};

/// Everything the interface reads and writes.
///
/// `Cell` and `RefCell` rather than `&mut`, because the element tree is rebuilt
/// every frame and its callbacks outlive the builder that made them.
struct State {
    dark: Cell<bool>,
    notify: Cell<bool>,
    volume: Cell<f32>,
    name: RefCell<TextEdit>,
    status: RefCell<String>,
}

impl State {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            dark: Cell::new(true),
            notify: Cell::new(true),
            volume: Cell::new(65.0),
            name: RefCell::new(TextEdit::from_text("Untitled project")),
            status: RefCell::new("Ready.".into()),
        })
    }

    fn theme(&self) -> Theme {
        if self.dark.get() { Theme::dark() } else { Theme::light() }
    }

    fn say(&self, message: impl Into<String>) {
        *self.status.borrow_mut() = message.into();
    }
}

struct SystemWindowApp {
    window: Option<Arc<spherekit::platform::backend::Window>>,
    surface: Option<SphereKitSurface>,
    input: InputTranslator,
    started: Instant,
    state: Rc<State>,
    frames: u64,
    frame_limit: Option<u64>,
    reported: bool,
    ime_allowed: bool,
    ime_caret: Option<spherekit::core::Rect<Px>>,
}

impl SystemWindowApp {
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
            ime_allowed: false,
            ime_caret: None,
        }
    }

    fn build(&mut self) -> AnyElement {
        let theme = self.state.theme();
        let c = theme.colors;
        let state = Rc::clone(&self.state);

        div()
            .flex_col()
            .w(relative(1.0))
            .h(relative(1.0))
            .bg(c.background)
            .child(self.body(&theme))
            .child(
                div()
                    .flex_row()
                    .items_center()
                    .h(px(26.0))
                    .px_(theme.spacing.md)
                    .bg(c.surface)
                    .child(
                        label(state.status.borrow().clone())
                            .text_size(theme.typography.sm)
                            .text_color(c.text_muted),
                    ),
            )
            .into_element()
    }

    fn body(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let state = Rc::clone(&self.state);

        div()
            .flex_col()
            .flex_1()
            .gap(theme.spacing.lg)
            .p(theme.spacing.xl)
            .child(
                label("Project settings")
                    .text_size(theme.typography.xl)
                    .text_color(c.text)
                    .no_wrap(),
            )
            .child(
                label(
                    "The platform draws this window's title bar. Compare with the `desktop_app` \
                     example, which draws its own.",
                )
                .text_size(theme.typography.sm)
                .text_color(c.text_muted),
            )
            .child(
                div()
                    .flex_col()
                    .gap(theme.spacing.md)
                    .p(theme.spacing.lg)
                    .bg(c.surface)
                    .rounded(theme.radii.lg)
                    .border(px(1.0), c.border)
                    .child(
                        label("Name")
                            .text_size(theme.typography.md)
                            .weight(theme.typography.strong)
                            .text_color(c.text),
                    )
                    .child({
                        let s = Rc::clone(&state);
                        let done = Rc::clone(&state);
                        text_field(state.name.borrow().clone())
                            .id("name")
                            .placeholder("project name")
                            .on_change(move |e| *s.name.borrow_mut() = e.clone())
                            .on_submit(move |t| done.say(format!("Renamed to {t}.")))
                    })
                    .child({
                        let s = Rc::clone(&state);
                        checkbox(state.notify.get())
                            .id("notify")
                            .label("Notify when a render finishes")
                            .on_change(move |on| {
                                s.notify.set(on);
                                s.say(if on { "Notifications on." } else { "Notifications off." });
                            })
                    })
                    .child(
                        label(format!("Preview volume — {:.0}%", state.volume.get()))
                            .text_size(theme.typography.sm)
                            .text_color(c.text_muted),
                    )
                    .child({
                        let s = Rc::clone(&state);
                        slider(state.volume.get() / 100.0).id("volume").on_change(move |v| {
                            s.volume.set(v * 100.0);
                        })
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_row()
                    .gap(theme.spacing.md)
                    .child(div().flex_1())
                    .child({
                        let s = Rc::clone(&state);
                        button(if self.state.dark.get() { "Light theme" } else { "Dark theme" })
                            .id("theme")
                            .variant(ButtonVariant::Secondary)
                            .on_press(move || s.dark.set(!s.dark.get()))
                    })
                    .child({
                        let s = Rc::clone(&state);
                        button("Save").id("save").variant(ButtonVariant::Primary).on_press(
                            move || {
                                let name = s.name.borrow().committed_text().into_owned();
                                s.say(format!("Saved {name}."));
                            },
                        )
                    }),
            )
            .into_element()
    }

    fn draw(&mut self) {
        let root = self.build();
        let clear = self.state.theme().colors.background;
        let Some(surface) = self.surface.as_mut() else { return };
        match surface.render(root, clear) {
            Ok(Some(_)) => self.frames += 1,
            Ok(None) => {}
            Err(e) => eprintln!("frame failed: {e}"),
        }
        self.apply_ime();
    }

    /// Tells the window whether to compose, and where.
    ///
    /// Read after rendering, because a caret only has a position once the field
    /// has laid its string out — which it does while painting. Diffed before
    /// pushing, because a platform may treat re-enabling an input method as a
    /// reason to cancel the composition in progress.
    fn apply_ime(&mut self) {
        let (Some(surface), Some(window)) = (self.surface.as_ref(), self.window.as_ref()) else {
            return;
        };
        let area = surface.ime();
        let allowed = area.is_some();
        if allowed != self.ime_allowed {
            window.set_ime_allowed(allowed);
            self.ime_allowed = allowed;
        }
        match area {
            Some(area) if self.ime_caret != Some(area.caret) => {
                window.set_ime_cursor_area(area.caret);
                self.ime_caret = Some(area.caret);
            }
            None => self.ime_caret = None,
            _ => {}
        }
    }

    fn report(&self) {
        let stats = self.surface.as_ref().map(|s| s.stats()).unwrap_or_default();
        println!("--- system window report ---");
        println!("frames rendered:   {}", self.frames);
        println!("draw calls:        {}", stats.frame.draw_calls);
        println!("glyph instances:   {}", stats.frame.glyphs);
        println!("nodes laid out:    {}", stats.nodes_laid_out);
        println!("cpu this frame:    {:.3} ms", stats.cpu_ms);
    }
}

impl AppHandler for SystemWindowApp {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        if self.surface.is_some() {
            return;
        }
        // Created hidden and revealed after the first frame, so the window is
        // never on screen as a blank rectangle while the GPU comes up.
        let attrs = WindowAttributes::new("SphereKit — Project settings")
            .with_inner_size(size(px(720.0), px(560.0)))
            .with_min_inner_size(size(px(480.0), px(360.0)))
            // The only line that differs from `desktop_app`.
            .with_chrome(WindowChrome::System)
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
            Ok(s) => {
                let t = s.init_timing();
                println!("adapter: {}", s.adapter_name());
                println!("init: gpu {:.0} ms, fonts {:.0} ms", t.gpu_ms, t.fonts_ms);
                println!("chrome: {:?}", window.chrome());
                self.surface = Some(s);
            }
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
            let result = match self.surface.as_mut() {
                Some(surface) => surface.dispatch(&ui_event),
                None => continue,
            };
            needs_redraw |= result.repaint || result.relayout || result.focus_changed;

            if result.consumed {
                continue;
            }
            if let spherekit::ui::UiEvent::Key(key) = &ui_event
                && key.state.is_pressed()
            {
                match &key.key {
                    spherekit::ui::Key::Escape => cx.exit(),
                    spherekit::ui::Key::Tab => {
                        if let Some(surface) = self.surface.as_mut() {
                            surface.tree_mut().navigate_focus(if key.modifiers.shift {
                                spherekit::ui::FocusDirection::Previous
                            } else {
                                spherekit::ui::FocusDirection::Next
                            });
                            needs_redraw = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(window) = self.window.as_ref()
            && let Some(surface) = self.surface.as_ref()
        {
            let _ = surface;
            if needs_redraw {
                window.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, cx: &mut AppContext<'_>) {
        let Some(limit) = self.frame_limit else { return };
        if self.frames >= limit {
            if !self.reported {
                self.reported = true;
                self.report();
                cx.exit();
            }
            return;
        }
        self.draw();
        if let Some(window) = self.window.as_ref() {
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
    if let Err(e) = App::new(SystemWindowApp::new()).run() {
        eprintln!("event loop failed: {e}");
        std::process::exit(1);
    }
}
