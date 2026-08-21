//! A conventional desktop application, built with nothing audio-specific.
//!
//! The other example is a plug-in editor and leans on `spherekit-audio-ui`. This
//! one is the shape most applications actually are: a compact title bar, a
//! settings sidebar, a scrolling pane, and a status bar. Its custom dark theme
//! uses lifted charcoal surfaces instead of near-black panels.
//!
//! It is also where the two rendering paths that are *not* analytically
//! antialiased get exercised — the sidebar icons are SVG, tessellated into
//! triangles, and they are smooth because the surface is multisampled.
//!
//! ```text
//! cargo run -p spherekit --example desktop_app --release
//! ```
//!
//! Keyboard: Tab and Shift-Tab move focus, Space and Enter activate, arrow keys
//! adjust a focused slider, Escape quits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use spherekit::core::animate::{Drive, Motion};
use spherekit::core::time::{Clock, SystemClock, Timeline};
use spherekit::core::{Color, Px, px, relative, size};
use spherekit::platform::{
    App, AppContext, AppHandler, RedrawPolicy, WindowAttributes, WindowEvent, WindowId,
};
use spherekit::platform::{CaptionRegions, WindowChrome};
use spherekit::svg::SvgCache;
use spherekit::text::FontWeight;
use spherekit::ui::{
    AnyElement, Cursor, Element, EventContext, InputTranslator, Interactive, IntoElement,
    ParentElement, Role, Semantics, Styled, StyledInteraction, TextEdit, Theme, button, checkbox,
    div, label, scroll_view, separator, slider, text_field, toggle,
};
use spherekit::{SphereKitSurface, SurfaceOptions};

/// The desktop example's product theme.
///
/// The application owns this mapping rather than changing SphereKit's default
/// theme: the example is intentionally demonstrating how a product can carry
/// a distinct visual language without globally restyling every consumer.
fn spherekit_dark_theme() -> Theme {
    let mut theme = Theme::dark();
    let c = &mut theme.colors;

    // Neutral graphite base
    c.background = Color::hex(0x2B2C2F);
    c.surface = Color::hex(0x333438);
    c.elevated = Color::hex(0x3B3D41);

    // Interaction states
    c.hover = Color::hex(0x44464A);
    c.pressed = Color::hex(0x4D4F54);

    // Borders
    c.border = Color::hex(0x414348);
    c.border_strong = Color::hex(0x5A5D63);

    // Typography
    c.text = Color::hex(0xF0F0F1);
    c.text_muted = Color::hex(0xACADB0);
    c.text_on_accent = Color::hex(0x202124);

    // Accent — blue แต่ลดความอมฟ้าของทั้ง UI
    c.accent = Color::hex(0x78A8E8);
    c.accent_hover = Color::hex(0x8BB5ED);
    c.focus = Color::hex(0x78A8E8);

    // Semantic
    c.success = Color::hex(0x79B88A);
    c.warning = Color::hex(0xD5AF68);
    c.danger = Color::hex(0xD77880);

    // Typography
    theme.typography.xs = px(10.0);
    theme.typography.sm = px(12.0);
    theme.typography.md = px(14.0);
    theme.typography.lg = px(17.0);
    theme.typography.xl = px(22.0);
    theme.typography.weight = FontWeight::NORMAL;
    theme.typography.strong = FontWeight::SEMI_BOLD;

    // Radius
    theme.radii.sm = px(4.0);
    theme.radii.md = px(6.0);
    theme.radii.lg = px(8.0);

    theme
}

/// Title/status bars sit one step above the panel and content surfaces.
fn chrome_background() -> Color {
    Color::hex(0x343A45)
}

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
    notifications: Cell<bool>,
    auto_update: Cell<bool>,
    telemetry: Cell<bool>,
    ui_scale: Cell<f32>,
    volume: Cell<f32>,
    download: Cell<f32>,
    /// Editable buffers. `RefCell` rather than `Cell` because a `TextEdit` is
    /// not `Copy`; the field takes a clone each frame and hands back the edited
    /// one, which is the same shape as a slider reporting an `f32`.
    /// Whether the window is maximised, so the restore glyph is right.
    maximized: Cell<bool>,
    /// Whether the pointer is over each caption button. Written by the button's
    /// event handler, read by the frame loop.
    wco_hovered: [Cell<bool>; CAPTION_BUTTONS],
    /// The hover fade for each caption button.
    ///
    /// A spring rather than a fixed-duration fade, because a pointer sweeping
    /// across three buttons interrupts every one of them: a spring retargets
    /// from wherever it is with whatever momentum it has, and a tween would
    /// have to restart and jump.
    wco_fade: [Cell<Motion<f32>>; CAPTION_BUTTONS],
    /// A window command the caption asked for, drained by the runner.
    ///
    /// Queued rather than executed inline because a widget callback has no
    /// window: the element tree is deliberately free of platform types, so the
    /// request travels as data and the runner performs it.
    pending: Cell<Option<WindowCommand>>,
    server: RefCell<TextEdit>,
    passphrase: RefCell<TextEdit>,
    log: RefCell<Vec<String>>,
}

impl State {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            section: Cell::new(Section::General),
            notifications: Cell::new(true),
            auto_update: Cell::new(false),
            telemetry: Cell::new(false),
            ui_scale: Cell::new(100.0),
            volume: Cell::new(65.0),
            download: Cell::new(0.0),
            maximized: Cell::new(false),
            wco_hovered: [const { Cell::new(false) }; CAPTION_BUTTONS],
            wco_fade: core::array::from_fn(|_| Cell::new(Motion::at(0.0, Drive::SMOOTH))),
            pending: Cell::new(None),
            server: RefCell::new(TextEdit::from_text("sync.futureboard.local")),
            passphrase: RefCell::new(TextEdit::new()),
            log: RefCell::new(vec!["Ready.".into()]),
        })
    }

    fn theme(&self) -> Theme {
        spherekit_dark_theme()
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
    window: Option<Arc<spherekit::platform::backend::Window>>,
    surface: Option<SphereKitSurface>,
    input: InputTranslator,
    started: Instant,
    state: Rc<State>,
    icons: Vec<(Section, spherekit::core::SvgId)>,
    frames: u64,
    frame_limit: Option<u64>,
    reported: bool,
    /// Last input-method state pushed to the window, so it is only pushed when
    /// it changes. Re-enabling an input method can cancel a composition.
    ime_allowed: bool,
    ime_caret: Option<spherekit::core::Rect<Px>>,
    /// One clock and one delta for every animation in the window.
    timeline: Timeline,
    clock: SystemClock,
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
            ime_allowed: false,
            ime_caret: None,
            timeline: Timeline::new(),
            clock: SystemClock::new(),
            frames: 0,
            frame_limit: std::env::var("SPHEREKIT_DEMO_FRAMES").ok().and_then(|v| v.parse().ok()),
            reported: false,
        }
    }

    fn icon_for(&self, section: Section) -> Option<spherekit::core::SvgId> {
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

        div()
            .flex_row()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(CAPTION_HEIGHT))
            // Padded on the left only: the window buttons run flush to the
            // right edge, exactly as the shell's do.
            .pl(px(12.0))
            .bg(chrome_background())
            .child(
                label("SphereKit")
                    .text_size(theme.typography.sm)
                    .weight(theme.typography.strong)
                    .text_color(c.text)
                    .no_wrap(),
            )
            .child(label("/").text_size(theme.typography.sm).text_color(c.text_muted).no_wrap())
            .child(
                label("Settings").text_size(theme.typography.sm).text_color(c.text_muted).no_wrap(),
            )
            .child(div().flex_1())
            // The window buttons sit inside the caption strip, which is exactly
            // why they have to be published as exclusions: a press the platform
            // routes as caption is swallowed by the modal move loop and never
            // reaches the button at all.
            .child(
                div()
                    .flex_row()
                    .items_center()
                    .child(CaptionButton::element(
                        wco::MINIMIZE,
                        WindowCommand::Minimize,
                        0,
                        &state,
                    ))
                    .child(CaptionButton::element(
                        if self.state.maximized.get() { wco::RESTORE } else { wco::MAXIMIZE },
                        WindowCommand::ToggleMaximize,
                        1,
                        &state,
                    ))
                    .child(CaptionButton::element(wco::CLOSE, WindowCommand::Close, 2, &state)),
            )
            .into_element()
    }

    fn sidebar(&mut self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let current = self.state.section.get();
        let mut nav = div()
            .flex_col()
            .w(px(220.0))
            .h(relative(1.0))
            .p(theme.spacing.md)
            .gap(theme.spacing.xs)
            .bg(c.surface)
            .child(
                label("SETTINGS")
                    .text_size(theme.typography.xs)
                    .weight(theme.typography.strong)
                    .text_color(c.text_muted)
                    .px_(theme.spacing.md)
                    .py_(theme.spacing.sm),
            );

        for section in Section::ALL {
            let selected = section == current;
            let state = Rc::clone(&self.state);
            let icon = self.icon_for(section);
            let tint = if selected { c.accent } else { c.text_muted };

            nav = nav.child(
                div()
                    .id(section.title())
                    .focusable()
                    .flex_row()
                    .items_center()
                    .gap(theme.spacing.md)
                    .h(px(32.0))
                    .px_(theme.spacing.md)
                    .rounded(theme.radii.sm)
                    .bg(if selected { c.pressed } else { Color::TRANSPARENT })
                    .hover_bg(if selected { c.pressed } else { c.hover })
                    .active_bg(c.pressed)
                    .cursor(Cursor::Pointer)
                    .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
                    .semantics(Semantics::new(Role::Tab, section.title()))
                    .child(IconElement { svg: icon, tint, size: px(16.0) })
                    .child(
                        label(section.title())
                            .text_size(theme.typography.sm)
                            .weight(if selected {
                                theme.typography.strong
                            } else {
                                theme.typography.weight
                            })
                            .text_color(c.text)
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
                div()
                    .flex_row()
                    .items_center()
                    .h(px(36.0))
                    .px_(px(12.0))
                    .bg(c.surface)
                    .child(
                        label(self.state.section.get().title())
                            .text_size(theme.typography.sm)
                            .weight(theme.typography.strong)
                            .text_color(c.text)
                            .no_wrap(),
                    )
                    .child(div().flex_1())
                    .child(
                        label("User settings · changes save automatically")
                            .text_size(theme.typography.xs)
                            .text_color(c.text_muted)
                            .no_wrap(),
                    ),
            )
            .child(separator(false))
            .child(
                scroll_view().id("content-scroll").flex_1().w(relative(1.0)).child(
                    div().flex_row().justify_center().w(relative(1.0)).child(
                        div()
                            .flex_col()
                            .w(relative(1.0))
                            .max_w(px(760.0))
                            .p(px(32.0))
                            .gap(theme.spacing.xl)
                            .child(body),
                    ),
                ),
            )
            .into_element()
    }

    fn general(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let state = Rc::clone(&self.state);

        div()
            .flex_col()
            .gap(theme.spacing.md)
            .child(page_header("General", "Everyday behavior for this SphereKit workspace.", theme))
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
            .child(separator(false))
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
            .child(separator(false))
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
                    .child(
                        label("Output volume")
                            .text_size(theme.typography.md)
                            .weight(theme.typography.strong)
                            .text_color(c.text),
                    )
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
            .child(page_header(
                "Appearance",
                "Tune interface density and inspect the active type system.",
                theme,
            ))
            .child(
                div()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .child(
                        label("Interface scale")
                            .text_size(theme.typography.md)
                            .weight(theme.typography.strong)
                            .text_color(c.text),
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
            .child(
                label("Type specimen")
                    .text_size(theme.typography.md)
                    .weight(theme.typography.strong)
                    .text_color(c.text),
            )
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
                    .child(
                        div()
                            .flex_row()
                            .gap(theme.spacing.lg)
                            .child(
                                label("Regular 400")
                                    .text_size(theme.typography.md)
                                    .weight(spherekit::text::FontWeight::NORMAL)
                                    .text_color(c.text),
                            )
                            .child(
                                label("SemiBold 600")
                                    .text_size(theme.typography.md)
                                    .weight(spherekit::text::FontWeight::SEMI_BOLD)
                                    .text_color(c.text),
                            )
                            .child(
                                label("Bold 700")
                                    .text_size(theme.typography.md)
                                    .weight(spherekit::text::FontWeight::BOLD)
                                    .text_color(c.text),
                            ),
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
            .child(page_header(
                "Network",
                "Manage sync state and the server used by this workspace.",
                theme,
            ))
            .child(
                div()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .p(theme.spacing.lg)
                    .bg(c.surface)
                    .rounded(theme.radii.lg)
                    .child(
                        label("Sync status")
                            .text_size(theme.typography.md)
                            .weight(theme.typography.strong)
                            .text_color(c.text),
                    )
                    .child(progress_bar(downloaded, theme))
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
                        button("Sync now")
                            .id("sync")
                            .weight(theme.typography.strong)
                            .on_press(move || {
                                s.download.set(0.0);
                                s.say("Sync started.");
                            })
                    })),
            )
            .child(
                div()
                    .flex_col()
                    .gap(theme.spacing.md)
                    .p(theme.spacing.lg)
                    .bg(c.surface)
                    .rounded(theme.radii.lg)
                    .child(
                        label("Server")
                            .text_size(theme.typography.md)
                            .weight(theme.typography.strong)
                            .text_color(c.text),
                    )
                    .child({
                        let s = Rc::clone(&state);
                        let commit = Rc::clone(&state);
                        text_field(state.server.borrow().clone())
                            .id("server")
                            .placeholder("host name or address")
                            .on_change(move |e| *s.server.borrow_mut() = e.clone())
                            .on_submit(move |text| commit.say(format!("Server set to {text}.")))
                    })
                    .child(
                        label(
                            "Try an input method here — composition stays underlined until it is committed.",
                        )
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                    )
                    .child(
                        label("Passphrase")
                            .text_size(theme.typography.md)
                            .weight(theme.typography.strong)
                            .text_color(c.text),
                    )
                    .child({
                        let s = Rc::clone(&state);
                        text_field(state.passphrase.borrow().clone())
                            .id("passphrase")
                            .placeholder("optional")
                            .mask(true)
                            .on_change(move |e| *s.passphrase.borrow_mut() = e.clone())
                    }),
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
            .child(page_header(
                "About",
                "Runtime and renderer details for this SphereKit build.",
                theme,
            ))
            .child(
                label("SphereKit")
                    .text_size(theme.typography.lg)
                    .weight(FontWeight::BOLD)
                    .text_color(c.text),
            )
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
            .bg(chrome_background())
            .child(label(last).text_size(theme.typography.xs).text_color(c.text_muted).no_wrap())
            .child(div().flex_1())
            .child(
                label("Tab to move · Space to activate · Esc to quit")
                    .text_size(theme.typography.xs)
                    .text_color(c.text_muted)
                    .no_wrap(),
            )
            .into_element()
    }
}

/// What a caption button asks the runner to do.
///
/// A command rather than a direct call: the element tree has no window and is
/// not going to grow one, so the request travels as data.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WindowCommand {
    Minimize,
    ToggleMaximize,
    Close,
}

/// The window-control glyphs, from Segoe Fluent Icons.
///
/// The same codepoints Windows uses for its own caption buttons, so a custom
/// title bar drawn with them is identical to the real one rather than an
/// approximation made of box-drawing characters.
mod wco {
    /// `ChromeMinimize`.
    pub const MINIMIZE: &str = "\u{E921}";
    /// `ChromeMaximize`.
    pub const MAXIMIZE: &str = "\u{E922}";
    /// `ChromeRestore`, shown while the window is maximised.
    pub const RESTORE: &str = "\u{E923}";
    /// `ChromeClose`.
    pub const CLOSE: &str = "\u{E8BB}";
}

/// The icon families, in priority order.
///
/// Segoe Fluent Icons ships with Windows 11; Segoe MDL2 Assets is its Windows
/// 10 predecessor and carries the same codepoints for these four glyphs, so
/// naming it second makes the caption correct on both with no version check.
const ICON_FONT: [&str; 2] = ["Segoe Fluent Icons", "Segoe MDL2 Assets"];

/// Width and height of a caption button.
const CAPTION_BUTTON: f32 = 32.0;
/// Height of the caption strip.
///
/// Thirty-two logical pixels is what Windows itself uses. A taller strip is
/// what makes a custom title bar read as "an application that drew its own"
/// rather than as part of the system.
const CAPTION_HEIGHT: f32 = 32.0;
/// Size the caption glyphs are drawn at.
const CAPTION_GLYPH: f32 = 10.0;
/// How many caption buttons there are, and therefore how many hover springs.
const CAPTION_BUTTONS: usize = 3;

/// The colour Windows uses for a hovered close button.
const CLOSE_HOVER: Color = Color::hex(0xC4_2B1C);
/// The same, pressed.
const CLOSE_PRESSED: Color = Color::hex(0xB2_2719);

/// A window button that fades on hover.
///
/// A custom element rather than a [`button`] because it needs three things the
/// stock one does not offer: square corners, a background driven by an
/// animation rather than by the interaction state directly, and a hover signal
/// the frame loop can see.
///
/// The split is deliberate. `handle_event` records *intent* — the pointer is
/// over this button — and nothing else. The frame loop advances the spring.
/// `paint` reads whatever the spring currently says. That keeps paint a pure
/// function of state, which is the engine's rule, and it is the only way an
/// animation can outlive the event that started it.
struct CaptionButton {
    glyph: &'static str,
    command: WindowCommand,
    index: usize,
    state: Rc<State>,
    /// The spring's value this frame, sampled at build time.
    fade: f32,
}

impl CaptionButton {
    /// Builds one, already boxed. Returns an element rather than `Self` because
    /// nothing ever wants a bare `CaptionButton`.
    fn element(
        glyph: &'static str,
        command: WindowCommand,
        index: usize,
        state: &Rc<State>,
    ) -> AnyElement {
        let fade = state.wco_fade[index].get().value();
        CaptionButton { glyph, command, index, state: Rc::clone(state), fade }.into_element()
    }
}

impl Element for CaptionButton {
    fn id(&self) -> Option<spherekit::core::ElementId> {
        Some(spherekit::core::ElementId::from_key(("wco", self.index)))
    }

    fn layout_style(&self) -> spherekit::layout::Style {
        let mut style = spherekit::layout::Style::DEFAULT;
        style.size.width = spherekit::core::Length::Px(px(CAPTION_BUTTON));
        style.size.height = spherekit::core::Length::Px(px(CAPTION_BUTTON));
        style
    }

    fn paint(&mut self, cx: &mut spherekit::ui::PaintContext<'_, '_>) {
        let c = cx.theme.colors;
        cx.keep_interactive();
        let danger = self.command == WindowCommand::Close;

        let (hover, pressed) =
            if danger { (CLOSE_HOVER, CLOSE_PRESSED) } else { (c.hover, c.pressed) };
        let end = if cx.state.active { pressed } else { hover };
        // Faded from the target colour at zero alpha rather than from
        // `Color::TRANSPARENT`: interpolating out of fully transparent black
        // loses the hue and takes the fade through grey on its way to red.
        let background = Color::lerp(end.with_alpha(0.0), end, self.fade);

        // Square on purpose. A rounded caption button reads as a control
        // floating on the title bar; the shell's are flush to the edge and to
        // each other.
        cx.canvas.fill_rect(cx.bounds, background);
        // The glyph sits on the button, not on the window, so the coverage
        // correction is measured against whichever the fade has arrived at.
        let background = Color::lerp(c.surface, end, self.fade);

        let colour = if danger && self.fade > 0.5 { Color::WHITE } else { c.text };
        let style = spherekit_text_style(px(CAPTION_GLYPH));
        let layout = cx.text.layout(self.glyph, &style, None);
        let origin = spherekit::core::Point::new(
            cx.bounds.min_x() + (cx.bounds.width() - layout.size.width) * 0.5,
            cx.bounds.min_y() + (cx.bounds.height() - layout.size.height) * 0.5,
        );
        spherekit::ui::text::draw_layout(
            cx.canvas,
            &layout,
            origin,
            colour,
            spherekit::render::TextRasterMode::Auto,
            (Px::ZERO, Color::TRANSPARENT),
            spherekit::render::coverage_contrast_for(colour, background),
        );
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> spherekit::ui::EventFlow {
        use spherekit::ui::{EventFlow, UiEvent};
        cx.set_cursor(Cursor::Pointer);
        match cx.event {
            // Intent only. The spring is advanced by the frame loop, because an
            // animation has to keep running after the event that started it.
            UiEvent::MouseEnter(_) => {
                self.state.wco_hovered[self.index].set(true);
                cx.notify();
                EventFlow::Continue
            }
            UiEvent::MouseLeave(_) => {
                self.state.wco_hovered[self.index].set(false);
                cx.notify();
                EventFlow::Continue
            }
            UiEvent::MouseUp(e) if e.button == spherekit::ui::MouseButton::Primary => {
                if cx.bounds.contains(e.position) {
                    self.state.pending.set(Some(self.command));
                }
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::MouseDown(e) if e.button == spherekit::ui::MouseButton::Primary => {
                cx.focus();
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::Key(k)
                if k.state.is_pressed()
                    && matches!(k.key, spherekit::ui::Key::Enter | spherekit::ui::Key::Space) =>
            {
                self.state.pending.set(Some(self.command));
                EventFlow::Stop
            }
            _ => EventFlow::Continue,
        }
    }

    /// Not in the tab order, and drawn with no focus ring.
    ///
    /// The shell's own caption buttons are not tab stops either: minimise,
    /// maximise and close are reachable from the window menu, which Alt+Space
    /// and a right-click on the caption both open. A ring here would also be
    /// clipped by the 32-pixel strip and show as two stray vertical bars.
    fn focusable(&self) -> bool {
        false
    }

    fn semantics(&self) -> Option<Semantics> {
        let name = match self.command {
            WindowCommand::Minimize => "Minimise",
            WindowCommand::ToggleMaximize => "Maximise",
            WindowCommand::Close => "Close",
        };
        Some(Semantics::new(Role::Button, name))
    }
}

/// The text style the caption glyphs are drawn with.
fn spherekit_text_style(size: Px) -> spherekit::text::TextStyle {
    spherekit::text::TextStyle {
        font_size: size,
        font: spherekit::text::FontRequest {
            families: ICON_FONT.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        },
        wrap: spherekit::text::WrapMode::None,
        ..Default::default()
    }
}

fn page_header(title: &str, description: &str, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_col()
        .gap(theme.spacing.sm)
        .pb(theme.spacing.md)
        .child(
            label(title.to_string())
                .text_size(theme.typography.xl)
                .weight(FontWeight::BOLD)
                .text_color(c.text),
        )
        .child(
            label(description.to_string()).text_size(theme.typography.sm).text_color(c.text_muted),
        )
        .into_element()
}

fn progress_bar(fraction: f32, theme: &Theme) -> AnyElement {
    let t = fraction.clamp(0.0, 1.0);
    div()
        .h(px(4.0))
        .w(relative(1.0))
        .rounded_full()
        .bg(theme.colors.pressed)
        .clip()
        .child(div().h(relative(1.0)).w(relative(t)).rounded_full().bg(theme.colors.accent))
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
        .py_(theme.spacing.md)
        .child(
            div()
                .flex_col()
                .flex_1()
                .gap(theme.spacing.xs)
                // The row's title carries the weight; its description stays at
                // book weight and muted, which is the whole of the hierarchy.
                .child(
                    label(title.to_string())
                        .text_size(theme.typography.md)
                        .weight(theme.typography.strong)
                        .text_color(c.text),
                )
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
    svg: Option<spherekit::core::SvgId>,
    tint: Color,
    size: Px,
}

impl spherekit::ui::Element for IconElement {
    fn layout_style(&self) -> spherekit::layout::Style {
        spherekit::layout::Style {
            size: spherekit::core::Size {
                width: spherekit::core::Length::Px(self.size),
                height: spherekit::core::Length::Px(self.size),
            },
            ..spherekit::layout::Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut spherekit::ui::PaintContext<'_, '_>) {
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
        let attrs = WindowAttributes::new("SphereKit — Preferences")
            .with_inner_size(size(px(980.0), px(640.0)))
            .with_min_inner_size(size(px(560.0), px(380.0)))
            // The header is the title bar. The platform keeps the resize
            // borders, snap, the drop shadow and the window menu; only the
            // caption strip becomes ours to draw.
            .with_chrome(WindowChrome::Custom)
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
                println!("scale factor: {}", window.scale_factor().get());
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
            let presented = self.surface.as_ref().is_some_and(SphereKitSurface::has_presented);
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
        self.run_window_command(cx);

        match &event {
            WindowEvent::CloseRequested => {
                cx.exit();
                return;
            }
            WindowEvent::Resized(new_size) => {
                if let (Some(surface), Some(window)) = (self.surface.as_mut(), self.window.as_ref())
                {
                    let _ = surface.resize(*new_size, window.scale_factor());
                    // Snap, a double-click on the caption and Win+Up all
                    // maximise without asking, so the glyph is refreshed from
                    // the window rather than only from the button.
                    self.state.maximized.set(window.is_maximized());
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

        self.run_window_command(cx);
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
        let moving = self.advance_motion();
        let root = self.build();
        let clear = self.state.theme().colors.background;
        let Some(surface) = self.surface.as_mut() else { return };
        match surface.render(root, clear) {
            Ok(Some(_)) => self.frames += 1,
            Ok(None) => {}
            Err(e) => eprintln!("frame failed: {e}"),
        }
        self.apply_ime();
        self.publish_caption();
        // A spring that has not settled owes another frame. A settled one owes
        // nothing, which is what lets the window go back to a blocking wait
        // the moment the pointer stops moving.
        if moving && let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    /// Performs whatever the caption asked for, if anything.
    ///
    /// The queue exists because a widget callback has no window to act on. It
    /// is drained here, where one is in scope.
    fn run_window_command(&mut self, cx: &mut AppContext<'_>) {
        let Some(command) = self.state.pending.take() else { return };
        let Some(window) = self.window.as_ref() else { return };
        match command {
            WindowCommand::Minimize => window.set_minimized(true),
            WindowCommand::ToggleMaximize => {
                let now = !window.is_maximized();
                window.set_maximized(now);
                self.state.maximized.set(now);
            }
            WindowCommand::Close => cx.exit(),
        }
    }

    /// Tells the platform which part of the client area behaves as a title bar.
    ///
    /// Republished after every paint, because the caption's contents are laid
    /// out by flexbox and move with the window's width. It is a cheap store —
    /// the platform reads the newest value on its next hit test — and
    /// republishing unconditionally removes a whole class of bug where a caption
    /// drags in the wrong place after a resize nobody remembered to hook.
    ///
    /// The exclusions are **not** listed by hand. Every interactive widget
    /// declares itself during paint through `PaintContext::keep_interactive`,
    /// and the tree collects them, so a button added to the header later is
    /// clickable without anyone remembering this function exists. Hand-listing
    /// them is how header actions can be swallowed by the modal move loop, with
    /// no error and no way to notice but trying to click them.
    fn publish_caption(&self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_ref()) else {
            return;
        };
        if window.chrome() != WindowChrome::Custom {
            return;
        }
        let strip = spherekit::core::Rect::new(
            spherekit::core::Point::new(Px::ZERO, Px::ZERO),
            size(surface.viewport().width, px(CAPTION_HEIGHT)),
        );
        let regions = CaptionRegions {
            drag: vec![strip],
            // Filtered to the strip so the platform's hit test, which runs on
            // every mouse move, walks three rectangles rather than thirty.
            exclude: surface
                .tree()
                .caption_exclusions()
                .iter()
                .copied()
                .filter(|r| r.intersects(strip))
                .collect(),
        };
        window.set_caption_regions(&regions);
    }

    /// Advances every running animation by one frame.
    ///
    /// The only place a spring is stepped. Events set targets and paint reads
    /// values; this is what sits between them, and it is what lets an animation
    /// outlive the event that started it.
    ///
    /// `Timeline` owns the clock so that one delta drives everything and a
    /// stall — a debugger pause, a minimised window — is clamped once rather
    /// than at every call site.
    fn advance_motion(&mut self) -> bool {
        let frame = self.timeline.advance_to(self.clock.now());
        let mut moving = false;
        for i in 0..CAPTION_BUTTONS {
            let mut motion = self.state.wco_fade[i].get();
            motion.retarget(if self.state.wco_hovered[i].get() { 1.0 } else { 0.0 });
            motion.step(frame.delta);
            moving |= !motion.is_settled();
            self.state.wco_fade[i].set(motion);
        }
        moving
    }

    /// Tells the window whether to compose, and where.
    ///
    /// Read after rendering, because the caret's position is a paint-time fact:
    /// a text field can only say where its caret is once it has laid its string
    /// out, and it lays it out while painting.
    ///
    /// Both halves matter. Without `set_ime_allowed` nothing composes at all;
    /// without `set_ime_cursor_area` a CJK candidate window opens in a corner of
    /// the screen rather than under the caret, which makes the feature useless
    /// for the languages that need it. The state is diffed rather than pushed
    /// every frame, because a platform is entitled to treat re-enabling an input
    /// method as a reason to cancel the composition in progress.
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
        if let Some(area) = area
            && self.ime_caret != Some(area.caret)
        {
            window.set_ime_cursor_area(area.caret);
            self.ime_caret = Some(area.caret);
        }
        if area.is_none() {
            self.ime_caret = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit::core::ScaleFactor;
    use spherekit::render::{Canvas, Scene};
    use spherekit::text::{FontRequest, TextSystem};
    use spherekit::ui::UiTree;

    #[test]
    fn product_theme_is_dark_but_uses_lifted_surfaces() {
        let theme = spherekit_dark_theme();
        let c = theme.colors;

        assert!(theme.is_dark());
        assert!(c.background.luminance() > Color::hex(0x181A1F).luminance());
        assert!(c.surface.luminance() > c.background.luminance());
        assert!(c.elevated.luminance() > c.surface.luminance());
        assert!((c.text.luminance() - c.background.luminance()).abs() > 0.6);
    }

    #[test]
    fn desktop_typography_paints_regular_semibold_and_bold_faces() {
        let mut text = TextSystem::with_system_fonts();
        let regular = text.fonts_mut().resolve(&FontRequest::default().weight(FontWeight::NORMAL));
        let semibold =
            text.fonts_mut().resolve(&FontRequest::default().weight(FontWeight::SEMI_BOLD));
        let bold = text.fonts_mut().resolve(&FontRequest::default().weight(FontWeight::BOLD));
        let (Some(regular), Some(semibold), Some(bold)) = (regular, semibold, bold) else {
            eprintln!("system has no complete UI weight family; skipping");
            return;
        };
        if regular == semibold || regular == bold || semibold == bold {
            eprintln!("system UI family aliases weight faces; skipping");
            return;
        }

        let mut app = DesktopApp::new();
        let viewport = size(px(980.0), px(640.0));
        let mut tree = UiTree::new();
        tree.set_theme(app.state.theme());
        tree.build(app.build());
        tree.compute_layout_with_text(viewport, &mut text).unwrap();

        let mut scene = Scene::new(viewport, ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut text, viewport, 0.0);
        }

        for (name, face) in [("regular", regular), ("semibold", semibold), ("bold", bold)] {
            assert!(
                scene.runs.iter().any(|run| run.font == face),
                "desktop app never painted its {name} face"
            );
        }
    }
}

fn main() {
    if let Err(e) = App::new(DesktopApp::new()).run() {
        eprintln!("event loop failed: {e}");
        std::process::exit(1);
    }
}
