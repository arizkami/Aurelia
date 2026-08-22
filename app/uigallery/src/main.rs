//! # SphereKit UI Gallery
//!
//! Every built-in widget, live and interactive, in one window.
//!
//! This is a real application rather than a catalogue: the shell is a custom
//! Windows frame over DWM Mica, the pages are a scrolling pane, and every
//! control on every page owns nothing — it takes a value and reports changes,
//! exactly as an application's own controls would. Read [`pages`] to see what
//! each widget is for; read this file to see the shell it all hangs in.
//!
//! It is also where the rendering paths that are *not* analytically
//! antialiased get exercised — the sidebar icons are SVG, tessellated into
//! triangles, and they are smooth because the surface is multisampled.
//!
//! ```text
//! cargo run -p uigallery --release
//! ```
//!
//! Keyboard: Tab and Shift-Tab move focus, Space and Enter activate, arrow keys
//! adjust a focused slider or knob, Escape closes a menu and then quits.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pages;

use pages::Page;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use spherekit::core::animate::{Drive, Motion};
use spherekit::core::time::{Clock, SystemClock, Timeline};
use spherekit::core::{Color, Px, px, relative, size};
use spherekit::platform::{
    App, AppContext, AppHandler, RedrawPolicy, Theme as PlatformTheme, WindowAttributes,
    WindowEvent, WindowId,
};
use spherekit::platform::{CaptionRegions, WindowBackdrop, WindowChrome};
use spherekit::svg::SvgCache;
use spherekit::text::FontWeight;
use spherekit::ui::{
    AnyElement, Cursor, Element, EventContext, InputTranslator, Interactive, IntoElement,
    ParentElement, Presence, Role, Semantics, Styled, StyledInteraction, TextEdit, Theme, avatar,
    context_menu, div, dropdown, label, menu_item, scroll_view, separator,
};
use spherekit::{SphereKitSurface, SurfaceOptions};

// ---------------------------------------------------------------------------
// Composition diagnostics (env-driven; defaults reproduce the shipping app).
//
//   SPHEREKIT_DIAG_TRANSPARENT = 0 | 1        window + surface transparency
//   SPHEREKIT_DIAG_BACKDROP    = none | mica | acrylic
//   SPHEREKIT_DIAG_CLEAR       = transparent | opaque
//
// These exist so the opaque/no-Mica baseline and the DX12-vs-Vulkan comparison
// run the *same* scene; nothing here changes any theme colour.
// ---------------------------------------------------------------------------
fn diag_flag(name: &str, default: bool) -> bool {
    match std::env::var(name).ok().as_deref() {
        Some("0") | Some("false") => false,
        Some("1") | Some("true") => true,
        _ => default,
    }
}

fn diag_backdrop() -> Option<WindowBackdrop> {
    match std::env::var("SPHEREKIT_DIAG_BACKDROP").ok().as_deref() {
        Some("none") => Some(WindowBackdrop::None),
        Some("acrylic") => Some(WindowBackdrop::Acrylic),
        Some("off") => None,
        _ => Some(WindowBackdrop::Mica),
    }
}

fn diag_clear() -> Color {
    if std::env::var("SPHEREKIT_DIAG_CLEAR").ok().as_deref() == Some("opaque") {
        // Opaque baseline only: an alpha-1 clear so the surface can never be
        // composited against anything behind the window.
        Color::hex(0x1F2023)
    } else {
        Color::TRANSPARENT
    }
}

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
    c.mica_surface = Color::hex(0x2B2C2F).with_alpha(0.72);
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

/// Maps the operating system appearance to the product theme.
///
/// The renderer does not need to know about platform theme APIs; an app owns
/// this small policy and hands the resulting semantic tokens to the UI tree.
fn product_theme(system_theme: PlatformTheme) -> Theme {
    match system_theme {
        PlatformTheme::Dark => spherekit_dark_theme(),
        PlatformTheme::Light => {
            let mut theme = Theme::light();
            theme.colors.mica_surface = Color::hex(0xF8F9FB).with_alpha(0.72);
            theme.typography.xs = px(10.0);
            theme.typography.sm = px(12.0);
            theme.typography.md = px(14.0);
            theme.typography.lg = px(17.0);
            theme.typography.xl = px(22.0);
            theme.radii.sm = px(4.0);
            theme.radii.md = px(6.0);
            theme.radii.lg = px(8.0);
            theme
        }
    }
}

/// What a context-menu row does to the field it was opened over.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditAction {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// Everything the UI reads, and everything a handler may write.
///
/// One shared struct of `Cell`s rather than a mutable borrow: the element tree
/// is rebuilt every frame, so a handler installed during one build has to be
/// able to change what the *next* build reads. This is the pattern the engine
/// intends — explicit, and impossible to panic on.
pub(crate) struct State {
    system_theme: Cell<PlatformTheme>,
    /// Which gallery page the sidebar has selected.
    pub(crate) page: Cell<Page>,

    // --- Buttons -----------------------------------------------------------
    /// How many times any button on the Buttons page has been pressed.
    pub(crate) presses: Cell<u32>,

    // --- Selection ---------------------------------------------------------
    pub(crate) wifi: Cell<bool>,
    pub(crate) opt_a: Cell<bool>,
    pub(crate) opt_b: Cell<bool>,
    pub(crate) opt_c: Cell<bool>,

    // --- Values ------------------------------------------------------------
    pub(crate) gain: Cell<f32>,
    pub(crate) scale: Cell<f32>,
    pub(crate) pan: Cell<f32>,
    pub(crate) level: Cell<f32>,
    pub(crate) tone: Cell<f32>,
    pub(crate) width: Cell<f32>,

    // --- Text --------------------------------------------------------------
    pub(crate) name_field: RefCell<TextEdit>,
    pub(crate) secret_field: RefCell<TextEdit>,

    // --- Identity ----------------------------------------------------------
    /// The Identity page's own dropdown, separate from the sidebar footer's so
    /// the two can be open at once and prove they do not share state.
    pub(crate) demo_menu_open: Cell<bool>,
    pub(crate) demo_menu: Cell<Motion<f32>>,

    // --- Containers --------------------------------------------------------
    pub(crate) download: Cell<f32>,
    /// Whether the Containers page is running its indeterminate bar.
    pub(crate) busy: Cell<bool>,

    // --- Context menu ------------------------------------------------------
    /// Where the last right-click landed, in window coordinates.
    pub(crate) menu_at: Cell<spherekit::core::Point<Px>>,
    /// Which field the menu is acting on: 0 none, 1 name, 2 secret.
    pub(crate) menu_field: Cell<u8>,
    pub(crate) ctx_menu_open: Cell<bool>,
    pub(crate) ctx_menu: Cell<Motion<f32>>,
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
    /// Whether the account menu in the sidebar footer is open.
    ///
    /// The flag and the animation are separate on purpose: this is the truth an
    /// event handler writes, and [`State::user_menu`] is where it has got to.
    user_menu_open: Cell<bool>,
    /// How far open the account menu has actually travelled, `0..=1`.
    ///
    /// A spring for the same reason the caption fades are: clicking the row
    /// twice in quick succession retargets a panel that is still moving, and it
    /// reverses from where it is rather than snapping back to the start.
    user_menu: Cell<Motion<f32>>,
    /// Whether the press being dispatched landed on the account row or its menu.
    ///
    /// Set while the event bubbles through the footer and read by the runner
    /// once dispatch is over. That is how "click anywhere else to dismiss" is
    /// answered without the runner knowing where the panel ended up.
    press_inside_account: Cell<bool>,
    /// A window command the caption asked for, drained by the runner.
    ///
    /// Queued rather than executed inline because a widget callback has no
    /// window: the element tree is deliberately free of platform types, so the
    /// request travels as data and the runner performs it.
    pending: Cell<Option<WindowCommand>>,
    log: RefCell<Vec<String>>,
}

impl State {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            system_theme: Cell::new(PlatformTheme::Dark),
            page: Cell::new(Page::Buttons),
            presses: Cell::new(0),
            wifi: Cell::new(true),
            opt_a: Cell::new(true),
            opt_b: Cell::new(false),
            opt_c: Cell::new(false),
            gain: Cell::new(-6.0),
            scale: Cell::new(100.0),
            pan: Cell::new(0.0),
            level: Cell::new(72.0),
            tone: Cell::new(45.0),
            width: Cell::new(0.0),
            name_field: RefCell::new(TextEdit::from_text("Ada Lovelace")),
            secret_field: RefCell::new(TextEdit::new()),
            demo_menu_open: Cell::new(false),
            demo_menu: Cell::new(Motion::at(0.0, Drive::STIFF)),
            download: Cell::new(0.0),
            busy: Cell::new(true),
            menu_at: Cell::new(spherekit::core::Point::new(Px::ZERO, Px::ZERO)),
            menu_field: Cell::new(0),
            ctx_menu_open: Cell::new(false),
            ctx_menu: Cell::new(Motion::at(0.0, Drive::STIFF)),
            maximized: Cell::new(false),
            wco_hovered: [const { Cell::new(false) }; CAPTION_BUTTONS],
            wco_fade: core::array::from_fn(|_| Cell::new(Motion::at(0.0, Drive::SMOOTH))),
            user_menu_open: Cell::new(false),
            user_menu: Cell::new(Motion::at(0.0, Drive::STIFF)),
            press_inside_account: Cell::new(false),
            pending: Cell::new(None),
            log: RefCell::new(vec!["Ready.".into()]),
        })
    }

    fn theme(&self) -> Theme {
        product_theme(self.system_theme.get())
    }

    /// Opens the edit menu over `field` at a window position.
    pub(crate) fn open_edit_menu(&self, field: u8, at: spherekit::core::Point<Px>) {
        self.menu_field.set(field);
        self.menu_at.set(at);
        self.ctx_menu_open.set(true);
    }

    /// Runs one clipboard action against whichever field the menu is about.
    ///
    /// The clipboard comes from the tree rather than being created here, so an
    /// embedder that replaced the system clipboard gets the same one the
    /// shortcuts use.
    pub(crate) fn apply_edit(&self, action: EditAction) {
        let clipboard = spherekit::platform::Clipboard::system();
        let which = self.menu_field.get();
        let masked = which == 2;
        let mut field = match which {
            1 => self.name_field.borrow_mut(),
            2 => self.secret_field.borrow_mut(),
            _ => return,
        };
        let name = match action {
            EditAction::Cut => {
                field.cut_to(&clipboard, masked);
                "Cut"
            }
            EditAction::Copy => {
                field.copy_to(&clipboard, masked);
                "Copy"
            }
            EditAction::Paste => {
                field.paste_from(&clipboard);
                "Paste"
            }
            EditAction::SelectAll => {
                field.select_all();
                "Select all"
            }
        };
        drop(field);
        self.say(format!("{name}."));
    }

    /// Opens or closes the account menu. Returns whether anything changed.
    fn set_user_menu(&self, open: bool) -> bool {
        if self.user_menu_open.get() == open {
            return false;
        }
        self.user_menu_open.set(open);
        true
    }

    pub(crate) fn say(&self, message: impl Into<String>) {
        let mut log = self.log.borrow_mut();
        log.push(message.into());
        // The log is a UI element, not a record; letting it grow forever would
        // be an unbounded allocation in a long-running window.
        if log.len() > 32 {
            log.remove(0);
        }
    }
}

struct GalleryApp {
    window: Option<Arc<spherekit::platform::backend::Window>>,
    surface: Option<SphereKitSurface>,
    input: InputTranslator,
    started: Instant,
    state: Rc<State>,
    icons: Vec<(Page, spherekit::core::SvgId)>,
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

impl GalleryApp {
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

    fn icon_for(&self, section: Page) -> Option<spherekit::core::SvgId> {
        self.icons.iter().find(|(s, _)| *s == section).map(|(_, id)| *id)
    }

    fn build(&mut self) -> AnyElement {
        let theme = self.state.theme();

        div()
            .flex_col()
            .full()
            .child(self.header(&theme))
            .child(
                div()
                    .flex_row()
                    .flex_1()
                    // Without this the row grows to its content instead of to
                    // the window: a flex item cannot shrink below its automatic
                    // minimum size, and that minimum is the whole page. The
                    // pane inside would then never overflow, so nothing would
                    // ever scroll — it would simply run off the bottom.
                    .min_h(px(0.0))
                    .z(1)
                    .child(self.sidebar(&theme))
                    .child(separator(true).bg(Color::TRANSPARENT))
                    .child(self.content(&theme)),
            )
            .child(separator(false).bg(Color::TRANSPARENT))
            .child(self.status_bar(&theme))
            // Last child of the root, so its coordinates are window
            // coordinates and it paints over everything. A context menu that
            // lived inside the pane it was opened from could not escape it.
            .child(self.edit_menu(&theme))
            .into_element()
    }

    /// The cut/copy/paste menu, opened by a right-click in a text field.
    ///
    /// Its items act on the application's own [`TextEdit`] buffers, not on the
    /// field element — the element is rebuilt every frame and owns nothing. The
    /// same `TextEdit` methods back the keyboard shortcuts, so the two cannot
    /// disagree about what Copy means.
    fn edit_menu(&self, theme: &Theme) -> AnyElement {
        let open = self.state.ctx_menu.get().value();
        let which = self.state.menu_field.get();
        let at = self.state.menu_at.get();

        // Read the buffer the menu is about, so the rows can be honest about
        // what is available before they are clicked.
        let (has_selection, masked) = match which {
            1 => (self.state.name_field.borrow().has_selection(), false),
            2 => (self.state.secret_field.borrow().has_selection(), true),
            _ => (false, false),
        };
        let can_copy = has_selection && !masked;

        let act = |state: &Rc<State>, action: EditAction| {
            let state = Rc::clone(state);
            move || {
                state.apply_edit(action);
                state.ctx_menu_open.set(false);
            }
        };

        context_menu(open, at)
            .id("ctx.menu")
            .w(px(216.0))
            .p(theme.spacing.xs)
            .gap(px(1.0))
            .child(
                menu_item("Cut")
                    .id("ctx.cut")
                    .shortcut("Ctrl+X")
                    .disabled(!can_copy)
                    .on_select(act(&self.state, EditAction::Cut)),
            )
            .child(
                menu_item("Copy")
                    .id("ctx.copy")
                    .shortcut("Ctrl+C")
                    .disabled(!can_copy)
                    .on_select(act(&self.state, EditAction::Copy)),
            )
            .child(
                menu_item("Paste")
                    .id("ctx.paste")
                    .shortcut("Ctrl+V")
                    .on_select(act(&self.state, EditAction::Paste)),
            )
            .child(separator(false).bg(theme.colors.border).m(theme.spacing.xs))
            .child(
                menu_item("Select all")
                    .id("ctx.all")
                    .shortcut("Ctrl+A")
                    .on_select(act(&self.state, EditAction::SelectAll)),
            )
            .into_element()
    }

    fn header(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let state = Rc::clone(&self.state);

        div()
            .id("chrome.header")
            .flex_row()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(CAPTION_HEIGHT))
            // Fixed-height chrome, so it refuses to shrink. A flex item with a
            // height and the default `flex_shrink: 1` is squashed whenever the
            // column runs short, and the caption quietly loses pixels on a
            // long page — the one place it must never move.
            .shrink(0.0)
            // Padded on the left only: the window buttons run flush to the
            // right edge, exactly as the shell's do.
            .pl(px(12.0))
            .z(2)
            .child(
                label("SphereKit")
                    .text_size(theme.typography.sm)
                    .weight(theme.typography.strong)
                    .text_color(c.text)
                    .no_wrap(),
            )
            .child(label("/").text_size(theme.typography.sm).text_color(c.text_muted).no_wrap())
            .child(
                label("UI Gallery")
                    .text_size(theme.typography.sm)
                    .text_color(c.text_muted)
                    .no_wrap(),
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
        let current = self.state.page.get();
        let mut nav = div()
            .flex_col()
            // Clipped, and scrollable once it no longer fits. Without this the
            // rows overflow a short sidebar and are drawn straight over the
            // account footer — the container shrinks, its fixed-height
            // children do not, and nothing was cutting the difference off.
            .overflow_y_scroll()
            // Grows instead of filling: the footer below claims its own height
            // first, and whatever is left over is the navigation's.
            .grow(1.0)
            .min_h(px(0.0))
            .px_(theme.spacing.md)
            .pt(theme.spacing.md)
            .gap(theme.spacing.xs)
            .child(
                label("WIDGETS")
                    .text_size(theme.typography.xs)
                    .weight(theme.typography.strong)
                    .text_color(c.text_muted)
                    .px_(theme.spacing.md)
                    .py_(theme.spacing.sm),
            );

        for section in Page::ALL {
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
                    .hover_bg(c.hover)
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
                        state.page.set(section);
                        // A section switch changes what is in the pane, so this
                        // one genuinely is a layout change — unlike a hover or
                        // a slider drag, which are repaints.
                        cx.notify_layout();
                    }),
            );
        }
        // Keep the backdrop effect isolated from the navigation content. A
        // filtered parent composites an off-screen layer; putting the labels
        // in that same layer can make them disappear when the adjacent opaque
        // content pane is repainted.
        div()
            .flex_col()
            .w(px(220.0))
            .shrink(0.0)
            .h(relative(1.0))
            .child(div().absolute().inset(px(0.0)).backdrop_blur(px(18.0)))
            .child(nav)
            // Last, so it paints over the navigation: the account menu opens
            // upward across it.
            .child(self.account_footer(theme))
            .into_element()
    }

    /// The signed-in account, pinned to the bottom of the sidebar.
    ///
    /// The row and the menu share one container, and that container is what the
    /// [`dropdown`] anchors against — every node is a containing block here, so
    /// the panel lands on the row's top edge without anyone measuring the row.
    fn account_footer(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let open = self.state.user_menu.get().value();
        let expanded = self.state.user_menu_open.get();
        let state = Rc::clone(&self.state);

        let menu = dropdown(open)
            .above()
            .offset(theme.spacing.sm)
            .p(theme.spacing.xs)
            .gap(theme.spacing.xs)
            .child(self.account_menu_item(theme, "acct.profile", "Profile", false))
            .child(self.account_menu_item(theme, "acct.keys", "Account keys", false))
            .child(separator(false).bg(c.border).m(theme.spacing.xs))
            .child(self.account_menu_item(theme, "acct.signout", "Sign out", true));

        let row = div()
            .id("acct.row")
            .focusable()
            .flex_row()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(44.0))
            .px_(theme.spacing.sm)
            .rounded(theme.radii.md)
            .bg(if expanded { c.pressed } else { Color::TRANSPARENT })
            .hover_bg(c.hover)
            .active_bg(c.pressed)
            .cursor(Cursor::Pointer)
            .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
            .semantics(Semantics::new(Role::Button, "Account menu"))
            .child(
                avatar(USER_NAME)
                    .size(px(28.0))
                    .presence(Presence::Online)
                    // The dot is cut out of what is actually behind it. The
                    // sidebar is translucent over Mica, so the theme's surface
                    // would read as a lighter blob than the row it sits on.
                    .ring(c.background),
            )
            .child(
                div()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        label(USER_NAME)
                            .text_size(theme.typography.sm)
                            .weight(theme.typography.strong)
                            .text_color(c.text)
                            .no_wrap(),
                    )
                    .child(
                        label(USER_EMAIL)
                            .text_size(theme.typography.xs)
                            .text_color(c.text_muted)
                            .no_wrap(),
                    ),
            )
            .child(ChevronElement { tint: c.text_muted, open })
            .on_click(move |cx: &mut EventContext<'_>| {
                state.set_user_menu(!state.user_menu_open.get());
                // A repaint, not a relayout: the panel is already in the tree
                // and only its open amount changes. The spring in
                // `advance_motion` drives every frame after this one.
                cx.notify();
            })
            // `on_click` is a mouse contract — it only ever fires on MouseUp —
            // so a row that calls itself a button has to answer the keyboard
            // itself, or Tab would reach it and nothing would happen.
            .on_key(keyboard_activate(Rc::clone(&self.state), |state| {
                state.set_user_menu(!state.user_menu_open.get());
            }));

        let claim = Rc::clone(&self.state);
        div()
            .flex_col()
            .shrink(0.0)
            .px_(theme.spacing.sm)
            .pb(theme.spacing.sm)
            .pt(theme.spacing.xs)
            // Presses bubble out through here, so this is the one place that
            // knows a press landed on the account UI — whichever part of it.
            // `on_mouse_down` does not consume, so the row and the menu rows
            // still get the event.
            .on_mouse_down(move |_| claim.press_inside_account.set(true))
            .child(separator(false).bg(c.border))
            .child(div().h(theme.spacing.xs))
            // Row first, panel second. The panel is absolutely positioned, so
            // order costs it nothing in layout, but it buys two things: Tab
            // walks from the row *into* the menu it just opened, and the
            // popover paints last, over anything it ever overlaps.
            .child(div().flex_col().child(row).child(menu))
            .into_element()
    }

    /// One row inside the account menu.
    fn account_menu_item(
        &self,
        theme: &Theme,
        key: &'static str,
        text: &'static str,
        danger: bool,
    ) -> AnyElement {
        let c = theme.colors;
        let state = Rc::clone(&self.state);
        div()
            .id(key)
            .focusable()
            .flex_row()
            .items_center()
            .h(px(30.0))
            .px_(theme.spacing.sm)
            .rounded(theme.radii.sm)
            .hover_bg(if danger { c.danger.with_alpha(0.16) } else { c.hover })
            .active_bg(if danger { c.danger.with_alpha(0.24) } else { c.pressed })
            .cursor(Cursor::Pointer)
            .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
            .semantics(Semantics::new(Role::MenuItem, text))
            .child(
                label(text)
                    .text_size(theme.typography.sm)
                    .text_color(if danger { c.danger } else { c.text })
                    .no_wrap(),
            )
            .on_click(move |cx: &mut EventContext<'_>| {
                state.set_user_menu(false);
                state.say(format!("{text} selected."));
                cx.notify();
            })
            .on_key(keyboard_activate(Rc::clone(&self.state), move |state| {
                state.set_user_menu(false);
                state.say(format!("{text} selected."));
            }))
            .into_element()
    }

    fn content(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        // The renderer's own numbers are on the Containers page, so they have to
        // be sampled here where the surface is reachable and handed down.
        let adapter = self.surface.as_ref().map(|s| s.adapter_name()).unwrap_or("").to_string();
        let stats = self.surface.as_ref().map(|s| s.stats()).unwrap_or_default();
        let body = pages::render(self.state.page.get(), &self.state, theme, &adapter, &stats);

        div()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h(relative(1.0))
            // Keep the app pane translucent so the native DWM Mica material
            // remains visible in the gaps and margins around the controls.
            .bg(c.mica_surface)
            .child(
                div()
                    .flex_row()
                    .items_center()
                    .h(px(36.0))
                    .shrink(0.0)
                    .px_(px(12.0))
                    // A `no_wrap` label's minimum width is its whole string, so
                    // two of them in one row simply refuse to yield and get
                    // drawn over each other. The title takes the leftover space
                    // and clips inside it; the hint keeps its size and wins,
                    // which is the right way round — a truncated page title is
                    // still readable, a truncated sentence is not.
                    .child(
                        div().flex_1().min_w(px(0.0)).overflow_hidden().child(
                            label(self.state.page.get().title())
                                .text_size(theme.typography.sm)
                                .weight(theme.typography.strong)
                                .text_color(c.text)
                                .no_wrap(),
                        ),
                    )
                    .child(div().w(theme.spacing.md).shrink(0.0))
                    .child(
                        label("Every control here is live")
                            .shrink(0.0)
                            .text_size(theme.typography.xs)
                            .text_color(c.text_muted)
                            .no_wrap(),
                    ),
            )
            .child(separator(false).bg(Color::TRANSPARENT))
            .child(
                scroll_view().id("content-scroll").flex_1().min_h(px(0.0)).w(relative(1.0)).child(
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

    fn status_bar(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let last = self.state.log.borrow().last().cloned().unwrap_or_default();
        div()
            .id("chrome.status")
            .flex_row()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(26.0))
            // Same reason as the caption: 26 px means 26 px, whatever the page
            // above it is doing.
            .shrink(0.0)
            .px_(theme.spacing.lg)
            .z(1)
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
pub(crate) const ICON_FONT: [&str; 2] = ["Segoe Fluent Icons", "Segoe MDL2 Assets"];

/// The signed-in account the sidebar footer shows.
///
/// A constant because this example has no account system. A real application
/// would hand the same two strings to the same two widgets.
pub(crate) const USER_NAME: &str = "Ada Lovelace";
/// The account's address, shown under the name.
pub(crate) const USER_EMAIL: &str = "ada@futureboard.local";

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

/// Draws a cached SVG icon, tinted.
///
/// A minimal custom element: it has no children, no layout of its own beyond a
/// fixed size, and its whole job is one `SvgCache::render` call. Writing one is
/// meant to be this small.
pub(crate) struct IconElement {
    pub(crate) svg: Option<spherekit::core::SvgId>,
    pub(crate) tint: Color,
    pub(crate) size: Px,
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

/// Turns Space and Enter on a focused row into the same action a click does.
///
/// `on_click` fires only on `MouseUp`, so anything that is `focusable()` and
/// styled as a control needs this as well or the keyboard reaches it and can do
/// nothing with it. Returned as a handler so the two call sites cannot drift.
fn keyboard_activate(
    state: Rc<State>,
    mut action: impl FnMut(&State) + 'static,
) -> impl FnMut(&mut EventContext<'_>) -> spherekit::ui::EventFlow + 'static {
    use spherekit::ui::{EventFlow, Key, UiEvent};
    move |cx: &mut EventContext<'_>| {
        let UiEvent::Key(key) = cx.event else { return EventFlow::Continue };
        if key.state.is_pressed() && matches!(key.key, Key::Space | Key::Enter) {
            action(&state);
            cx.notify();
            return EventFlow::Stop;
        }
        EventFlow::Continue
    }
}

/// The chevron on the account row, drawn rather than shaped.
///
/// Two strokes instead of a glyph so it can *rotate* with the menu: it points
/// up when the panel is open and down when it is shut, and every frame in
/// between is a real angle rather than a swap between two characters.
pub(crate) struct ChevronElement {
    pub(crate) tint: Color,
    /// How far open the menu is, `0..=1`.
    pub(crate) open: f32,
}

impl spherekit::ui::Element for ChevronElement {
    fn layout_style(&self) -> spherekit::layout::Style {
        spherekit::layout::Style {
            size: spherekit::core::Size {
                width: spherekit::core::Length::Px(px(12.0)),
                height: spherekit::core::Length::Px(px(12.0)),
            },
            flex_shrink: 0.0,
            ..spherekit::layout::Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut spherekit::ui::PaintContext<'_, '_>) {
        let b = cx.bounds;
        if b.is_empty() {
            return;
        }
        // `1.0` points down, `-1.0` points up; the spring supplies everything
        // between, so the arrow sweeps through flat instead of flipping.
        let dir = 1.0 - 2.0 * self.open.clamp(0.0, 1.0);
        let c = b.center();
        let half = b.width() * 0.28;
        let rise = b.height() * 0.18 * dir;
        let left = spherekit::core::Point::new(c.x - half, c.y - rise);
        let tip = spherekit::core::Point::new(c.x, c.y + rise);
        let right = spherekit::core::Point::new(c.x + half, c.y - rise);
        cx.canvas.draw_line(left, tip, self.tint, px(1.5));
        cx.canvas.draw_line(tip, right, self.tint, px(1.5));
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

impl AppHandler for GalleryApp {
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
        let attrs = WindowAttributes::new("SphereKit — UI Gallery")
            .with_inner_size(size(px(980.0), px(640.0)))
            .with_min_inner_size(size(px(560.0), px(380.0)))
            // The header is the title bar. The platform keeps the resize
            // borders, snap, the drop shadow and the window menu; only the
            // caption strip becomes ours to draw.
            .with_chrome(WindowChrome::Custom)
            .with_transparent(diag_flag("SPHEREKIT_DIAG_TRANSPARENT", true))
            .with_visible(false);
        let window = match cx.create_window(&attrs) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("failed to create a window: {e}");
                cx.exit();
                return;
            }
        };
        self.state.system_theme.set(window.theme().unwrap_or(PlatformTheme::Dark));

        match pollster::block_on(SphereKitSurface::new(
            Arc::clone(&window),
            window.physical_size(),
            window.scale_factor(),
            SurfaceOptions {
                transparent: diag_flag("SPHEREKIT_DIAG_TRANSPARENT", true),
                ..SurfaceOptions::default()
            },
        )) {
            Ok(s) => {
                let t = s.init_timing();
                println!("adapter: {}", s.adapter_name());
                println!("scale factor: {}", window.scale_factor().get());
                println!("init: gpu {:.0} ms, fonts {:.0} ms", t.gpu_ms, t.fonts_ms);
                println!("chrome: {:?}", window.chrome());
                self.surface = Some(s);
                // Apply Mica after the DX12 DirectComposition surface exists:
                // creating that visual can otherwise replace the composition
                // state that was attached to the HWND before GPU setup.
                match diag_backdrop() {
                    Some(backdrop) => {
                        eprintln!("diag: requesting backdrop {backdrop:?}");
                        if let Err(error) = window.set_backdrop(backdrop) {
                            eprintln!(
                                "system backdrop unavailable; using transparent fallback: {error}"
                            );
                        }
                    }
                    None => eprintln!("diag: skipping set_backdrop entirely"),
                }
                if let Some(surface) = self.surface.as_mut() {
                    surface.set_theme(self.state.theme());
                }
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
                for section in Page::ALL {
                    let _ = cache.load_str(section.icon());
                }
            }
        });
        // Re-resolve the ids against the cache the painter will actually use.
        self.icons = ICONS.with(|cache| {
            let mut cache = cache.borrow_mut();
            Page::ALL
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
            WindowEvent::ThemeChanged(theme) => {
                self.state.system_theme.set(*theme);
                if let Some(surface) = self.surface.as_mut() {
                    surface.set_theme(self.state.theme());
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
            // A press anywhere dismisses the account menu unless it landed on
            // the account UI itself. The footer sets the flag while the press
            // bubbles through it, so this reads the answer immediately after
            // dispatch rather than trying to hit-test the panel from out here.
            let press = matches!(&ui_event, spherekit::ui::UiEvent::MouseDown(e)
                if e.button == spherekit::ui::MouseButton::Primary);
            if press {
                self.state.press_inside_account.set(false);
            }

            let result = match self.surface.as_mut() {
                Some(surface) => surface.dispatch(&ui_event),
                None => continue,
            };
            needs_redraw |= result.repaint || result.relayout || result.focus_changed;

            // Deliberately outside the `consumed` guard below: a click on a
            // toggle in the content pane is consumed, and it should still shut
            // the menu. Only a press on the account UI is exempt.
            if press {
                if !self.state.press_inside_account.get() && self.state.set_user_menu(false) {
                    needs_redraw = true;
                }
                // The edit menu closes on any press its own rows did not
                // consume — including the right-click that opens it over a
                // different field, which reopens it a moment later at the new
                // position. Checked after dispatch so a row still gets its
                // click before the menu goes.
                if !result.consumed && self.state.ctx_menu_open.replace(false) {
                    needs_redraw = true;
                }
            }

            if result.consumed {
                continue;
            }
            if let spherekit::ui::UiEvent::Key(key) = &ui_event
                && key.state.is_pressed()
            {
                match &key.key {
                    // Escape dismisses the account menu before it quits: an
                    // open popover is what the key most recently opened, and
                    // closing the window out from under it would be a surprise.
                    spherekit::ui::Key::Escape => {
                        if self.state.set_user_menu(false) {
                            needs_redraw = true;
                        } else {
                            cx.exit();
                        }
                    }
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

impl GalleryApp {
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
        let clear = diag_clear();
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

        // The account menu rides the same clock. Nothing here calls
        // `notify_layout`: the panel's open amount is a style value, and
        // `UiTree::build` marks a node layout-dirty exactly when its style
        // changed, so animating the style *is* the invalidation.
        let mut menu = self.state.user_menu.get();
        menu.retarget(if self.state.user_menu_open.get() { 1.0 } else { 0.0 });
        menu.step(frame.delta);
        moving |= !menu.is_settled();
        self.state.user_menu.set(menu);

        // The Identity page's own dropdown. A second spring rather than a
        // shared one, so the two panels can be open at the same time and it is
        // visible that neither widget owns any state of its own.
        let mut demo = self.state.demo_menu.get();
        demo.retarget(if self.state.demo_menu_open.get() { 1.0 } else { 0.0 });
        demo.step(frame.delta);
        moving |= !demo.is_settled();
        self.state.demo_menu.set(demo);

        let mut ctx = self.state.ctx_menu.get();
        ctx.retarget(if self.state.ctx_menu_open.get() { 1.0 } else { 0.0 });
        ctx.step(frame.delta);
        moving |= !ctx.is_settled();
        self.state.ctx_menu.set(ctx);

        // The indeterminate bar is a function of paint time, so it only moves
        // while frames keep coming. It is on screen only on one page, so only
        // that page pays for the continuous redraw.
        moving |= self.state.page.get() == Page::Containers && self.state.busy.get();

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

fn main() {
    if let Err(e) = App::new(GalleryApp::new()).run() {
        eprintln!("event loop failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit::core::ScaleFactor;
    use spherekit::render::{Canvas, Scene};
    use spherekit::text::{FontRequest, TextSystem};
    use spherekit::ui::UiTree;

    /// Builds and lays out one page, returning the tree it produced.
    fn lay_out(page: Page, text: &mut TextSystem) -> (UiTree, spherekit::core::Size<Px>) {
        let mut app = GalleryApp::new();
        app.state.page.set(page);
        let viewport = size(px(1100.0), px(720.0));
        let mut tree = UiTree::new();
        tree.set_theme(app.state.theme());
        tree.build(app.build());
        tree.compute_layout_with_text(viewport, text).unwrap();
        (tree, viewport)
    }

    #[test]
    fn every_page_builds_lays_out_and_paints() {
        // The gallery's whole claim is that these pages are live. A page that
        // panics on build, or lays out to nothing, is the one failure mode that
        // would make the claim false without looking broken in a screenshot.
        let mut text = TextSystem::with_system_fonts();
        for page in Page::ALL {
            let (mut tree, viewport) = lay_out(page, &mut text);
            let mut scene = Scene::new(viewport, ScaleFactor::IDENTITY);
            {
                let mut canvas = Canvas::new(&mut scene);
                tree.paint(&mut canvas, &mut text, viewport, 0.0);
            }
            assert!(!scene.is_empty(), "the {} page painted nothing at all", page.title());
            assert!(
                tree.stats().elements > 40,
                "the {} page built only {} elements — it is probably empty",
                page.title(),
                tree.stats().elements
            );
        }
    }

    #[test]
    fn the_chrome_keeps_its_height_in_a_short_window() {
        // Height, not page length, is the other way the column runs short. A
        // window dragged down to a sliver still has to keep its caption and its
        // status bar intact — they are the two things the user grabs to undo it.
        let mut text = TextSystem::with_system_fonts();
        for height in [720.0, 400.0, 300.0, 220.0, 160.0] {
            let mut app = GalleryApp::new();
            app.state.page.set(Page::Palette);
            let viewport = size(px(560.0), px(height));
            let mut tree = UiTree::new();
            tree.set_theme(app.state.theme());
            tree.build(app.build());
            tree.compute_layout_with_text(viewport, &mut text).unwrap();

            let header = tree.bounds_of("chrome.header").unwrap();
            let status = tree.bounds_of("chrome.status").unwrap();
            assert_eq!(
                header.height(),
                px(CAPTION_HEIGHT),
                "the caption was squashed in a {height} px window"
            );
            assert_eq!(
                status.height(),
                px(26.0),
                "the status bar was squashed in a {height} px window"
            );
        }
    }

    #[test]
    fn the_chrome_keeps_its_height_on_every_page() {
        // A flex item with a height still has `flex_shrink: 1` by default, so
        // a long page silently squashes the caption and the status bar — the
        // status bar was down to 15 px of its 26 before this was pinned. The
        // longest page is the one that used to break it, so every page is
        // checked rather than a representative one.
        let mut text = TextSystem::with_system_fonts();
        for page in Page::ALL {
            let (tree, _) = lay_out(page, &mut text);
            let header = tree.bounds_of("chrome.header").expect("the header is built");
            let status = tree.bounds_of("chrome.status").expect("the status bar is built");
            assert_eq!(
                header.height(),
                px(CAPTION_HEIGHT),
                "the caption was squashed on the {} page",
                page.title()
            );
            assert_eq!(
                status.height(),
                px(26.0),
                "the status bar was squashed on the {} page",
                page.title()
            );
        }
    }

    #[test]
    fn the_content_pane_can_actually_scroll_on_a_long_page() {
        // The other half of the same flexbox trap: without `min_h(0)` on the
        // row and the pane, the pane grows to fit the page instead of
        // overflowing, and the wheel has nothing to move.
        let mut text = TextSystem::with_system_fonts();
        let (mut tree, _) = lay_out(Page::Palette, &mut text);
        tree.dispatch(&spherekit::ui::UiEvent::Scroll(spherekit::ui::ScrollEvent {
            position: spherekit::core::Point::new(px(700.0), px(300.0)),
            delta: spherekit::ui::ScrollDelta::Lines(spherekit::core::Size::new(0.0, -3.0)),
            modifiers: spherekit::ui::Modifiers::NONE,
            momentum: false,
        }));
        // The wheel now sets a destination and the tree glides there, so the
        // frames a window would draw have to be drawn here too.
        for _ in 0..120 {
            if !tree.advance(std::time::Duration::from_millis(16)) {
                break;
            }
        }
        let offset = tree.scroll_offset_of("content-scroll").expect("the pane is built");
        assert!(offset.height > Px::ZERO, "a long page did not scroll: {offset:?}");
    }

    #[test]
    fn every_page_has_a_title_and_a_blurb() {
        for page in Page::ALL {
            assert!(!page.title().is_empty());
            assert!(!page.blurb().is_empty(), "{} has no summary line", page.title());
            assert!(page.icon().starts_with("<svg"), "{} has no icon", page.title());
        }
    }

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
    fn the_text_page_paints_regular_semibold_and_bold_faces() {
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

        // The weights live on the Text page, which is the one that claims them.
        let (mut tree, viewport) = lay_out(Page::Text, &mut text);
        let mut scene = Scene::new(viewport, ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut text, viewport, 0.0);
        }

        for (name, face) in [("regular", regular), ("semibold", semibold), ("bold", bold)] {
            assert!(
                scene.runs.iter().any(|run| run.font == face),
                "the Text page never painted its {name} face"
            );
        }
    }
}
