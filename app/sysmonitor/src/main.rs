//! # SphereKit System Monitor
//!
//! A task manager: what the machine is doing, sampled straight from the Win32
//! API and drawn by SphereKit's own widgets.
//!
//! The shell is the same one the UI Gallery uses — a custom Windows frame over
//! DWM Mica, a collapsing sidebar, a status line — because that shell is what a
//! shipping Windows application looks like and there was no reason to invent a
//! second one. What is different is underneath it: every number on every page
//! comes from [`sys`], which is nothing but direct calls into `kernel32`,
//! `psapi`, `advapi32` and `iphlpapi`.
//!
//! ```text
//! cargo run -p sysmonitor --release
//! ```
//!
//! Keyboard: Tab and Shift-Tab move focus, Space and Enter activate, Escape
//! closes a menu and then quits, Delete ends the selected process.
//!
//! `SPHEREKIT_MONITOR_PAGE=Memory` opens straight onto one page, by title.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod graph;
mod pages;
mod sys;

use pages::Page;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    AnyElement, ButtonVariant, Cursor, Element, EventContext, InputTranslator, Interactive,
    IntoElement, ParentElement, Presence, Role, Semantics, Styled, StyledInteraction, TextEdit,
    TextRole, Theme, ToastVariant, TypeScale, avatar, button, div, dropdown, label, overlay,
    scroll_view, segmented, separator, spinner, toast, toast_layer,
};
use spherekit::{SphereKitSurface, SurfaceOptions};

use sys::{MachineInfo, Sampler, Snapshot};

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// The product theme.
///
/// The application owns this mapping rather than changing SphereKit's default
/// theme: a monitor sitting next to the shell's own Task Manager should read as
/// a peer of it, and that is a decision about this product, not about the
/// engine's defaults.
fn monitor_dark_theme() -> Theme {
    let mut theme = Theme::dark();
    let c = &mut theme.colors;

    c.background = Color::hex(0x24262A);
    c.mica_surface = Color::hex(0x24262A).with_alpha(0.74);
    c.surface = Color::hex(0x2D3035);
    c.elevated = Color::hex(0x35383E);

    c.hover = Color::hex(0x3D4046);
    c.pressed = Color::hex(0x474A51);

    c.border = Color::hex(0x3A3D43);
    c.border_strong = Color::hex(0x54585F);

    c.text = Color::hex(0xF1F2F4);
    c.text_muted = Color::hex(0xA6A9AF);
    c.text_on_accent = Color::hex(0x14161A);

    // A teal accent rather than the shell's blue: the graphs are the loudest
    // thing on screen and blue-on-Mica is where a chart line goes to disappear.
    c.accent = Color::hex(0x5CC8C0);
    c.accent_hover = Color::hex(0x76D6CE);
    c.focus = Color::hex(0x5CC8C0);

    c.success = Color::hex(0x79B88A);
    c.warning = Color::hex(0xE0B15F);
    c.danger = Color::hex(0xE0777F);

    theme.typography.xs = px(10.0);
    theme.typography.sm = px(12.0);
    theme.typography.md = px(14.0);
    theme.typography.lg = px(17.0);
    theme.typography.xl = px(22.0);
    theme.typography.weight = FontWeight::NORMAL;
    theme.typography.strong = FontWeight::SEMI_BOLD;

    theme.radii.sm = px(4.0);
    theme.radii.md = px(6.0);
    theme.radii.lg = px(8.0);

    theme
}

/// Maps the operating system appearance to the product theme.
fn product_theme(system_theme: PlatformTheme) -> Theme {
    match system_theme {
        PlatformTheme::Dark => monitor_dark_theme(),
        PlatformTheme::Light => {
            let mut theme = Theme::light();
            theme.colors.mica_surface = Color::hex(0xF7F8FA).with_alpha(0.74);
            theme.colors.accent = Color::hex(0x1E8E86);
            theme.colors.accent_hover = Color::hex(0x24A69C);
            theme.colors.focus = Color::hex(0x1E8E86);
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

/// Which theme the window is showing, whatever the operating system says.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Appearance {
    System,
    Light,
    Dark,
}

impl Appearance {
    const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    fn title(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// How many samples each chart remembers.
///
/// At the default one-second interval that is two minutes of history, which is
/// the window Task Manager shows and about as far back as anyone reads a spike.
pub(crate) const HISTORY: usize = 120;

/// The ring buffers behind the charts.
///
/// Kept here rather than in [`graph::Graph`] because an element is rebuilt
/// every frame and a history that died with it would be one sample long. The
/// sampler pushes; the charts read.
#[derive(Default)]
pub(crate) struct History {
    /// Whole-machine CPU, `0..=1`.
    pub(crate) cpu: Vec<f32>,
    /// Physical memory in use, `0..=1`.
    pub(crate) memory: Vec<f32>,
    /// Receive and transmit, in bytes per second. Stored *unnormalised*,
    /// because the axis a network chart needs is not known until the whole
    /// series is in hand — see [`History::network_scale`].
    pub(crate) rx: Vec<f32>,
    pub(crate) tx: Vec<f32>,
}

impl History {
    fn push(series: &mut Vec<f32>, value: f32) {
        if series.len() == HISTORY {
            series.remove(0);
        }
        series.push(value);
    }

    fn record(&mut self, snap: &Snapshot) {
        Self::push(&mut self.cpu, (snap.cpu / 100.0).clamp(0.0, 1.0));
        let mem = if snap.memory.total > 0 {
            snap.memory.used as f32 / snap.memory.total as f32
        } else {
            0.0
        };
        Self::push(&mut self.memory, mem.clamp(0.0, 1.0));
        Self::push(&mut self.rx, snap.rx_rate() as f32);
        Self::push(&mut self.tx, snap.tx_rate() as f32);
    }

    /// The full-scale value the network chart should be drawn against.
    ///
    /// A network graph has no natural ceiling — a link's advertised speed is
    /// almost never what it does — so the axis is the largest rate seen in the
    /// window, rounded up, with a floor so an idle adapter does not draw its
    /// own noise as a mountain range.
    pub(crate) fn network_scale(&self) -> f32 {
        const FLOOR: f32 = 64.0 * 1024.0;
        let peak = self.rx.iter().chain(self.tx.iter()).copied().fold(0.0f32, f32::max);
        (peak * 1.25).max(FLOOR)
    }

    /// One series, normalised against a scale, ready for a chart.
    pub(crate) fn scaled(series: &[f32], scale: f32) -> Vec<f32> {
        if scale <= 0.0 {
            return vec![0.0; series.len()];
        }
        series.iter().map(|v| (v / scale).clamp(0.0, 1.0)).collect()
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// How the process list is ordered.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SortKey {
    Name,
    Cpu,
    Memory,
    Pid,
    Threads,
}

impl SortKey {
    pub(crate) const ALL: [SortKey; 5] =
        [SortKey::Name, SortKey::Cpu, SortKey::Memory, SortKey::Pid, SortKey::Threads];

    pub(crate) fn title(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Cpu => "CPU",
            SortKey::Memory => "Memory",
            SortKey::Pid => "PID",
            SortKey::Threads => "Threads",
        }
    }

    /// Which way round a fresh click on this column should sort.
    ///
    /// A name sorts A-to-Z and a measurement sorts largest-first, because the
    /// question behind clicking "CPU" is always "what is using it".
    fn default_descending(self) -> bool {
        !matches!(self, SortKey::Name)
    }
}

/// How often the machine is asked.
const INTERVALS: [(f32, &str); 4] = [(0.5, "0.5 s"), (1.0, "1 s"), (2.0, "2 s"), (5.0, "5 s")];

/// One entry in the application's own toast list.
pub(crate) struct ToastEntry {
    pub(crate) key: u64,
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) variant: ToastVariant,
    pub(crate) born: f32,
    pub(crate) fade: Motion<f32>,
    pub(crate) leaving: bool,
}

/// How long a toast stays before it starts to leave, in seconds.
const TOAST_LIFETIME: f32 = 4.5;
/// The most toasts on screen at once.
const TOAST_LIMIT: usize = 3;

/// Everything the UI reads, and everything a handler may write.
///
/// One shared struct of `Cell`s rather than a mutable borrow: the element tree
/// is rebuilt every frame, so a handler installed during one build has to be
/// able to change what the *next* build reads.
pub(crate) struct State {
    system_theme: Cell<PlatformTheme>,
    pub(crate) page: Cell<Page>,
    pub(crate) appearance: Cell<Appearance>,

    // --- what the machine said --------------------------------------------
    /// The most recent sample. Replaced whole rather than mutated in place, so
    /// a page always draws one consistent moment rather than a mixture of two.
    pub(crate) snapshot: RefCell<Snapshot>,
    /// The unchanging facts, read once.
    pub(crate) machine: MachineInfo,
    pub(crate) history: RefCell<History>,
    /// Which entry in [`INTERVALS`] is selected.
    pub(crate) interval: Cell<usize>,
    /// Set by the pause button. The window keeps drawing; it stops *asking*.
    pub(crate) paused: Cell<bool>,

    // --- the process list --------------------------------------------------
    pub(crate) sort: Cell<SortKey>,
    pub(crate) sort_desc: Cell<bool>,
    pub(crate) filter: RefCell<TextEdit>,
    /// The selected row, by pid. `None` once that process exits, because a
    /// selection that survived its process would end the wrong one.
    pub(crate) selected: Cell<Option<u32>>,

    // --- ending a process --------------------------------------------------
    pub(crate) kill_open: Cell<bool>,
    pub(crate) kill: Cell<Motion<f32>>,
    /// What the confirm dialog is about, captured when it opened.
    ///
    /// Held separately from [`State::selected`] on purpose: the list keeps
    /// re-sorting underneath an open dialog, and a dialog that read the
    /// selection live could end up asking about one process and ending another.
    pub(crate) kill_target: RefCell<Option<(u32, String)>>,

    // --- chrome ------------------------------------------------------------
    pub(crate) sidebar_open: Cell<bool>,
    pub(crate) sidebar: Cell<Motion<f32>>,
    pub(crate) page_in: Cell<Motion<f32>>,
    /// A toast a callback asked for, drained by the frame loop.
    ///
    /// Queued rather than pushed directly because a toast needs the frame's
    /// clock reading to know when it was born, and a callback has no clock.
    pub(crate) pending_toast: RefCell<Option<(ToastVariant, String, String)>>,
    pub(crate) toasts: RefCell<Vec<ToastEntry>>,
    pub(crate) next_toast: Cell<u64>,

    maximized: Cell<bool>,
    wco_hovered: [Cell<bool>; CAPTION_BUTTONS],
    wco_fade: [Cell<Motion<f32>>; CAPTION_BUTTONS],
    user_menu_open: Cell<bool>,
    user_menu: Cell<Motion<f32>>,
    press_inside_account: Cell<bool>,
    account_anchor: Cell<spherekit::core::Rect<Px>>,
    /// A window command the caption asked for, drained by the runner.
    pending: Cell<Option<WindowCommand>>,
    log: RefCell<Vec<String>>,
}

impl State {
    fn new(machine: MachineInfo) -> Rc<Self> {
        Rc::new(Self {
            system_theme: Cell::new(PlatformTheme::Dark),
            page: Cell::new(
                std::env::var("SPHEREKIT_MONITOR_PAGE")
                    .ok()
                    .and_then(|name| Page::from_name(&name))
                    .unwrap_or(Page::Overview),
            ),
            appearance: Cell::new(Appearance::System),
            snapshot: RefCell::new(Snapshot::default()),
            machine,
            history: RefCell::new(History::default()),
            interval: Cell::new(1),
            paused: Cell::new(false),
            sort: Cell::new(SortKey::Cpu),
            sort_desc: Cell::new(true),
            filter: RefCell::new(TextEdit::new()),
            selected: Cell::new(None),
            kill_open: Cell::new(false),
            kill: Cell::new(Motion::at(0.0, Drive::STIFF)),
            kill_target: RefCell::new(None),
            // Settled and fully arrived, so a tree built outside the frame loop
            // gets an opaque page rather than a half-finished transition.
            page_in: Cell::new(Motion::at(1.0, Drive::SMOOTH)),
            sidebar_open: Cell::new(true),
            sidebar: Cell::new(Motion::at(1.0, Drive::SMOOTH)),
            pending_toast: RefCell::new(None),
            toasts: RefCell::new(Vec::new()),
            next_toast: Cell::new(1),
            maximized: Cell::new(false),
            wco_hovered: [const { Cell::new(false) }; CAPTION_BUTTONS],
            wco_fade: core::array::from_fn(|_| Cell::new(Motion::at(0.0, Drive::SMOOTH))),
            user_menu_open: Cell::new(false),
            user_menu: Cell::new(Motion::at(0.0, Drive::STIFF)),
            press_inside_account: Cell::new(false),
            account_anchor: Cell::new(spherekit::core::Rect::ZERO),
            pending: Cell::new(None),
            log: RefCell::new(vec!["Sampling.".into()]),
        })
    }

    fn theme(&self) -> Theme {
        product_theme(self.effective_theme())
    }

    fn effective_theme(&self) -> PlatformTheme {
        match self.appearance.get() {
            Appearance::System => self.system_theme.get(),
            Appearance::Light => PlatformTheme::Light,
            Appearance::Dark => PlatformTheme::Dark,
        }
    }

    /// How long between samples, in seconds.
    pub(crate) fn interval_secs(&self) -> f32 {
        INTERVALS[self.interval.get().min(INTERVALS.len() - 1)].0
    }

    /// The process list, filtered and sorted as the page header asks.
    ///
    /// Returned as a fresh `Vec` rather than sorting the snapshot in place: the
    /// snapshot is what every *other* page reads, and reordering it to answer a
    /// question about one page would be a side effect nothing asked for.
    pub(crate) fn visible_processes(&self) -> Vec<sys::ProcInfo> {
        let needle = self.filter.borrow().text().trim().to_lowercase();
        let snap = self.snapshot.borrow();
        let mut list: Vec<sys::ProcInfo> = snap
            .processes
            .iter()
            .filter(|p| {
                needle.is_empty()
                    || p.name.to_lowercase().contains(&needle)
                    || p.pid.to_string().contains(&needle)
            })
            .cloned()
            .collect();

        let desc = self.sort_desc.get();
        list.sort_by(|a, b| {
            let ordering = match self.sort.get() {
                // Case-insensitive, or every capitalised service name sorts
                // ahead of every lower-case one and the list reads as random.
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Cpu => a.cpu.partial_cmp(&b.cpu).unwrap_or(std::cmp::Ordering::Equal),
                SortKey::Memory => a.working_set.cmp(&b.working_set),
                SortKey::Pid => a.pid.cmp(&b.pid),
                SortKey::Threads => a.threads.cmp(&b.threads),
            };
            // Broken by pid, so two idle processes do not swap places every
            // sample and make a still list look like it is shivering.
            let ordering = if desc { ordering.reverse() } else { ordering };
            ordering.then(a.pid.cmp(&b.pid))
        });
        list
    }

    /// Clicks a column heading: the same one reverses, a new one takes its own
    /// natural direction.
    pub(crate) fn sort_by(&self, key: SortKey) {
        if self.sort.get() == key {
            self.sort_desc.set(!self.sort_desc.get());
        } else {
            self.sort.set(key);
            self.sort_desc.set(key.default_descending());
        }
    }

    /// Opens the confirm dialog over one process.
    pub(crate) fn ask_to_end(&self, pid: u32, name: &str) {
        *self.kill_target.borrow_mut() = Some((pid, name.to_string()));
        self.kill_open.set(true);
    }

    pub(crate) fn toast(&self, variant: ToastVariant, title: &str, message: impl Into<String>) {
        *self.pending_toast.borrow_mut() = Some((variant, title.to_string(), message.into()));
    }

    /// Pushes a toast, retiring the oldest if the screen is full.
    fn push_toast(&self, at: f32, variant: ToastVariant, title: String, message: String) {
        let mut toasts = self.toasts.borrow_mut();
        let live = toasts.iter().filter(|t| !t.leaving).count();
        if live >= TOAST_LIMIT
            && let Some(oldest) = toasts.iter_mut().find(|t| !t.leaving)
        {
            oldest.leaving = true;
        }
        let key = self.next_toast.get();
        self.next_toast.set(key + 1);
        toasts.push(ToastEntry {
            key,
            title,
            message,
            variant,
            born: at,
            fade: Motion::at(0.0, Drive::SMOOTH),
            leaving: false,
        });
    }

    pub(crate) fn dismiss_toast(&self, key: u64) {
        if let Some(entry) = self.toasts.borrow_mut().iter_mut().find(|t| t.key == key) {
            entry.leaving = true;
        }
    }

    /// Opens or closes the sidebar. Returns whether anything changed.
    pub(crate) fn set_sidebar(&self, open: bool) -> bool {
        if self.sidebar_open.get() == open {
            return false;
        }
        self.sidebar_open.set(open);
        true
    }

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
        // be an unbounded allocation in a window meant to run for days.
        if log.len() > 32 {
            log.remove(0);
        }
    }
}

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

struct MonitorApp {
    window: Option<Arc<spherekit::platform::backend::Window>>,
    surface: Option<SphereKitSurface>,
    input: InputTranslator,
    started: Instant,
    state: Rc<State>,
    sampler: Sampler,
    /// When the machine was last asked. Kept here rather than in [`State`]
    /// because nothing in the tree has any business knowing.
    last_sample: Instant,
    icons: Vec<(Page, spherekit::core::SvgId)>,
    frames: u64,
    frame_limit: Option<u64>,
    reported: bool,
    shown_page: Option<Page>,
    applied_theme: Option<Theme>,
    ime_allowed: bool,
    ime_caret: Option<spherekit::core::Rect<Px>>,
    timeline: Timeline,
    clock: SystemClock,
}

impl MonitorApp {
    fn new() -> Self {
        let machine = sys::machine_info();
        let sampler = Sampler::new(&machine);
        Self {
            window: None,
            surface: None,
            input: InputTranslator::new(),
            started: Instant::now(),
            state: State::new(machine),
            sampler,
            // Deliberately in the past, so the first frame samples rather than
            // waiting a whole interval to show anything.
            last_sample: Instant::now() - Duration::from_secs(60),
            icons: Vec::new(),
            shown_page: None,
            applied_theme: None,
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

    /// Asks the machine, if it is time to.
    ///
    /// The whole cost of this application is here: one pass over every process
    /// with an `OpenProcess` each. It runs on its own interval rather than per
    /// frame, which is what lets the window animate at the display's rate while
    /// the numbers change once a second — the same split every other
    /// application makes between what it draws and what it knows.
    fn maybe_sample(&mut self) {
        if self.state.paused.get() {
            return;
        }
        let due = Duration::from_secs_f32(self.state.interval_secs());
        if self.last_sample.elapsed() < due {
            return;
        }
        self.last_sample = Instant::now();
        let snapshot = self.sampler.sample();
        self.state.history.borrow_mut().record(&snapshot);

        // A selection outlives a re-sort but must not outlive its process:
        // Windows reuses pids, and an ended task whose number came back would
        // leave the wrong row highlighted and armed.
        if let Some(pid) = self.state.selected.get()
            && !snapshot.processes.iter().any(|p| p.pid == pid)
        {
            self.state.selected.set(None);
        }
        *self.state.snapshot.borrow_mut() = snapshot;
    }

    fn build(&mut self) -> AnyElement {
        let theme = self.state.theme();
        if let Some(surface) = self.surface.as_ref()
            && let Some(row) = surface.tree().bounds_of("acct.row")
        {
            self.state.account_anchor.set(row);
        }

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
                    // pane inside would then never overflow, so a three-hundred
                    // row process list would simply run off the bottom.
                    .min_h(px(0.0))
                    .z(1)
                    .child(self.sidebar(&theme))
                    .child(separator(true).bg(Color::TRANSPARENT))
                    .child(self.content(&theme)),
            )
            .child(separator(false).bg(Color::TRANSPARENT))
            .child(self.status_bar(&theme))
            // Last children of the root, so their coordinates are window
            // coordinates and they paint over everything.
            .child(self.confirm_dialog(&theme))
            .child(self.account_menu(&theme))
            .into_element()
    }

    /// The "end this process?" dialog: a scrim with a card centred on it.
    ///
    /// At the root of the tree, so the scrim covers the window. An overlay
    /// fills its *parent*, and one built inside the content pane would leave
    /// the sidebar and the caption live — which is a dialog that only looks
    /// modal, over an action that cannot be undone.
    fn confirm_dialog(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let open = self.state.kill.get().value();
        let (pid, name) = self.state.kill_target.borrow().clone().unwrap_or_default();
        let close = {
            let state = Rc::clone(&self.state);
            move || state.kill_open.set(false)
        };

        overlay(open)
            .id("modal")
            .on_dismiss(close.clone())
            .child(
                div()
                    .flex_col()
                    .w(px(400.0))
                    .gap(theme.spacing.md)
                    .p(theme.spacing.lg)
                    .rounded(theme.radii.lg)
                    .bg(c.surface)
                    .border(px(1.0), c.border)
                    .shadow(theme.shadows.lg)
                    .child(
                        label(format!("End {name}?"))
                            .scale(TypeScale::Lg)
                            .weight(theme.typography.strong),
                    )
                    .child(
                        label(format!(
                            "Process {pid} is ended immediately. Unsaved work in it is lost, \
                             and a system process taken down this way can take the session \
                             with it."
                        ))
                        .scale(TypeScale::Sm)
                        .role(TextRole::Muted),
                    )
                    .child(div().h(theme.spacing.xs))
                    .child(
                        div()
                            .flex_row()
                            .justify(spherekit::layout::Distribute::End)
                            .gap(theme.spacing.sm)
                            .child({
                                let close = close.clone();
                                button("Cancel")
                                    .id("modal.cancel")
                                    .variant(ButtonVariant::Outline)
                                    .on_press(move || close.clone()())
                            })
                            .child({
                                let close = close.clone();
                                let state = Rc::clone(&self.state);
                                let name = name.clone();
                                button("End process")
                                    .id("modal.end")
                                    .variant(ButtonVariant::Danger)
                                    .on_press(move || {
                                        close.clone()();
                                        match sys::terminate(pid) {
                                            Ok(()) => {
                                                state.selected.set(None);
                                                state.say(format!("Ended {name} ({pid})."));
                                                state.toast(
                                                    ToastVariant::Success,
                                                    "Process ended",
                                                    format!("{name} — pid {pid}"),
                                                );
                                            }
                                            Err(why) => {
                                                state.say(format!("Could not end {name}: {why}"));
                                                state.toast(
                                                    ToastVariant::Danger,
                                                    "Could not end it",
                                                    why,
                                                );
                                            }
                                        }
                                    })
                            }),
                    ),
            )
            .into_element()
    }

    /// Everything currently being announced, stacked in the bottom-right.
    fn toasts(&self, _theme: &Theme) -> AnyElement {
        let mut layer = toast_layer(true, true);
        for entry in self.state.toasts.borrow().iter() {
            let state = Rc::clone(&self.state);
            let key = entry.key;
            layer = layer.child(
                toast(entry.message.clone(), entry.fade.value())
                    .id(("toast", entry.key))
                    .title(entry.title.clone())
                    .variant(entry.variant)
                    .on_dismiss(move || state.dismiss_toast(key)),
            );
        }
        layer.into_element()
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
            // height and the default shrink of one is squashed whenever the
            // column runs short, and the caption quietly loses pixels on a long
            // page — the one place it must never move.
            .shrink(0.0)
            .pl(px(6.0))
            .z(2)
            .child({
                let toggle = Rc::clone(&state);
                let open = self.state.sidebar_open.get();
                button("\u{E700}")
                    .id("chrome.sidebar")
                    .variant(ButtonVariant::Ghost)
                    .font(ICON_FONT)
                    .text_size(px(11.0))
                    .width(px(28.0))
                    .height(px(24.0))
                    .on_press(move || {
                        toggle.set_sidebar(!open);
                    })
            })
            .child(
                label("SphereKit")
                    .text_size(theme.typography.sm)
                    .weight(theme.typography.strong)
                    .text_color(c.text)
                    .no_wrap(),
            )
            .child(label("/").text_size(theme.typography.sm).text_color(c.text_muted).no_wrap())
            .child(
                label("System Monitor")
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
        // One spring drives the width, the label alpha and the heading. Reading
        // all three off the same number is what keeps them in step.
        let open = self.state.sidebar.get().value().clamp(0.0, 1.0);
        let width = px(SIDEBAR_COLLAPSED + (SIDEBAR_WIDTH - SIDEBAR_COLLAPSED) * open);
        let row_width = width - px(NAV_INSET * 2.0);
        // Faded rather than removed. A label that left the tree would take the
        // row's width with it and the sidebar would jump instead of sliding.
        let text = c.text.scale_alpha(open);
        let muted = c.text_muted.scale_alpha(open);
        let mut nav = div()
            .flex_col()
            // Clipped, and scrollable once it no longer fits: the rows have
            // fixed heights and a short window would otherwise draw them
            // straight over the footer.
            .overflow_y_scroll()
            .grow(1.0)
            .min_h(px(0.0))
            .px_(px(NAV_INSET))
            .pt(theme.spacing.md)
            .gap(theme.spacing.xs)
            .child(
                label("MONITOR")
                    .text_size(theme.typography.xs)
                    .weight(theme.typography.strong)
                    .text_color(muted)
                    .no_wrap()
                    .px_(px(NAV_INSET))
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
                    .shrink(0.0)
                    .w(row_width)
                    .px_(px(NAV_INSET))
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
                            .text_color(text)
                            .no_wrap(),
                    )
                    .on_click(move |cx: &mut EventContext<'_>| {
                        state.page.set(section);
                        cx.notify_layout();
                    }),
            );
        }
        // Keep the backdrop effect isolated from the navigation content: a
        // filtered parent composites an off-screen layer, and putting the
        // labels in that layer can make them disappear when the adjacent
        // opaque content pane is repainted.
        div()
            .id("chrome.sidebar-panel")
            .flex_col()
            .w(width)
            .shrink(0.0)
            .h(relative(1.0))
            .overflow_hidden()
            .child(div().absolute().inset(px(0.0)).backdrop_blur(px(18.0)))
            .child(nav)
            .child(self.account_footer(theme))
            .into_element()
    }

    /// The machine this is a monitor of, pinned to the bottom of the sidebar.
    ///
    /// The row lives here; the *menu* does not. The sidebar panel clips its
    /// overflow — that is what makes the labels slide out under its edge as it
    /// collapses — and a panel that clips clips everything inside it.
    fn account_footer(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let shown = self.state.sidebar.get().value().clamp(0.0, 1.0);
        let width = px(SIDEBAR_COLLAPSED + (SIDEBAR_WIDTH - SIDEBAR_COLLAPSED) * shown);
        let row_width = width - px(FOOTER_INSET * 2.0);
        let open = self.state.user_menu.get().value();
        let expanded = self.state.user_menu_open.get();
        let state = Rc::clone(&self.state);
        let host = self.state.machine.host.clone();
        let user = self.state.machine.user.clone();

        let row = div()
            .id("acct.row")
            .focusable()
            .flex_row()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(44.0))
            .shrink(0.0)
            .w(row_width)
            .px_(px(FOOTER_INSET))
            .rounded(theme.radii.md)
            .bg(if expanded { c.pressed } else { Color::TRANSPARENT })
            .hover_bg(c.hover)
            .active_bg(c.pressed)
            .cursor(Cursor::Pointer)
            .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
            .semantics(Semantics::new(Role::Button, "Machine menu"))
            .child(
                avatar(host.clone())
                    .size(px(28.0))
                    // Online because the machine is, by definition, running.
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
                        label(host)
                            .text_size(theme.typography.sm)
                            .weight(theme.typography.strong)
                            .text_color(c.text.scale_alpha(shown))
                            .no_wrap(),
                    )
                    .child(
                        label(user)
                            .text_size(theme.typography.xs)
                            .text_color(c.text_muted.scale_alpha(shown))
                            .no_wrap(),
                    ),
            )
            .child(ChevronElement { tint: c.text_muted.scale_alpha(shown), open })
            .on_click(move |cx: &mut EventContext<'_>| {
                // The menu is built from this rectangle, and this is the one
                // place it is known for free: a handler is handed its own
                // absolute bounds.
                state.account_anchor.set(cx.bounds);
                state.set_user_menu(!state.user_menu_open.get());
                cx.notify();
            })
            // `on_click` is a mouse contract — it only ever fires on MouseUp —
            // so a row that calls itself a button has to answer the keyboard
            // itself, or Tab would reach it and nothing would happen.
            .on_key({
                let state = Rc::clone(&self.state);
                move |cx: &mut EventContext<'_>| {
                    use spherekit::ui::{EventFlow, Key, UiEvent};
                    let UiEvent::Key(key) = cx.event else { return EventFlow::Continue };
                    if key.state.is_pressed() && matches!(key.key, Key::Space | Key::Enter) {
                        state.account_anchor.set(cx.bounds);
                        state.set_user_menu(!state.user_menu_open.get());
                        cx.notify();
                        return EventFlow::Stop;
                    }
                    EventFlow::Continue
                }
            });

        let claim = Rc::clone(&self.state);
        div()
            .flex_col()
            .shrink(0.0)
            .px_(px(FOOTER_INSET))
            .pb(theme.spacing.sm)
            .pt(theme.spacing.xs)
            // Presses bubble out through here, so this is the one place that
            // knows a press landed on the machine UI — whichever part of it.
            .on_mouse_down(move |_| claim.press_inside_account.set(true))
            .child(separator(false).bg(c.border))
            .child(div().h(theme.spacing.xs))
            .child(row)
            .into_element()
    }

    /// The machine menu, built at the root of the tree.
    ///
    /// Two separate things force it out of the sidebar. The panel clips, so a
    /// menu inside it is cut off at its edge for painting *and* for hit
    /// testing, which is unusable at 64 px wide. And `z-index` orders siblings
    /// only, so even an unclipped menu inside the sidebar would paint under the
    /// content pane next to it.
    fn account_menu(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let open = self.state.user_menu.get().value();
        let anchor = self.state.account_anchor.get();
        // Nothing has opened it yet, so there is no rectangle to hang it off.
        if anchor.is_empty() {
            return div().into_element();
        }
        let claim = Rc::clone(&self.state);

        // A stand-in for the row, at the row position in window coordinates.
        //
        // Zero height, and that is load-bearing. Sized to the row it would sit
        // *over* the row at a higher z, and hit testing takes the topmost node
        // — so the row would stop hovering and stop opening the menu.
        div()
            .id("acct.anchor")
            .absolute()
            .left(anchor.min_x())
            .top(anchor.min_y())
            .w(anchor.width())
            .h(px(0.0))
            .z(2)
            .on_mouse_down(move |_| claim.press_inside_account.set(true))
            .child(
                dropdown(open)
                    .id("acct.menu")
                    .above()
                    .offset(theme.spacing.sm)
                    // Given rather than inherited: an unsized dropdown spans its
                    // anchor, and the anchor is 48 px wide once collapsed.
                    .w(px(ACCOUNT_MENU_WIDTH))
                    .p(theme.spacing.xs)
                    .gap(theme.spacing.xs)
                    .child(self.account_menu_item(theme, "acct.system", "System summary", false))
                    .child(self.account_menu_item(theme, "acct.copy", "Copy report", false))
                    .child(separator(false).bg(c.border).m(theme.spacing.xs))
                    .child(self.account_menu_item(theme, "acct.quit", "Quit", true)),
            )
            .into_element()
    }

    /// One row inside the machine menu.
    fn account_menu_item(
        &self,
        theme: &Theme,
        key: &'static str,
        text: &'static str,
        danger: bool,
    ) -> AnyElement {
        let c = theme.colors;
        let act = {
            let state = Rc::clone(&self.state);
            move || {
                state.set_user_menu(false);
                match key {
                    "acct.system" => {
                        state.page.set(Page::System);
                        state.say("System.");
                    }
                    "acct.copy" => {
                        let report = report_text(&state);
                        let _ = spherekit::platform::Clipboard::system().set_text(&report);
                        state.say("Report copied.");
                        state.toast(
                            ToastVariant::Success,
                            "Copied",
                            "The summary is on the clipboard.",
                        );
                    }
                    _ => state.pending.set(Some(WindowCommand::Close)),
                }
            }
        };
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
            .on_click({
                let act = act.clone();
                move |cx: &mut EventContext<'_>| {
                    act();
                    cx.notify();
                }
            })
            .on_key(keyboard_activate(act))
            .into_element()
    }

    fn content(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let body = pages::render(self.state.page.get(), &self.state, theme);
        let reveal = self.state.page_in.get().value().clamp(0.0, 1.0);

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
                    .h(px(PAGE_HEADER_HEIGHT))
                    .shrink(0.0)
                    .px_(px(12.0))
                    // Above the pane it is heading, so its shadow lands on the
                    // content rather than under it.
                    .z(1)
                    // A `no_wrap` label's minimum width is its whole string, so
                    // two of them in one row simply refuse to yield and get
                    // drawn over each other. The title takes the leftover space
                    // and clips inside it; the controls keep their size.
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
                    .child(self.pause_button(theme))
                    .child(div().w(theme.spacing.sm).shrink(0.0))
                    .child(self.interval_switch())
                    .child(div().w(theme.spacing.sm).shrink(0.0))
                    .child(self.appearance_switch())
                    // Last child, so it is recorded after everything the row
                    // draws — though it only ever paints below the row.
                    .child(HeaderShadow {
                        color: theme.shadows.md.color,
                        rise: px(PAGE_HEADER_HEIGHT),
                    }),
            )
            .child(separator(false).bg(Color::TRANSPARENT))
            // Inside the pane rather than at the root: pinned to the window a
            // toast would sit over the status bar, which is chrome.
            .child(self.toasts(theme))
            .child(
                scroll_view().id("content-scroll").flex_1().min_h(px(0.0)).w(relative(1.0)).child(
                    div().flex_row().justify_center().w(relative(1.0)).child(
                        div()
                            .flex_col()
                            .w(relative(1.0))
                            .max_w(px(880.0))
                            .px_(px(28.0))
                            .pb(px(28.0))
                            // The page fades in and rises the last few pixels
                            // as it does. Both halves are a function of one
                            // spring, so they cannot get out of step.
                            .pt(px(24.0) + px(PAGE_RISE) * (1.0 - reveal))
                            .opacity(reveal)
                            .gap(theme.spacing.xl)
                            .child(body),
                    ),
                ),
            )
            .into_element()
    }

    /// Stops the sampling without stopping the window.
    ///
    /// Worth having in a monitor: the list re-sorts under the pointer every
    /// interval, and reading a row that keeps moving is impossible. Paused, the
    /// numbers hold still and everything else — scrolling, sorting, selecting —
    /// still works, because none of that ever depended on the sampler.
    fn pause_button(&self, theme: &Theme) -> AnyElement {
        let state = Rc::clone(&self.state);
        let paused = self.state.paused.get();
        button(if paused { "Resume" } else { "Pause" })
            .id("chrome.pause")
            .variant(if paused { ButtonVariant::Primary } else { ButtonVariant::Outline })
            .height(px(24.0))
            .text_size(theme.typography.xs)
            .on_press(move || {
                state.paused.set(!paused);
                state.say(if paused { "Sampling." } else { "Paused." });
            })
            .into_element()
    }

    /// How often the machine is asked.
    fn interval_switch(&self) -> AnyElement {
        let state = Rc::clone(&self.state);
        segmented(self.state.interval.get())
            .id("chrome.interval")
            .name("Sample interval")
            .h(px(24.0))
            .options(INTERVALS.iter().map(|(_, title)| *title))
            .on_select(move |index| {
                let index = index.min(INTERVALS.len() - 1);
                state.interval.set(index);
                state.say(format!("Sampling every {}.", INTERVALS[index].1));
            })
            .into_element()
    }

    /// The light/dark switch in the pane header.
    fn appearance_switch(&self) -> AnyElement {
        let state = Rc::clone(&self.state);
        let current =
            Appearance::ALL.iter().position(|a| *a == self.state.appearance.get()).unwrap_or(0);
        segmented(current)
            .id("chrome.appearance")
            .name("Appearance")
            .h(px(24.0))
            .options(Appearance::ALL.iter().map(|a| a.title()))
            .on_select(move |index| {
                let next = Appearance::ALL[index.min(Appearance::ALL.len() - 1)];
                state.appearance.set(next);
            })
            .into_element()
    }

    fn status_bar(&self, theme: &Theme) -> AnyElement {
        let c = theme.colors;
        let last = self.state.log.borrow().last().cloned().unwrap_or_default();
        let snap = self.state.snapshot.borrow();
        let summary = format!(
            "CPU {:.0}%  ·  Memory {}%  ·  {} processes  ·  sampled in {:.1} ms",
            snap.cpu,
            snap.memory.load,
            snap.processes.len(),
            snap.sample_ms
        );
        drop(snap);

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
            // Only while sampling is actually running: a spinner that is on
            // screen when nothing is happening teaches the reader to ignore it.
            .child(
                (!self.state.paused.get())
                    .then(|| spinner().id("chrome.sync").size(px(12.0)).thickness(px(1.5))),
            )
            .child(label(last).text_size(theme.typography.xs).text_color(c.text_muted).no_wrap())
            .child(div().flex_1())
            .child(label(summary).text_size(theme.typography.xs).text_color(c.text_muted).no_wrap())
            .into_element()
    }
}

/// The plain-text summary the machine menu puts on the clipboard.
///
/// A monitor whose numbers can only be photographed is half a tool: the reason
/// anyone reads these figures is to paste them into a bug report.
fn report_text(state: &State) -> String {
    let m = &state.machine;
    let snap = state.snapshot.borrow();
    let mut out = String::new();
    out.push_str(&format!("{} — {}\n", m.host, m.user));
    out.push_str(&format!("{} {} (build {})\n", m.os_name.trim(), m.os_release, m.os_build));
    out.push_str(&format!(
        "{} · {} logical / {} physical · {}\n",
        m.cpu_name, m.logical_cores, m.physical_cores, m.arch
    ));
    out.push_str(&format!("Uptime {}\n", sys::duration(snap.uptime)));
    out.push_str(&format!("CPU {:.1}%\n", snap.cpu));
    out.push_str(&format!(
        "Memory {} of {} ({}%)\n",
        sys::bytes(snap.memory.used),
        sys::bytes(snap.memory.total),
        snap.memory.load
    ));
    out.push_str(&format!(
        "Commit {} of {}\n",
        sys::bytes(snap.memory.commit_total),
        sys::bytes(snap.memory.commit_limit)
    ));
    out.push_str(&format!(
        "{} processes, {} threads, {} handles\n",
        snap.memory.processes, snap.memory.threads, snap.memory.handles
    ));
    for disk in &snap.disks {
        out.push_str(&format!(
            "{} {} — {} free of {}\n",
            disk.letter,
            disk.label,
            sys::bytes(disk.free),
            sys::bytes(disk.total)
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

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
/// 10 predecessor and carries the same codepoints for these four glyphs.
pub(crate) const ICON_FONT: [&str; 2] = ["Segoe Fluent Icons", "Segoe MDL2 Assets"];

/// Width and height of a caption button.
const CAPTION_BUTTON: f32 = 32.0;
/// Height of the caption strip. Thirty-two logical pixels is what Windows uses.
const CAPTION_HEIGHT: f32 = 32.0;
/// Size the caption glyphs are drawn at.
const CAPTION_GLYPH: f32 = 10.0;
/// How many caption buttons there are, and therefore how many hover springs.
const CAPTION_BUTTONS: usize = 3;

/// The sidebar's width with its labels showing.
const SIDEBAR_WIDTH: f32 = 210.0;
/// And collapsed. Not a round number chosen by eye: the row is inset by
/// [`NAV_INSET`] and pads itself by the same again, so a 16 px icon sits at
/// `12 + 12 + 8 = 32` from the panel's left edge — the centre of 64 px.
const SIDEBAR_COLLAPSED: f32 = 64.0;
/// How far the navigation rows are inset from the panel's edges.
const NAV_INSET: f32 = 12.0;
/// The same for the footer, whose avatar is wider than an icon.
const FOOTER_INSET: f32 = 8.0;
/// How wide the machine menu is, whatever the sidebar is doing.
const ACCOUNT_MENU_WIDTH: f32 = 190.0;

/// How far a page rises as it arrives, in logical pixels.
const PAGE_RISE: f32 = 14.0;
/// Height of the pane's own header.
const PAGE_HEADER_HEIGHT: f32 = 36.0;
/// How far the page header's shadow reaches down the pane.
const HEADER_SHADOW_BLUR: f32 = 10.0;

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
struct CaptionButton {
    glyph: &'static str,
    command: WindowCommand,
    index: usize,
    state: Rc<State>,
    /// The spring's value this frame, sampled at build time.
    fade: f32,
}

impl CaptionButton {
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
        // floating on the title bar; the shell's are flush to the edge.
        cx.canvas.fill_rect(cx.bounds, background);
        let background = Color::lerp(c.surface, end, self.fade);

        let colour = if danger && self.fade > 0.5 { Color::WHITE } else { c.text };
        let style = icon_text_style(px(CAPTION_GLYPH));
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

    /// Not in the tab order, and drawn with no focus ring — the shell's own
    /// caption buttons are not tab stops either.
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
fn icon_text_style(size: Px) -> spherekit::text::TextStyle {
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

/// The soft edge under the pane's header, separating it from what scrolls.
///
/// One logical pixel tall, painting a shadow that reaches far outside its own
/// box. Both are deliberate. It cannot be a shadow on the header row itself:
/// the row has no background — it is the pane's translucent Mica tint showing
/// through — and a drop shadow is painted *behind* the shape that casts it. And
/// it cannot be a strip as tall as the falloff either, because the topmost node
/// containing a point wins the hit test outright and an eighteen-pixel band
/// would quietly swallow every click along the top of the content.
pub(crate) struct HeaderShadow {
    pub(crate) color: Color,
    /// How tall the caption is: the shape the shadow is cast from.
    pub(crate) rise: Px,
}

impl spherekit::ui::Element for HeaderShadow {
    fn layout_style(&self) -> spherekit::layout::Style {
        let mut style = spherekit::layout::Style::DEFAULT;
        style.position = spherekit::layout::Position::Absolute;
        style.inset.top = spherekit::core::Length::Fraction(1.0);
        style.inset.left = spherekit::core::Length::Px(Px::ZERO);
        style.inset.right = spherekit::core::Length::Px(Px::ZERO);
        style.size.height = spherekit::core::Length::Px(px(1.0));
        style
    }

    fn paint(&mut self, cx: &mut spherekit::ui::PaintContext<'_, '_>) {
        use spherekit::core::{Point, Rect, RoundedRect, Shadow, size};
        let b = cx.bounds;
        if b.width() <= Px::ZERO {
            return;
        }
        let blur = px(HEADER_SHADOW_BLUR);
        let falloff = blur * 2.0 + px(3.0);
        // Clipped to the band below the caption, so the half of the shadow that
        // would fall across the caption itself is never recorded.
        let below = Rect::from_corners(
            Point::new(b.min_x(), b.min_y()),
            Point::new(b.max_x(), b.min_y() + falloff),
        );
        let shadow = Shadow {
            offset: size(Px::ZERO, px(3.0)),
            blur_radius: blur,
            spread: Px::ZERO,
            color: self.color,
            inset: false,
        };
        // The caster is widened by the blur radius at both ends: a shadow cast
        // by a shape that stops at the window edge fades out in the corners,
        // and the caption does not stop at the window edge.
        let caster = Rect::from_corners(
            Point::new(b.min_x() - falloff, b.min_y() - self.rise),
            Point::new(b.max_x() + falloff, b.min_y()),
        );
        cx.canvas.with_save(|canvas| {
            canvas.clip_rect(below);
            canvas.draw_shadow(RoundedRect::uniform(caster, Px::ZERO), &shadow);
        });
    }
}

/// Draws a cached SVG icon, tinted.
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
            // An icon is the one thing in its row that must not be squashed. A
            // flex item shrinks by default, and in a collapsed sidebar the icon
            // is what disappears while the label keeps every pixel of its text.
            flex_shrink: 0.0,
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
/// styled as a control needs this as well, or the keyboard reaches it and can
/// do nothing with it.
pub(crate) fn keyboard_activate(
    mut action: impl FnMut() + 'static,
) -> impl FnMut(&mut EventContext<'_>) -> spherekit::ui::EventFlow + 'static {
    use spherekit::ui::{EventFlow, Key, UiEvent};
    move |cx: &mut EventContext<'_>| {
        let UiEvent::Key(key) = cx.event else { return EventFlow::Continue };
        if key.state.is_pressed() && matches!(key.key, Key::Space | Key::Enter) {
            action();
            cx.notify();
            return EventFlow::Stop;
        }
        EventFlow::Continue
    }
}

/// The chevron on the machine row, drawn rather than shaped.
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
    /// application concern, and the engine should not grow a field for every
    /// asset kind an application might have.
    static ICONS: RefCell<SvgCache> = RefCell::new(SvgCache::new());
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

impl AppHandler for MonitorApp {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        if self.surface.is_some() {
            return;
        }
        // Created hidden on purpose. Bringing up an adapter, a device and a
        // swapchain and then scanning the system fonts takes a few hundred
        // milliseconds, and a window mapped before any of that has run is a
        // blank rectangle for the whole of it.
        let attrs = WindowAttributes::new("SphereKit — System Monitor")
            .with_inner_size(size(px(1040.0), px(680.0)))
            .with_min_inner_size(size(px(620.0), px(400.0)))
            // The header is the title bar. The platform keeps the resize
            // borders, snap, the drop shadow and the window menu; only the
            // caption strip becomes ours to draw.
            .with_chrome(WindowChrome::Custom)
            .with_transparent(true)
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
            SurfaceOptions { transparent: true, ..SurfaceOptions::default() },
        )) {
            Ok(s) => {
                let t = s.init_timing();
                println!("adapter: {}", s.adapter_name());
                println!("init: gpu {:.0} ms, fonts {:.0} ms", t.gpu_ms, t.fonts_ms);
                self.surface = Some(s);
                // Apply Mica after the DX12 DirectComposition surface exists:
                // creating that visual can otherwise replace the composition
                // state attached to the HWND before GPU setup.
                if let Err(error) = window.set_backdrop(WindowBackdrop::Mica) {
                    eprintln!("system backdrop unavailable; using transparent fallback: {error}");
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

        // Seed the shared icon cache with the same documents the sidebar uses.
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
        // Unlike a settings window, a monitor is never idle: the charts scroll
        // whether or not anyone is touching it, and a rate that is only
        // computed when the pointer moves is not a rate. The scheduler paces
        // this against the display rather than spinning.
        cx.scheduler_mut().set_floor(RedrawPolicy::Animating);

        // Paint before the window is mapped, so the first thing the compositor
        // is ever handed is a finished frame.
        let first = std::time::Instant::now();
        self.draw();
        let first_ms = first.elapsed().as_secs_f32() * 1000.0;

        if let Some(window) = self.window.as_ref() {
            let presented = self.surface.as_ref().is_some_and(SphereKitSurface::has_presented);
            if !presented {
                // Reveal anyway. A surface that is not ready at start-up
                // recovers on the next redraw; a window that never appears does
                // not, and an invisible application is the worse failure.
                eprintln!("first frame did not present; showing the window regardless");
            }
            window.set_visible(true);
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
            // A press anywhere dismisses the machine menu unless it landed on
            // the machine UI itself. The footer sets the flag while the press
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
            // process row is consumed, and it should still shut the menu.
            if press && !self.state.press_inside_account.get() && self.state.set_user_menu(false) {
                needs_redraw = true;
            }

            if result.consumed {
                continue;
            }
            if let spherekit::ui::UiEvent::Key(key) = &ui_event
                && key.state.is_pressed()
            {
                match &key.key {
                    // Escape unwinds in the order a reader expects: the newest
                    // thing first, the window last. `||` short-circuits, which
                    // is the point — one press closes the top thing, not both.
                    spherekit::ui::Key::Escape => {
                        let dismissed =
                            self.state.kill_open.replace(false) || self.state.set_user_menu(false);
                        if dismissed {
                            needs_redraw = true;
                        } else {
                            cx.exit();
                        }
                    }
                    // Delete is what Task Manager binds End task to. Routed
                    // through the same confirm dialog the button opens, because
                    // a keystroke that ends a process with no confirmation is
                    // one fumbled keypress away from ending the wrong one.
                    spherekit::ui::Key::Delete => {
                        if let Some(pid) = self.state.selected.get() {
                            let name = self
                                .state
                                .snapshot
                                .borrow()
                                .processes
                                .iter()
                                .find(|p| p.pid == pid)
                                .map(|p| p.name.clone());
                            if let Some(name) = name {
                                self.state.ask_to_end(pid, &name);
                                needs_redraw = true;
                            }
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
        if let Some(limit) = self.frame_limit
            && self.frames >= limit
            && !self.reported
        {
            self.reported = true;
            self.report();
            cx.exit();
        }
    }

    fn exiting(&mut self, _cx: &mut AppContext<'_>) {
        // The GPU surface must die before the window it borrows.
        self.surface = None;
        self.window = None;
    }
}

impl MonitorApp {
    fn report(&self) {
        let stats = self.surface.as_ref().map(|s| s.stats()).unwrap_or_default();
        println!("--- system monitor report ---");
        println!("frames rendered:   {}", self.frames);
        println!("draw calls:        {}", stats.frame.draw_calls);
        println!("elements built:    {}", stats.tree.elements);
        println!("cpu this frame:    {:.3} ms", stats.cpu_ms);
        println!("sample cost:       {:.3} ms", self.state.snapshot.borrow().sample_ms);
    }

    /// Pushes the current theme to the surface, if it has changed.
    fn sync_theme(&mut self) {
        let theme = self.state.theme();
        if self.applied_theme.as_ref() == Some(&theme) {
            return;
        }
        if let Some(surface) = self.surface.as_mut() {
            surface.set_theme(theme.clone());
        }
        // The window's own material has to move with us. Everything outside the
        // opaque content pane is DWM Mica showing through, and Mica has a light
        // and a dark variant the operating system was choosing.
        if let Some(window) = self.window.as_ref() {
            window.set_preferred_theme(Some(self.state.effective_theme()));
        }
        self.applied_theme = Some(theme);
    }

    /// Notices a page change once, and starts everything that follows from it.
    fn sync_page(&mut self) {
        let page = self.state.page.get();
        if self.shown_page == Some(page) {
            return;
        }
        self.shown_page = Some(page);
        // From zero, not from wherever the last transition had got to.
        self.state.page_in.set(Motion::at(0.0, Drive::SMOOTH));
        // And put the pane back at the top. Without this, switching away from
        // the bottom of a long process list lands the reader half-way down a
        // page they have never seen.
        if let Some(surface) = self.surface.as_mut() {
            surface.tree_mut().scroll_element_to("content-scroll", size(Px::ZERO, Px::ZERO));
        }
    }

    fn draw(&mut self) {
        self.maybe_sample();
        self.sync_page();
        self.sync_theme();
        self.advance_motion();
        let root = self.build();
        let Some(surface) = self.surface.as_mut() else { return };
        match surface.render(root, Color::TRANSPARENT) {
            Ok(Some(_)) => self.frames += 1,
            Ok(None) => {}
            Err(e) => eprintln!("frame failed: {e}"),
        }
        self.apply_ime();
        self.publish_caption();
    }

    /// Performs whatever the caption asked for, if anything.
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
    /// out by flexbox and move with the window's width.
    ///
    /// The exclusions are **not** listed by hand. Every interactive widget
    /// declares itself during paint through `PaintContext::keep_interactive`,
    /// so a button added to the header later is clickable without anyone
    /// remembering this function exists.
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
    fn advance_motion(&mut self) {
        let frame = self.timeline.advance_to(self.clock.now());
        for i in 0..CAPTION_BUTTONS {
            let mut motion = self.state.wco_fade[i].get();
            motion.retarget(if self.state.wco_hovered[i].get() { 1.0 } else { 0.0 });
            motion.step(frame.delta);
            self.state.wco_fade[i].set(motion);
        }

        // Nothing here calls `notify_layout`: a panel's open amount is a style
        // value, and `UiTree::build` marks a node layout-dirty exactly when its
        // style changed, so animating the style *is* the invalidation.
        for (flag, motion) in [
            (self.state.user_menu_open.get(), &self.state.user_menu),
            (self.state.sidebar_open.get(), &self.state.sidebar),
            (self.state.kill_open.get(), &self.state.kill),
        ] {
            let mut m = motion.get();
            m.retarget(if flag { 1.0 } else { 0.0 });
            m.step(frame.delta);
            motion.set(m);
        }

        // The page transition. Retargeted to one every frame and reset to zero
        // by `sync_page`, so a page change is the only thing that starts it.
        let mut page_in = self.state.page_in.get();
        page_in.retarget(1.0);
        page_in.step(frame.delta);
        self.state.page_in.set(page_in);

        self.advance_toasts(frame.delta);
    }

    /// Ages the toast list by one frame.
    ///
    /// Three jobs in one pass, because they have to happen in this order: raise
    /// anything a callback queued, retire anything whose time is up, and drop
    /// anything whose spring has finished leaving. Dropping before stepping
    /// would cut the exit animation off at its first frame.
    fn advance_toasts(&mut self, delta: Duration) {
        let now = self.started.elapsed().as_secs_f32();
        if let Some((variant, title, message)) = self.state.pending_toast.borrow_mut().take() {
            self.state.push_toast(now, variant, title, message);
        }

        let mut toasts = self.state.toasts.borrow_mut();
        for entry in toasts.iter_mut() {
            if !entry.leaving && now - entry.born > TOAST_LIFETIME {
                entry.leaving = true;
            }
            entry.fade.retarget(if entry.leaving { 0.0 } else { 1.0 });
            entry.fade.step(delta);
        }
        // A toast that has finished leaving is gone; one that has not is still
        // on screen at whatever opacity its spring says.
        toasts.retain(|t| !(t.leaving && t.fade.is_settled()));
    }

    /// Tells the window whether to compose, and where.
    ///
    /// Read after rendering, because the caret's position is a paint-time fact:
    /// a text field can only say where its caret is once it has laid its string
    /// out, and it lays it out while painting. The state is diffed rather than
    /// pushed every frame, because a platform is entitled to treat re-enabling
    /// an input method as a reason to cancel the composition in progress.
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
    if let Err(e) = App::new(MonitorApp::new()).run() {
        eprintln!("event loop failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit::text::TextSystem;
    use spherekit::ui::UiTree;

    /// Builds and lays out one page, returning the tree it produced.
    ///
    /// One real sample first, so the pages are laid out against the numbers
    /// they will actually show. A snapshot of zeroes is exactly the case a list
    /// bug hides in: every row is absent, so nothing can be measured wrong.
    fn lay_out(page: Page, text: &mut TextSystem) -> (UiTree, spherekit::core::Size<Px>) {
        let mut app = MonitorApp::new();
        app.state.page.set(page);
        app.maybe_sample();
        let viewport = size(px(1100.0), px(720.0));
        let mut tree = UiTree::new();
        tree.set_theme(app.state.theme());
        tree.build(app.build());
        tree.compute_layout_with_text(viewport, text).unwrap();
        (tree, viewport)
    }

    #[test]
    fn every_page_builds_and_lays_out() {
        let mut text = TextSystem::with_system_fonts();
        for page in Page::ALL {
            let (tree, viewport) = lay_out(page, &mut text);
            let status = tree
                .bounds_of("chrome.status")
                .unwrap_or_else(|| panic!("{} lost the status bar", page.title()));
            // A pixel of slack: flexbox distributes leftover space in floating
            // point, so a column that exactly fills the viewport lands a
            // fraction past it and that is not the bug this is looking for.
            assert!(
                status.max_y() <= viewport.height + px(1.0),
                "{} pushed the status bar off the bottom: {status:?}",
                page.title()
            );
            let header = tree
                .bounds_of("chrome.header")
                .unwrap_or_else(|| panic!("{} lost the caption", page.title()));
            // The caption is the one strip that must never be squeezed: it is
            // what the platform is told to treat as a title bar.
            assert_eq!(
                header.height(),
                px(CAPTION_HEIGHT),
                "{} squashed the caption",
                page.title()
            );
        }
    }

    #[test]
    fn the_process_list_draws_rows_for_what_it_sampled() {
        let mut text = TextSystem::with_system_fonts();
        let mut app = MonitorApp::new();
        app.state.page.set(Page::Processes);
        app.maybe_sample();
        let expected = app.state.visible_processes();
        if expected.is_empty() {
            // A non-Windows build has nothing to enumerate, and an empty list
            // is the documented answer there rather than a failure.
            return;
        }

        let mut tree = UiTree::new();
        tree.set_theme(app.state.theme());
        tree.build(app.build());
        tree.compute_layout_with_text(size(px(1100.0), px(720.0)), &mut text).unwrap();

        for proc in expected.iter().take(MAX_ROWS_PROBE) {
            let id = ("proc.row", proc.pid);
            assert!(
                tree.bounds_of(id).is_some(),
                "{} ({}) is in the list but has no row",
                proc.name,
                proc.pid
            );
        }
    }

    /// How many of the sorted rows the test above checks. The page caps what it
    /// draws; this stays well inside that cap so the test is about the rows
    /// being built, not about where the cap falls.
    const MAX_ROWS_PROBE: usize = 20;

    #[test]
    fn sorting_a_column_twice_reverses_it() {
        let state = State::new(sys::machine_info());
        state.sort_by(SortKey::Memory);
        assert!(state.sort_desc.get(), "a measurement column opens largest-first");
        state.sort_by(SortKey::Memory);
        assert!(!state.sort_desc.get(), "the same column again reverses");
        state.sort_by(SortKey::Name);
        assert!(!state.sort_desc.get(), "a name column opens A to Z");
    }

    #[test]
    fn the_sort_is_total_so_a_still_list_does_not_shiver() {
        // Two processes with identical CPU must not swap places between
        // samples. The tiebreak is the pid, which never changes while a process
        // lives, so the same input always produces the same order.
        let state = State::new(sys::machine_info());
        state.sort.set(SortKey::Cpu);
        {
            let mut snap = state.snapshot.borrow_mut();
            for pid in [40u32, 10, 30, 20] {
                snap.processes.push(sys::ProcInfo {
                    pid,
                    name: format!("idle{pid}"),
                    ..Default::default()
                });
            }
        }
        let order: Vec<u32> = state.visible_processes().iter().map(|p| p.pid).collect();
        assert_eq!(order, vec![10, 20, 30, 40], "equal rows are not in a stable order");
    }

    #[test]
    fn the_filter_matches_a_name_or_a_pid() {
        let state = State::new(sys::machine_info());
        {
            let mut snap = state.snapshot.borrow_mut();
            snap.processes.push(sys::ProcInfo {
                pid: 4,
                name: "System".into(),
                ..Default::default()
            });
            snap.processes.push(sys::ProcInfo {
                pid: 1234,
                name: "explorer.exe".into(),
                ..Default::default()
            });
        }
        *state.filter.borrow_mut() = TextEdit::from_text("EXPLORER");
        assert_eq!(state.visible_processes().len(), 1, "the filter is case-sensitive");
        *state.filter.borrow_mut() = TextEdit::from_text("123");
        assert_eq!(state.visible_processes().len(), 1, "a pid substring does not match");
    }

    #[test]
    fn history_never_grows_past_its_window() {
        let mut history = History::default();
        let snap = Snapshot {
            memory: sys::MemInfo { total: 1024, used: 512, ..Default::default() },
            ..Default::default()
        };
        for _ in 0..HISTORY * 2 {
            history.record(&snap);
        }
        assert_eq!(history.cpu.len(), HISTORY);
        assert_eq!(history.memory.len(), HISTORY);
        assert_eq!(history.memory[0], 0.5);
    }

    #[test]
    fn the_network_axis_has_a_floor() {
        // An adapter doing nothing must not have its own rounding noise drawn
        // as a mountain range, which is what a scale fitted to a near-zero
        // peak would produce.
        let history = History { rx: vec![0.0, 1.0, 0.0], ..Default::default() };
        assert!(history.network_scale() >= 64.0 * 1024.0);
    }

    #[test]
    fn byte_formatting_uses_binary_units() {
        assert_eq!(sys::bytes(0), "0 B");
        assert_eq!(sys::bytes(1024), "1 KB");
        assert_eq!(sys::bytes(16 * 1024 * 1024 * 1024), "16.0 GB");
        assert_eq!(sys::duration(90), "00:01:30");
        assert_eq!(sys::duration(90_061), "1d 01:01:01");
    }
}
