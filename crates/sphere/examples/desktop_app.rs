//! A conventional desktop application, built with nothing audio-specific.
//!
//! The other example is a plug-in editor and leans on `sphere-audio-ui`. This
//! one is the shape most applications actually are: a header, a sidebar, a
//! scrolling settings pane, a status bar, and a theme that can be switched at
//! runtime.
//!
//! It is also where the two rendering paths that are *not* analytically
//! antialiased get exercised — the sidebar icons are SVG, tessellated into
//! triangles, and they are smooth because the surface is multisampled.
//!
//! ```text
//! cargo run -p sphere --example desktop_app --release
//! ```
//!
//! Keyboard: Tab and Shift-Tab move focus, Space and Enter activate, arrow keys
//! adjust a focused slider, Ctrl+T switches theme, Escape quits.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use sphere::core::{Color, Px, px, relative, size};
use sphere::platform::{
    App, AppContext, AppHandler, RedrawPolicy, WindowAttributes, WindowEvent, WindowId,
};
use sphere::svg::SvgCache;
use sphere::ui::{
    AnyElement, ButtonVariant, Cursor, EventContext, InputTranslator, Interactive, IntoElement,
    ParentElement, Role, Semantics, Styled, StyledInteraction, Theme, button, checkbox, div, label,
    progress, scroll_view, separator, slider, toggle,
};
use sphere::{SphereSurface, SurfaceOptions};

/// The sections the sidebar navigates between.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Section {
    General,
    Appearance,
    Network,
    About,
}

impl Section {
    const ALL: [Section; 4] =
        [Section::General, Section::Appearance, Section::Network, Section::About];

    fn title(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Network => "Network",
            Section::About => "About",
        }
    }

    /// A monochrome icon, tinted by the theme at draw time.
    ///
    /// Deliberately stroked rather than filled: a stroked path is the shape
    /// most likely to look jagged without multisampling, so it is the honest
    /// thing to put in a demo that claims to have fixed that.
    fn icon(self) -> &'static str {
        match self {
            Section::General => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round">
                     <circle cx="12" cy="12" r="3.2"/>
                     <path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3
                              M5.2 5.2l2.1 2.1M16.7 16.7l2.1 2.1
                              M18.8 5.2l-2.1 2.1M7.3 16.7l-2.1 2.1"/></svg>"##
            }
            Section::Appearance => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round">
                     <path d="M12 3a9 9 0 1 0 0 18c1.4 0 2.2-.9 2.2-2 0-1.4-1.2-1.7-1.2-2.7
                              0-.8.7-1.5 1.6-1.5H16a5 5 0 0 0 5-5c0-3.9-4-6.8-9-6.8z"/>
                     <circle cx="7.6" cy="11.5" r="1.3"/>
                     <circle cx="12" cy="7.6" r="1.3"/>
                     <circle cx="16.4" cy="10.4" r="1.3"/></svg>"##
            }
            Section::Network => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round">
                     <circle cx="12" cy="12" r="9"/>
                     <path d="M3 12h18M12 3c2.6 2.6 2.6 15.4 0 18M12 3c-2.6 2.6-2.6 15.4 0 18"/>
                     </svg>"##
            }
            Section::About => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round">
                     <circle cx="12" cy="12" r="9"/>
                     <path d="M12 11v5.5"/><circle cx="12" cy="7.8" r="1"/></svg>"##
            }
        }
    }
}

/// Everything the UI reads, and everything a handler may write.
///
/// One shared struct of `Cell`s rather than a mutable borrow: the element tree
/// is rebuilt every frame, so a handler installed during one build has to be
/// able to change what the *next* build reads. This is the pattern the engine
/// intends — explicit, and impossible to panic on.
struct State {
    section: Cell<Section>,
    dark: Cell<bool>,
    notifications: Cell<bool>,
    auto_update: Cell<bool>,
    telemetry: Cell<bool>,
    ui_scale: Cell<f32>,
    volume: Cell<f32>,
    download: Cell<f32>,
    log: RefCell<Vec<String>>,
}

impl State {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            section: Cell::new(Section::General),
            dark: Cell::new(true),
            notifications: Cell::new(true),
            auto_update: Cell::new(false),
            telemetry: Cell::new(false),
            ui_scale: Cell::new(100.0),
            volume: Cell::new(65.0),
            download: Cell::new(0.0),
            log: RefCell::new(vec!["Ready.".into()]),
        })
    }

    fn theme(&self) -> Theme {
        if self.dark.get() { Theme::dark() } else { Theme::light() }
    }

    fn say(&self, message: impl Into<String>) {
        let mut log = self.log.borrow_mut();
        log.push(message.into());
        // The log is a UI element, not a record; letting it grow forever would
        // be an unbounded allocation in a long-running window.
        if log.len() > 32 {
            log.remove(0);
        }
    }
}

struct DesktopApp {
    window: Option<Arc<sphere::platform::backend::Window>>,
    surface: Option<SphereSurface>,
    input: InputTranslator,
    started: Instant,
    state: Rc<State>,
    icons: Vec<(Section, sphere::core::SvgId)>,
    frames: u64,
    frame_limit: Option<u64>,
    reported: bool,
}

impl DesktopApp {
    fn new() -> Self {
        Self {
            window: None,
            surface: None,
            input: InputTranslator::new(),
            started: Instant::now(),
            state: State::new(),
            // Populated in `resumed`, against the same cache the painter uses.
            icons: Vec::new(),
            frames: 0,
            frame_limit: std::env::var("SPHERE_DEMO_FRAMES").ok().and_then(|v| v.parse().ok()),
            reported: false,
        }
    }

    fn icon_for(&self, section: Section) -> Option<sphere::core::SvgId> {
        self.icons.iter().find(|(s, _)| *s == section).map(|(_, id)| *id)
    }

    fn build(&mut self) -> AnyElement {
        let theme = self.state.theme();
        let c = theme.colors;

        div()
            .flex_col()
            .full()
            .bg(c.background)
            .child(self.header(&theme))
            .child(
                div()
                    .flex_row()
                    .flex_1()
                    .child(self.sidebar(&theme))
                    .child(separator(true))
                    .child(self.content(&theme)),
            )
            .child(separator(false))
            .child(self.status_bar(&theme))
            .into_element()
    }

    fn header(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let state = Rc::clone(&self.state);
        let dark = self.state.dark.get();

        div()
            .flex_row()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(52.0))
            .px_(theme.spacing.lg)
            .bg(c.surface)
            .child(label("Preferences").text_size(theme.typography.xl).text_color(c.text).no_wrap())
            .child(div().flex_1())
            .child({
                let s = Rc::clone(&state);
                button(if dark { "Light theme" } else { "Dark theme" })
                    .id("theme")
                    .variant(ButtonVariant::Ghost)
                    .on_press(move || {
                        s.dark.set(!s.dark.get());
                        s.say(if s.dark.get() {
                            "Switched to dark."
                        } else {
                            "Switched to light."
                        });
                    })
            })
            .child({
                let s = Rc::clone(&state);
                button("Apply").id("apply").variant(ButtonVariant::Primary).on_press(move || {
                    s.say(format!(
                        "Applied: scale {:.0}%, volume {:.0}%.",
                        s.ui_scale.get(),
                        s.volume.get()
                    ));
                    s.download.set(0.0);
                })
            })
            .into_element()
    }

    fn sidebar(&mut self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let current = self.state.section.get();
        let mut nav = div()
            .flex_col()
            .w(px(210.0))
            .h(relative(1.0))
            .p(theme.spacing.md)
            .gap(theme.spacing.xs)
            .bg(c.surface);

        for section in Section::ALL {
            let selected = section == current;
            let state = Rc::clone(&self.state);
            let icon = self.icon_for(section);
            let tint = if selected { c.text_on_accent } else { c.text_muted };

            nav = nav.child(
                div()
                    .id(section.title())
                    .focusable()
                    .flex_row()
                    .items_center()
                    .gap(theme.spacing.md)
                    .h(px(36.0))
                    .px_(theme.spacing.md)
                    .rounded(theme.radii.md)
                    .bg(if selected { c.accent } else { Color::TRANSPARENT })
                    .hover_bg(if selected { c.accent_hover } else { c.hover })
                    .active_bg(c.pressed)
                    .cursor(Cursor::Pointer)
                    .focus_ring(sphere::ui::FocusRing { color: c.focus, ..Default::default() })
                    .semantics(Semantics::new(Role::Tab, section.title()))
                    .child(IconElement { svg: icon, tint, size: px(20.0) })
                    .child(
                        label(section.title())
                            .text_size(theme.typography.md)
                            .text_color(if selected { c.text_on_accent } else { c.text })
                            .no_wrap(),
                    )
                    .on_click(move |cx: &mut EventContext<'_>| {
                        state.section.set(section);
                        // A section switch changes what is in the pane, so this
                        // one genuinely is a layout change — unlike a hover or
                        // a slider drag, which are repaints.
                        cx.notify_layout();
                    }),
            );
        }
        nav.into_element()
    }

    fn content(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let body = match self.state.section.get() {
            Section::General => self.general(theme),
            Section::Appearance => self.appearance(theme),
            Section::Network => self.network(theme),
            Section::About => self.about(theme),
        };

        div()
            .flex_col()
            .flex_1()
            .h(relative(1.0))
            .bg(c.background)
            .child(
                scroll_view()
                    .id("content-scroll")
                    .flex_1()
                    .w(relative(1.0))
                    .child(div().flex_col().p(theme.spacing.xl).gap(theme.spacing.lg).child(body)),
            )
            .into_element()
    }

    fn general(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let state = Rc::clone(&self.state);

        div()
            .flex_col()
            .gap(theme.spacing.lg)
            .child(section_title("General", theme))
            .child(setting_row(
                "Show notifications",
                "Desktop alerts when a background task finishes.",
                theme,
                {
                    let s = Rc::clone(&state);
                    let on = s.notifications.get();
                    toggle(on)
                        .id("notifications")
                        .label("Show notifications")
                        .on_change(move |v| {
                            s.notifications.set(v);
                            s.say(if v { "Notifications on." } else { "Notifications off." });
                        })
                        .into_element()
                },
            ))
            .child(setting_row(
                "Install updates automatically",
                "Download and apply in the background.",
                theme,
                {
                    let s = Rc::clone(&state);
                    let on = s.auto_update.get();
                    checkbox(on)
                        .id("auto-update")
                        .label("Install updates automatically")
                        .on_change(move |v| {
                            s.auto_update.set(v);
                            s.say(if v { "Auto-update enabled." } else { "Auto-update disabled." });
                        })
                        .into_element()
                },
            ))
            .child(setting_row("Send usage data", "Anonymous, and off by default.", theme, {
                let s = Rc::clone(&state);
                let on = s.telemetry.get();
                checkbox(on)
                    .id("telemetry")
                    .label("Send usage data")
                    .on_change(move |v| s.telemetry.set(v))
                    .into_element()
            }))
            .child(separator(false))
            .child(
                div()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .child(label("Output volume").text_size(theme.typography.md).text_color(c.text))
                    .child({
                        let s = Rc::clone(&state);
                        let v = s.volume.get();
                        slider(v)
                            .id("volume")
                            .range(0.0, 100.0)
                            .default_value(65.0)
                            .name("Output volume")
                            .unit("%")
                            .format(|x| format!("{x:.0}%"))
                            .on_change(move |x| s.volume.set(x))
                    })
                    .child(
                        label(format!("{:.0}%", state.volume.get()))
                            .text_size(theme.typography.sm)
                            .text_color(c.text_muted),
                    ),
            )
            .into_element()
    }

    fn appearance(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let state = Rc::clone(&self.state);

        div()
            .flex_col()
            .gap(theme.spacing.lg)
            .child(section_title("Appearance", theme))
            .child(
                div()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .child(
                        label("Interface scale").text_size(theme.typography.md).text_color(c.text),
                    )
                    .child({
                        let s = Rc::clone(&state);
                        let v = s.ui_scale.get();
                        slider(v)
                            .id("ui-scale")
                            .range(75.0, 200.0)
                            .step(25.0)
                            .default_value(100.0)
                            .name("Interface scale")
                            .format(|x| format!("{x:.0}%"))
                            .on_change(move |x| s.ui_scale.set(x))
                    })
                    .child(
                        label(format!(
                            "{:.0}%  —  stepped, so it lands on the sizes the assets are drawn for",
                            state.ui_scale.get()
                        ))
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                    ),
            )
            .child(separator(false))
            // A type specimen. Every size below goes through the same MTSDF
            // path, and the two smallest cross into the bitmap fallback.
            .child(label("Type specimen").text_size(theme.typography.md).text_color(c.text))
            .child(
                div()
                    .flex_col()
                    .gap(theme.spacing.xs)
                    .child(
                        label("Extra small — 10 px")
                            .text_size(theme.typography.xs)
                            .text_color(c.text),
                    )
                    .child(label("Small — 11 px").text_size(theme.typography.sm).text_color(c.text))
                    .child(
                        label("Medium — 13 px").text_size(theme.typography.md).text_color(c.text),
                    )
                    .child(label("Large — 16 px").text_size(theme.typography.lg).text_color(c.text))
                    .child(
                        label("Extra large — 20 px")
                            .text_size(theme.typography.xl)
                            .text_color(c.text),
                    )
                    .child(
                        label("ไทย · 日本語 · 中文 · 한국어 · العربية · Ελληνικά")
                            .text_size(theme.typography.md)
                            .text_color(c.text_muted),
                    ),
            )
            .into_element()
    }

    fn network(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let state = Rc::clone(&self.state);
        let downloaded = state.download.get();

        div()
            .flex_col()
            .gap(theme.spacing.lg)
            .child(section_title("Network", theme))
            .child(
                div()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .p(theme.spacing.lg)
                    .bg(c.surface)
                    .rounded(theme.radii.lg)
                    .border(px(1.0), c.border)
                    .shadow(theme.shadows.sm)
                    .child(label("Sync status").text_size(theme.typography.md).text_color(c.text))
                    .child(progress(downloaded))
                    .child(
                        label(if downloaded >= 1.0 {
                            "Up to date.".to_string()
                        } else {
                            format!("Downloading… {:.0}%", downloaded * 100.0)
                        })
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                    )
                    .child(div().flex_row().gap(theme.spacing.md).child({
                        let s = Rc::clone(&state);
                        button("Sync now").id("sync").on_press(move || {
                            s.download.set(0.0);
                            s.say("Sync started.");
                        })
                    })),
            )
            .into_element()
    }

    fn about(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let stats = self.surface.as_ref().map(|s| s.stats()).unwrap_or_default();
        let adapter =
            self.surface.as_ref().map(|s| s.adapter_name().to_string()).unwrap_or_default();

        div()
            .flex_col()
            .gap(theme.spacing.md)
            .child(section_title("About", theme))
            .child(label("SphereGraphicEngine").text_size(theme.typography.lg).text_color(c.text))
            .child(
                label("A GPU-first graphics and UI engine written in Rust.")
                    .text_size(theme.typography.md)
                    .text_color(c.text_muted),
            )
            .child(separator(false))
            .child(info_row("Adapter", &adapter, theme))
            .child(info_row("Draw calls", &stats.frame.draw_calls.to_string(), theme))
            .child(info_row("Quad instances", &stats.frame.quads.to_string(), theme))
            .child(info_row("Glyph instances", &stats.frame.glyphs.to_string(), theme))
            .child(info_row("Mesh triangles", &stats.frame.triangles.to_string(), theme))
            .child(info_row("Elements built", &stats.tree.elements.to_string(), theme))
            .child(info_row("Nodes relaid out", &stats.nodes_laid_out.to_string(), theme))
            .child(info_row("CPU per frame", &format!("{:.2} ms", stats.cpu_ms), theme))
            .child(separator(false))
            .child(
                label("BSD 3-Clause · Futureboard Digital Technologies")
                    .text_size(theme.typography.sm)
                    .text_color(c.text_muted),
            )
            .into_element()
    }

    fn status_bar(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let last = self.state.log.borrow().last().cloned().unwrap_or_default();
        div()
            .flex_row()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(26.0))
            .px_(theme.spacing.lg)
            .bg(c.surface)
            .child(label(last).text_size(theme.typography.xs).text_color(c.text_muted).no_wrap())
            .child(div().flex_1())
            .child(
                label("Tab to move · Space to activate · Ctrl+T theme · Esc quit")
                    .text_size(theme.typography.xs)
                    .text_color(c.text_muted)
                    .no_wrap(),
            )
            .into_element()
    }
}

fn section_title(text: &str, theme: &Theme) -> AnyElement {
    label(text.to_string())
        .text_size(theme.typography.lg)
        .text_color(theme.colors.text)
        .into_element()
}

fn info_row(name: &str, value: &str, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_row()
        .gap(theme.spacing.md)
        .child(
            label(name.to_string())
                .text_size(theme.typography.sm)
                .text_color(c.text_muted)
                .w(px(160.0)),
        )
        .child(label(value.to_string()).text_size(theme.typography.sm).text_color(c.text))
        .into_element()
}

/// A labelled row with its control on the right.
fn setting_row(title: &str, description: &str, theme: &Theme, control: AnyElement) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.lg)
        .child(
            div()
                .flex_col()
                .flex_1()
                .gap(theme.spacing.xs)
                .child(label(title.to_string()).text_size(theme.typography.md).text_color(c.text))
                .child(
                    label(description.to_string())
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                ),
        )
        .child(control)
        .into_element()
}

/// Draws a cached SVG icon, tinted.
///
/// A minimal custom element: it has no children, no layout of its own beyond a
/// fixed size, and its whole job is one `SvgCache::render` call. Writing one is
/// meant to be this small.
struct IconElement {
    svg: Option<sphere::core::SvgId>,
    tint: Color,
    size: Px,
}

impl sphere::ui::Element for IconElement {
    fn layout_style(&self) -> sphere::layout::Style {
        sphere::layout::Style {
            size: sphere::core::Size {
                width: sphere::core::Length::Px(self.size),
                height: sphere::core::Length::Px(self.size),
            },
            ..sphere::layout::Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut sphere::ui::PaintContext<'_, '_>) {
        // The cache lives in the application, not the element, because an
        // element is rebuilt every frame and a cache that died with it would
        // re-parse every icon sixty times a second.
        let Some(id) = self.svg else { return };
        ICONS.with(|cache| {
            if let Ok(mut cache) = cache.try_borrow_mut() {
                cache.render(id, cx.canvas, cx.bounds, Some(self.tint));
            }
        });
    }
}

thread_local! {
    /// The icon cache the paint pass reaches for.
    ///
    /// Thread-local rather than threaded through `PaintContext`: icons are an
    /// application concern, not an engine one, and the engine should not grow a
    /// field for every asset kind an application might have.
    static ICONS: RefCell<SvgCache> = RefCell::new(SvgCache::new());
}

impl AppHandler for DesktopApp {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        if self.surface.is_some() {
            return;
        }
        // Created hidden on purpose. Bringing up an adapter, a device and a
        // swapchain and then scanning the system fonts takes a few hundred
        // milliseconds, and a window that is mapped before any of that has run
        // is a blank rectangle for the whole of it. `WindowAttributes::visible`
        // documents this as the fix; the reveal is at the bottom of this
        // function, after a frame has actually been drawn.
        let attrs = WindowAttributes::new("SphereGraphicEngine — Preferences")
            .with_inner_size(size(px(980.0), px(640.0)))
            .with_min_inner_size(size(px(560.0), px(380.0)))
            .with_visible(false);
        let window = match cx.create_window(&attrs) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("failed to create a window: {e}");
                cx.exit();
                return;
            }
        };

        match pollster::block_on(SphereSurface::new(
            Arc::clone(&window),
            window.physical_size(),
            window.scale_factor(),
            SurfaceOptions::default(),
        )) {
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

        // Seed the shared icon cache with the same documents.
        ICONS.with(|cache| {
            if let Ok(mut cache) = cache.try_borrow_mut() {
                for section in Section::ALL {
                    let _ = cache.load_str(section.icon());
                }
            }
        });
        // Re-resolve the ids against the cache the painter will actually use.
        self.icons = ICONS.with(|cache| {
            let mut cache = cache.borrow_mut();
            Section::ALL
                .iter()
                .filter_map(|s| cache.load_str(s.icon()).ok().map(|id| (*s, id)))
                .collect()
        });

        self.window = Some(window);
        // A settings window is idle almost all the time; it should cost nothing
        // when nobody is touching it. Only the sync progress animates, so the
        // loop is woken on demand rather than run free.
        cx.scheduler_mut().set_floor(RedrawPolicy::Dirty);

        // Paint before the window is mapped, so the first thing the compositor
        // is ever handed is a finished frame.
        let first = std::time::Instant::now();
        self.draw();
        let first_ms = first.elapsed().as_secs_f32() * 1000.0;

        if let Some(window) = self.window.as_ref() {
            let presented = self.surface.as_ref().is_some_and(SphereSurface::has_presented);
            if !presented {
                // Reveal anyway. A surface that is not ready at start-up
                // recovers on the next redraw; a window that never appears
                // does not, and an invisible application is the worse failure.
                eprintln!("first frame did not present; showing the window regardless");
            }
            window.set_visible(true);
            // Mapping the window invalidates it, and on some platforms the
            // present above went to an unmapped surface. One more frame costs
            // a millisecond and removes the doubt.
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
            if let sphere::ui::UiEvent::Key(key) = &ui_event
                && key.state.is_pressed()
            {
                match &key.key {
                    sphere::ui::Key::Escape => cx.exit(),
                    sphere::ui::Key::Tab => {
                        if let Some(surface) = self.surface.as_mut() {
                            surface.tree_mut().navigate_focus(if key.modifiers.shift {
                                sphere::ui::FocusDirection::Previous
                            } else {
                                sphere::ui::FocusDirection::Next
                            });
                            needs_redraw = true;
                        }
                    }
                    sphere::ui::Key::Character(ch)
                        if ch.eq_ignore_ascii_case("t") && key.modifiers.command() =>
                    {
                        self.state.dark.set(!self.state.dark.get());
                        needs_redraw = true;
                    }
                    _ => {}
                }
            }
        }

        if needs_redraw {
            self.draw();
        }
    }

    fn about_to_wait(&mut self, cx: &mut AppContext<'_>) {
        // The only animation in the window: a sync that fills over a couple of
        // seconds. While it runs the loop keeps drawing; once it finishes the
        // window goes fully idle again.
        let progress = self.state.download.get();
        if progress < 1.0 {
            self.state.download.set((progress + 0.006).min(1.0));
            if self.state.download.get() >= 1.0 {
                self.state.say("Sync complete.");
            }
            self.draw();
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
        }

        if let Some(limit) = self.frame_limit
            && self.frames >= limit
            && !self.reported
        {
            self.reported = true;
            self.report();
            cx.exit();
        }
        if self.frame_limit.is_some() && !self.reported {
            self.draw();
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
        }
    }

    fn exiting(&mut self, _cx: &mut AppContext<'_>) {
        // The GPU surface must die before the window it borrows.
        self.surface = None;
        self.window = None;
    }
}

impl DesktopApp {
    fn report(&self) {
        let stats = self.surface.as_ref().map(|s| s.stats()).unwrap_or_default();
        println!("--- desktop app report ---");
        println!("frames rendered:   {}", self.frames);
        println!("draw calls:        {}", stats.frame.draw_calls);
        println!("quad instances:    {}", stats.frame.quads);
        println!("glyph instances:   {}", stats.frame.glyphs);
        println!("mesh triangles:    {}", stats.frame.triangles);
        println!("elements built:    {}", stats.tree.elements);
        println!("nodes created:     {}", stats.tree.nodes_created);
        println!("nodes reused:      {}", stats.tree.nodes_reused);
        println!("nodes laid out:    {}", stats.nodes_laid_out);
        println!("cpu this frame:    {:.3} ms", stats.cpu_ms);
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
    }
}

fn main() {
    if let Err(e) = App::new(DesktopApp::new()).run() {
        eprintln!("event loop failed: {e}");
        std::process::exit(1);
    }
}
