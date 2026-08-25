//! The pages: one per thing the machine can run short of.
//!
//! Every number on every page came out of [`crate::sys`] during the last
//! sample, and nothing here caches, smooths or interpolates it. A monitor that
//! made its numbers prettier than they are would be lying about the one thing
//! it exists to report.
//!
//! The pages share three habits. A **card** is a bordered block with a heading
//! and a note saying what the figure underneath it actually means, because
//! "Commit 24.1 GB" is not information until you know what commit is. A
//! **readout row** is a label and a value with the label at a fixed width, so
//! a column of them lines up without a table. And every chart is drawn against
//! a fixed axis rather than a fitted one — see [`crate::graph::Graph`].

use std::rc::Rc;

use spherekit::core::{Color, Px, px};
use spherekit::ui::{
    AnyElement, BadgeVariant, ButtonVariant, Cursor, EventContext, Interactive, IntoElement,
    ParentElement, Role, Semantics, Styled, StyledInteraction, Theme, badge, button, div, label,
    separator, text_field,
};

use crate::graph::{Graph, Meter};
use crate::sys::{self, ProcInfo};
use crate::{HISTORY, History, SortKey, State};

/// The pages the sidebar navigates between.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Overview,
    Processes,
    Cpu,
    Memory,
    Disks,
    Network,
    System,
}

impl Page {
    pub(crate) const ALL: [Page; 7] = [
        Page::Overview,
        Page::Processes,
        Page::Cpu,
        Page::Memory,
        Page::Disks,
        Page::Network,
        Page::System,
    ];

    /// Matches a page by its title, case-insensitively, for
    /// `SPHEREKIT_MONITOR_PAGE`. A monitor is a thing people screenshot, and a
    /// screenshot needs a way to be taken twice.
    pub(crate) fn from_name(name: &str) -> Option<Page> {
        let name = name.trim();
        Page::ALL.into_iter().find(|p| p.title().eq_ignore_ascii_case(name))
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Page::Overview => "Overview",
            Page::Processes => "Processes",
            Page::Cpu => "CPU",
            Page::Memory => "Memory",
            Page::Disks => "Disks",
            Page::Network => "Network",
            Page::System => "System",
        }
    }

    /// The one-line summary under the page title.
    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Page::Overview => "What the machine is doing right now, in one screen.",
            Page::Processes => "Everything running, and the one control here that changes it.",
            Page::Cpu => "Two minutes of processor history, and what the kernel counts.",
            Page::Memory => "Physical memory, the commit charge, and the kernel's own pools.",
            Page::Disks => "Every mounted volume and how much of it is left.",
            Page::Network => "Per-adapter throughput, differenced from the octet counters.",
            Page::System => "The machine itself: what it is, and how long it has been up.",
        }
    }

    /// A monochrome icon, tinted by the theme at draw time.
    ///
    /// Stroked rather than filled, like the rest of the shell's icons: a
    /// stroked path is the shape most likely to look jagged without
    /// multisampling, so it is the honest thing to put in a sidebar.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            Page::Overview => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round">
                     <rect x="3" y="3.5" width="8" height="8" rx="2"/>
                     <rect x="13" y="3.5" width="8" height="5" rx="2"/>
                     <rect x="3" y="13.5" width="8" height="7" rx="2"/>
                     <rect x="13" y="10.5" width="8" height="10" rx="2"/></svg>"##
            }
            Page::Processes => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round">
                     <path d="M4 6.5h16M4 12h16M4 17.5h16"/>
                     <circle cx="8.5" cy="6.5" r="1.6"/>
                     <circle cx="15" cy="12" r="1.6"/>
                     <circle cx="10.5" cy="17.5" r="1.6"/></svg>"##
            }
            Page::Cpu => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round"
                     stroke-linecap="round">
                     <rect x="6.5" y="6.5" width="11" height="11" rx="2.5"/>
                     <rect x="10" y="10" width="4" height="4" rx="1"/>
                     <path d="M9.5 3v3.5M14.5 3v3.5M9.5 17.5V21M14.5 17.5V21
                              M3 9.5h3.5M3 14.5h3.5M17.5 9.5H21M17.5 14.5H21"/></svg>"##
            }
            Page::Memory => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round"
                     stroke-linecap="round">
                     <rect x="2.5" y="7" width="19" height="10" rx="2.5"/>
                     <path d="M7 17v3M12 17v3M17 17v3M7 10.5v3M12 10.5v3M17 10.5v3"/></svg>"##
            }
            Page::Disks => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round">
                     <ellipse cx="12" cy="6.5" rx="8" ry="3.2"/>
                     <path d="M4 6.5v11c0 1.8 3.6 3.2 8 3.2s8-1.4 8-3.2v-11"/>
                     <path d="M4 12c0 1.8 3.6 3.2 8 3.2s8-1.4 8-3.2"/></svg>"##
            }
            Page::Network => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round"
                     stroke-linecap="round">
                     <circle cx="12" cy="12" r="8.5"/>
                     <path d="M3.5 12h17"/>
                     <path d="M12 3.5c2.4 2.4 3.6 5.3 3.6 8.5S14.4 18.1 12 20.5
                              C9.6 18.1 8.4 15.2 8.4 12S9.6 5.9 12 3.5z"/></svg>"##
            }
            Page::System => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round"
                     stroke-linecap="round">
                     <rect x="2.5" y="4.5" width="19" height="13" rx="2.5"/>
                     <path d="M8 21h8M12 17.5V21"/></svg>"##
            }
        }
    }
}

/// Renders the body of a page.
pub(crate) fn render(page: Page, state: &Rc<State>, theme: &Theme) -> AnyElement {
    match page {
        Page::Overview => overview(state, theme),
        Page::Processes => processes(state, theme),
        Page::Cpu => cpu(state, theme),
        Page::Memory => memory(state, theme),
        Page::Disks => disks(state, theme),
        Page::Network => network(state, theme),
        Page::System => system(state, theme),
    }
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

/// The landing page: the four resources, each with its chart or its meter.
///
/// Deliberately not a summary of the other pages. Somebody who opens a task
/// manager has already noticed something is wrong and wants to know *which*
/// resource — so the whole page is four answers to that one question, and every
/// tile is a way through to the page that expands it.
fn overview(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let snap = state.snapshot.borrow();
    let history = state.history.borrow();
    let net_scale = history.network_scale();

    let busiest_disk = snap
        .disks
        .iter()
        .filter(|d| d.kind == "Fixed")
        .max_by(|a, b| {
            a.used_fraction().partial_cmp(&b.used_fraction()).unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned();

    let mut grid = wrapping_row(theme);
    grid = grid
        .child(tile(
            state,
            theme,
            Page::Cpu,
            "CPU",
            &format!("{:.0}%", snap.cpu),
            &state.machine.cpu_name,
            Graph::new(history.cpu.clone(), HISTORY, c.accent, px(70.0)).into_element(),
        ))
        .child(tile(
            state,
            theme,
            Page::Memory,
            "Memory",
            &format!("{}%", snap.memory.load),
            &format!(
                "{} of {} in use",
                sys::bytes(snap.memory.used),
                sys::bytes(snap.memory.total)
            ),
            Graph::new(history.memory.clone(), HISTORY, c.success, px(70.0)).into_element(),
        ))
        .child(tile(
            state,
            theme,
            Page::Network,
            "Network",
            &sys::rate(snap.rx_rate()),
            &format!("{} sent", sys::rate(snap.tx_rate())),
            // Both directions on one axis, so the ratio between them is
            // readable. Two charts with independent scales would make a
            // trickle of uploads look like a flood.
            div()
                .flex_col()
                .gap(px(2.0))
                .child(
                    Graph::new(
                        History::scaled(&history.rx, net_scale),
                        HISTORY,
                        c.accent,
                        px(32.0),
                    )
                    .into_element(),
                )
                .child(
                    Graph::new(
                        History::scaled(&history.tx, net_scale),
                        HISTORY,
                        c.warning,
                        px(32.0),
                    )
                    .into_element(),
                )
                .into_element(),
        ))
        .child(tile(
            state,
            theme,
            Page::Disks,
            "Disk",
            &busiest_disk
                .as_ref()
                .map(|d| format!("{:.0}%", d.used_fraction() * 100.0))
                .unwrap_or_else(|| "—".into()),
            &busiest_disk
                .as_ref()
                .map(|d| format!("{} — {} free", d.letter, sys::bytes(d.free)))
                .unwrap_or_else(|| "no fixed volume".into()),
            div()
                .flex_col()
                .justify_center()
                .h(px(70.0))
                .gap(theme.spacing.sm)
                .child(
                    Meter::new(
                        busiest_disk.as_ref().map(|d| d.used_fraction()).unwrap_or(0.0),
                        c.elevated,
                        pressure_color(
                            theme,
                            busiest_disk.as_ref().map(|d| d.used_fraction()).unwrap_or(0.0),
                        ),
                    )
                    .height(px(8.0))
                    .into_element(),
                )
                .into_element(),
        ));

    // The five processes doing the most, which is the answer to the question
    // that brought anyone here. Read from the same sorted list the Processes
    // page uses, so the two can never disagree about what "busiest" means.
    let mut busiest: Vec<ProcInfo> = snap.processes.clone();
    busiest.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
    busiest.truncate(5);

    let mut top = div().flex_col().gap(px(1.0));
    for proc in &busiest {
        top = top.child(
            div()
                .flex_row()
                .items_center()
                .gap(theme.spacing.md)
                .py_(theme.spacing.xs)
                .child(
                    label(proc.name.clone())
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(theme.typography.sm)
                        .text_color(c.text)
                        .truncate(),
                )
                .child(
                    label(format!("{:.1}%", proc.cpu))
                        .w(px(56.0))
                        .text_size(theme.typography.sm)
                        .weight(theme.typography.strong)
                        .text_color(if proc.cpu > 10.0 { c.warning } else { c.text_muted })
                        .no_wrap(),
                )
                .child(
                    label(sys::bytes(proc.working_set))
                        .w(px(76.0))
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted)
                        .no_wrap(),
                ),
        );
    }

    drop(history);
    drop(snap);

    page(theme, Page::Overview)
        .child(grid)
        .child(section(
            theme,
            "Busiest processes",
            "By share of the whole machine over the last interval — not of one core.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .child(top)
                .child({
                    let s = Rc::clone(state);
                    button("Open the process list")
                        .id("overview.processes")
                        .variant(ButtonVariant::Outline)
                        .on_press(move || s.page.set(Page::Processes))
                })
                .into_element(),
        ))
        .into_element()
}

/// One resource, as a card that navigates to the page about it.
fn tile(
    state: &Rc<State>,
    theme: &Theme,
    page: Page,
    name: &str,
    value: &str,
    note: &str,
    body: AnyElement,
) -> AnyElement {
    let c = theme.colors;
    let s = Rc::clone(state);
    let go = move || s.page.set(page);
    div()
        .id(("overview.tile", page.title()))
        .focusable()
        .flex_col()
        .w(px(392.0))
        .gap(theme.spacing.sm)
        .p(theme.spacing.md)
        .rounded(theme.radii.lg)
        .bg(c.surface)
        .border(px(1.0), c.border)
        .hover_bg(c.hover)
        .active_bg(c.pressed)
        .cursor(Cursor::Pointer)
        .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
        .semantics(Semantics::new(Role::Button, page.title()))
        .child(
            div()
                .flex_row()
                .items_center()
                .gap(theme.spacing.sm)
                .child(
                    label(name.to_string())
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(theme.typography.sm)
                        .weight(theme.typography.strong)
                        .text_color(c.text)
                        .no_wrap(),
                )
                .child(
                    label(value.to_string())
                        .text_size(theme.typography.lg)
                        .weight(theme.typography.strong)
                        .text_color(c.text)
                        .no_wrap(),
                ),
        )
        .child(
            label(note.to_string())
                .text_size(theme.typography.xs)
                .text_color(c.text_muted)
                .truncate(),
        )
        .child(body)
        .on_click({
            let go = go.clone();
            move |cx: &mut EventContext<'_>| {
                go();
                cx.notify_layout();
            }
        })
        .on_key(crate::keyboard_activate(go))
        .into_element()
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// How many rows are drawn at once.
///
/// A running system has three or four hundred processes and the tree is rebuilt
/// every frame, so drawing all of them would spend most of a frame on rows
/// nobody has scrolled to. Sorted and filtered first, the cap is a window onto
/// the answer rather than a truncation of it — and the header says so, because
/// a list that silently stops is a list that has lied.
const MAX_ROWS: usize = 200;

fn processes(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let list = state.visible_processes();
    let total = state.snapshot.borrow().processes.len();
    let selected = state.selected.get();
    let shown = list.len().min(MAX_ROWS);

    // --- the controls -------------------------------------------------------
    let controls = div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.md)
        .child({
            let s = Rc::clone(state);
            text_field(state.filter.borrow().clone())
                .id("proc.filter")
                .placeholder("filter by name or pid")
                .w(px(240.0))
                .on_change(move |edit| *s.filter.borrow_mut() = edit.clone())
        })
        .child(
            label(if list.len() == total {
                format!("{total} processes")
            } else {
                format!("{} of {total} processes", list.len())
            })
            .text_size(theme.typography.sm)
            .text_color(c.text_muted)
            .no_wrap(),
        )
        .child(div().flex_1())
        .child({
            let s = Rc::clone(state);
            // Disabled rather than hidden: a control that appears when you
            // select something is a control you have to discover twice.
            let target = selected.and_then(|pid| {
                list.iter().find(|p| p.pid == pid).map(|p| (p.pid, p.name.clone()))
            });
            button("End task")
                .id("proc.end")
                .variant(ButtonVariant::Danger)
                .disabled(target.is_none())
                .on_press(move || {
                    if let Some((pid, name)) = target.clone() {
                        s.ask_to_end(pid, &name);
                    }
                })
        });

    // --- the column headings ------------------------------------------------
    let mut headings = div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.sm)
        .px_(theme.spacing.sm)
        .py_(theme.spacing.xs)
        // Tinted rather than ruled: a `div` has one border for all four sides,
        // and the separator below carries the line the heading row needs.
        .bg(c.elevated);
    for key in SortKey::ALL {
        headings = headings.child(column_heading(state, theme, key));
    }

    // --- the rows -----------------------------------------------------------
    let mut rows = div().flex_col();
    for proc in list.iter().take(MAX_ROWS) {
        rows = rows.child(process_row(state, theme, proc, selected == Some(proc.pid)));
    }
    if list.is_empty() {
        rows = rows.child(
            label("Nothing matches that filter.")
                .p(theme.spacing.lg)
                .text_size(theme.typography.sm)
                .text_color(c.text_muted),
        );
    }

    page(theme, Page::Processes)
        .child(controls)
        .child(
            div()
                .flex_col()
                .rounded(theme.radii.lg)
                .bg(c.surface)
                .border(px(1.0), c.border)
                .overflow_hidden()
                .child(headings)
                .child(separator(false).bg(c.border))
                .child(rows),
        )
        .child(
            label(if shown < list.len() {
                format!(
                    "Showing the first {shown} of {} matching rows. Sort or filter to bring the \
                     rest into view.",
                    list.len()
                )
            } else {
                "Select a row and press Delete, or use End task. Neither can be undone.".to_string()
            })
            .text_size(theme.typography.xs)
            .text_color(c.text_muted),
        )
        .into_element()
}

/// The width of every column but the first, which takes what is left.
fn column_width(key: SortKey) -> Px {
    match key {
        SortKey::Name => px(0.0),
        SortKey::Cpu => px(64.0),
        SortKey::Memory => px(88.0),
        SortKey::Pid => px(64.0),
        SortKey::Threads => px(64.0),
    }
}

/// One clickable column heading, with its sort arrow.
fn column_heading(state: &Rc<State>, theme: &Theme, key: SortKey) -> AnyElement {
    let c = theme.colors;
    let active = state.sort.get() == key;
    let descending = state.sort_desc.get();
    let s = Rc::clone(state);
    let sort = move || s.sort_by(key);

    let text = format!(
        "{}{}",
        key.title(),
        // A caret rather than a rotating chevron: the heading is 10 px of text
        // and a drawn glyph beside it at that size is a smudge.
        match (active, descending) {
            (true, true) => " \u{25BE}",
            (true, false) => " \u{25B4}",
            _ => "",
        }
    );

    let mut heading = div()
        .id(("proc.sort", key.title()))
        .focusable()
        .flex_row()
        .items_center()
        .px_(theme.spacing.xs)
        .rounded(theme.radii.sm)
        .hover_bg(c.hover)
        .active_bg(c.pressed)
        .cursor(Cursor::Pointer)
        .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
        .semantics(Semantics::new(Role::Button, key.title()))
        .child(
            label(text)
                .text_size(theme.typography.xs)
                .weight(theme.typography.strong)
                .text_color(if active { c.accent } else { c.text_muted })
                .no_wrap(),
        )
        .on_click({
            let sort = sort.clone();
            move |cx: &mut EventContext<'_>| {
                sort();
                cx.notify_layout();
            }
        })
        .on_key(crate::keyboard_activate(sort));

    heading = if key == SortKey::Name {
        heading.flex_1().min_w(px(0.0))
    } else {
        heading.w(column_width(key)).shrink(0.0)
    };
    heading.into_element()
}

/// One process.
fn process_row(state: &Rc<State>, theme: &Theme, proc: &ProcInfo, selected: bool) -> AnyElement {
    let c = theme.colors;
    let s = Rc::clone(state);
    let pid = proc.pid;
    let pick = move || s.selected.set(Some(pid));

    // Only the busy ones are coloured. A column where every cell is tinted is a
    // column with no signal in it; the point of the warning colour is that it
    // is rare enough to find by eye.
    let cpu_color = match proc.cpu {
        v if v >= 25.0 => c.danger,
        v if v >= 5.0 => c.warning,
        _ => c.text_muted,
    };

    div()
        .id(("proc.row", pid))
        .focusable()
        .flex_row()
        .items_center()
        .gap(theme.spacing.sm)
        .h(px(26.0))
        .shrink(0.0)
        .px_(theme.spacing.sm)
        .bg(if selected { c.accent.with_alpha(0.18) } else { Color::TRANSPARENT })
        .hover_bg(c.hover)
        .active_bg(c.pressed)
        .cursor(Cursor::Pointer)
        .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
        .semantics(Semantics::new(Role::Button, proc.name.clone()))
        .child(
            div()
                .flex_row()
                .items_center()
                .gap(theme.spacing.xs)
                .flex_1()
                .min_w(px(0.0))
                .child(
                    label(proc.name.clone())
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(theme.typography.sm)
                        .text_color(c.text)
                        .truncate(),
                )
                // Said out loud rather than left as a suspicious zero: a
                // protected process reads as perfectly idle otherwise, and that
                // is the one row somebody investigating a busy machine would
                // most like to be told about.
                .child(proc.restricted.then(|| badge("protected").variant(BadgeVariant::Neutral))),
        )
        .child(
            label(if proc.cpu >= 0.05 { format!("{:.1}", proc.cpu) } else { "—".into() })
                .w(column_width(SortKey::Cpu))
                .shrink(0.0)
                .text_size(theme.typography.sm)
                .text_color(cpu_color)
                .no_wrap(),
        )
        .child(
            label(sys::bytes(proc.working_set))
                .w(column_width(SortKey::Memory))
                .shrink(0.0)
                .text_size(theme.typography.sm)
                .text_color(c.text_muted)
                .no_wrap(),
        )
        .child(
            label(pid.to_string())
                .w(column_width(SortKey::Pid))
                .shrink(0.0)
                .text_size(theme.typography.sm)
                .text_color(c.text_muted)
                .no_wrap(),
        )
        .child(
            label(proc.threads.to_string())
                .w(column_width(SortKey::Threads))
                .shrink(0.0)
                .text_size(theme.typography.sm)
                .text_color(c.text_muted)
                .no_wrap(),
        )
        .on_click({
            let pick = pick.clone();
            move |cx: &mut EventContext<'_>| {
                pick();
                cx.notify();
            }
        })
        .on_key(crate::keyboard_activate(pick))
        .into_element()
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

fn cpu(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let m = &state.machine;
    let snap = state.snapshot.borrow();
    let history = state.history.borrow();
    let chart = Graph::new(history.cpu.clone(), HISTORY, c.accent, px(180.0)).into_element();
    let busiest = history.cpu.iter().copied().fold(0.0f32, f32::max) * 100.0;
    let average = if history.cpu.is_empty() {
        0.0
    } else {
        history.cpu.iter().sum::<f32>() / history.cpu.len() as f32 * 100.0
    };
    let cpu_now = snap.cpu;
    let processes = snap.memory.processes;
    let threads = snap.memory.threads;
    let handles = snap.memory.handles;
    let uptime = snap.uptime;
    drop(history);
    drop(snap);

    page(theme, Page::Cpu)
        .child(
            div()
                .flex_row()
                .gap(theme.spacing.md)
                .child(stat(theme, "In use", &format!("{cpu_now:.1}%")))
                .child(stat(theme, "Peak, 2 min", &format!("{busiest:.0}%")))
                .child(stat(theme, "Mean, 2 min", &format!("{average:.0}%")))
                .child(stat(theme, "Up", &sys::duration(uptime))),
        )
        .child(section(
            theme,
            "History",
            "The axis is 0 to 100% and does not rescale. A chart fitted to its own maximum \
             makes an idle machine look identical to a saturated one.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .p(theme.spacing.md)
                .rounded(theme.radii.lg)
                .bg(c.surface)
                .border(px(1.0), c.border)
                .child(chart)
                .child(
                    div()
                        .flex_row()
                        .child(
                            label(format!(
                                "{} seconds ago",
                                HISTORY as f32 * state.interval_secs()
                            ))
                            .flex_1()
                            .text_size(theme.typography.xs)
                            .text_color(c.text_muted),
                        )
                        .child(
                            label("now")
                                .text_size(theme.typography.xs)
                                .text_color(c.text_muted)
                                .no_wrap(),
                        ),
                )
                .into_element(),
        ))
        .child(section(
            theme,
            "The processor",
            "From GetNativeSystemInfo and the registry, which is where the marketing name \
             actually lives.",
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(info_row(theme, "Model", &m.cpu_name))
                .child(info_row(theme, "Logical processors", &m.logical_cores.to_string()))
                .child(info_row(
                    theme,
                    "Physical cores",
                    &if m.physical_cores > 0 {
                        m.physical_cores.to_string()
                    } else {
                        "not reported".into()
                    },
                ))
                .child(info_row(theme, "Architecture", m.arch))
                .into_element(),
        ))
        .child(section(
            theme,
            "What the kernel is holding",
            "Counted by GetPerformanceInfo in one call, which is why these three always agree \
             with each other.",
            div()
                .flex_row()
                .gap(theme.spacing.md)
                .child(stat(theme, "Processes", &processes.to_string()))
                .child(stat(theme, "Threads", &threads.to_string()))
                .child(stat(theme, "Handles", &handles.to_string()))
                .into_element(),
        ))
        .into_element()
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

fn memory(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let snap = state.snapshot.borrow();
    let mem = snap.memory.clone();
    let history = state.history.borrow();
    let chart = Graph::new(history.memory.clone(), HISTORY, c.success, px(160.0))
        .fill(c.success)
        .into_element();
    drop(history);
    drop(snap);

    let physical = if mem.total > 0 { mem.used as f32 / mem.total as f32 } else { 0.0 };
    let commit =
        if mem.commit_limit > 0 { mem.commit_total as f32 / mem.commit_limit as f32 } else { 0.0 };

    page(theme, Page::Memory)
        .child(
            div()
                .flex_row()
                .gap(theme.spacing.md)
                .child(stat(theme, "In use", &sys::bytes(mem.used)))
                .child(stat(theme, "Available", &sys::bytes(mem.available)))
                .child(stat(theme, "Cached", &sys::bytes(mem.cached)))
                .child(stat(theme, "Installed", &sys::bytes(mem.total))),
        )
        .child(section(
            theme,
            "Physical memory",
            "What is resident right now. Cached pages count as in use and are given back the \
             moment something asks for them, which is why a healthy machine reads as full.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .p(theme.spacing.md)
                .rounded(theme.radii.lg)
                .bg(c.surface)
                .border(px(1.0), c.border)
                .child(chart)
                .child(
                    Meter::new(physical, c.elevated, pressure_color(theme, physical))
                        .height(px(8.0))
                        .into_element(),
                )
                .child(
                    label(format!(
                        "{} of {} — {}%",
                        sys::bytes(mem.used),
                        sys::bytes(mem.total),
                        mem.load
                    ))
                    .text_size(theme.typography.xs)
                    .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .child(section(
            theme,
            "Commit charge",
            "Memory the system has promised, whether or not anything has touched it yet. This \
             is the number that runs out first, and the one an out-of-memory failure is about.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .p(theme.spacing.md)
                .rounded(theme.radii.lg)
                .bg(c.surface)
                .border(px(1.0), c.border)
                .child(
                    Meter::new(commit, c.elevated, pressure_color(theme, commit))
                        .height(px(8.0))
                        .into_element(),
                )
                .child(
                    label(format!(
                        "{} of {}",
                        sys::bytes(mem.commit_total),
                        sys::bytes(mem.commit_limit)
                    ))
                    .text_size(theme.typography.xs)
                    .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .child(section(
            theme,
            "Kernel pools",
            "The kernel's own allocations. Non-paged pool can never be swapped out, so a leak \
             there consumes physical memory that nothing else can reclaim.",
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(info_row(theme, "Paged pool", &sys::bytes(mem.paged_pool)))
                .child(info_row(theme, "Non-paged pool", &sys::bytes(mem.nonpaged_pool)))
                .child(info_row(theme, "System cache", &sys::bytes(mem.cached)))
                .child(info_row(theme, "Page size", &sys::bytes(state.machine.page_size)))
                .into_element(),
        ))
        .into_element()
}

// ---------------------------------------------------------------------------
// Disks
// ---------------------------------------------------------------------------

fn disks(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let snap = state.snapshot.borrow();
    let volumes = snap.disks.clone();
    drop(snap);

    let mut body = div().flex_col().gap(theme.spacing.md);
    if volumes.is_empty() {
        body = body.child(
            label("No volume answered. On a non-Windows build there is nothing to enumerate.")
                .text_size(theme.typography.sm)
                .text_color(c.text_muted),
        );
    }
    for disk in &volumes {
        let used = disk.used_fraction();
        body = body.child(
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .p(theme.spacing.md)
                .rounded(theme.radii.lg)
                .bg(c.surface)
                .border(px(1.0), c.border)
                .child(
                    div()
                        .flex_row()
                        .items_center()
                        .gap(theme.spacing.sm)
                        .child(
                            label(format!("{} {}", disk.letter, disk.label))
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(theme.typography.md)
                                .weight(theme.typography.strong)
                                .text_color(c.text)
                                .truncate(),
                        )
                        .child(badge(disk.kind).variant(if disk.kind == "Fixed" {
                            BadgeVariant::Neutral
                        } else {
                            BadgeVariant::Accent
                        }))
                        .child(
                            label(disk.filesystem.clone())
                                .text_size(theme.typography.xs)
                                .text_color(c.text_muted)
                                .no_wrap(),
                        ),
                )
                .child(
                    Meter::new(used, c.elevated, pressure_color(theme, used))
                        .height(px(8.0))
                        .into_element(),
                )
                .child(
                    div()
                        .flex_row()
                        .child(
                            label(format!(
                                "{} free of {}",
                                sys::bytes(disk.free),
                                sys::bytes(disk.total)
                            ))
                            .flex_1()
                            .text_size(theme.typography.xs)
                            .text_color(c.text_muted),
                        )
                        .child(
                            label(format!("{:.0}% used", used * 100.0))
                                .text_size(theme.typography.xs)
                                .text_color(c.text_muted)
                                .no_wrap(),
                        ),
                ),
        );
    }

    page(theme, Page::Disks)
        .child(section(
            theme,
            "Volumes",
            "Free space is the volume's, not this account's quota — GetDiskFreeSpaceEx reports \
             both and they are not the same number on a machine with quotas turned on.",
            body.into_element(),
        ))
        .into_element()
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

fn network(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let snap = state.snapshot.borrow();
    let adapters = snap.interfaces.clone();
    let rx = snap.rx_rate();
    let tx = snap.tx_rate();
    drop(snap);

    let history = state.history.borrow();
    let scale = history.network_scale();
    let rx_chart =
        Graph::new(History::scaled(&history.rx, scale), HISTORY, c.accent, px(90.0)).into_element();
    let tx_chart = Graph::new(History::scaled(&history.tx, scale), HISTORY, c.warning, px(90.0))
        .into_element();
    drop(history);

    let mut list = div().flex_col().gap(theme.spacing.md);
    if adapters.is_empty() {
        list = list.child(
            label("No adapter answered.").text_size(theme.typography.sm).text_color(c.text_muted),
        );
    }
    for adapter in &adapters {
        list = list.child(
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .p(theme.spacing.md)
                .rounded(theme.radii.lg)
                .bg(c.surface)
                .border(px(1.0), c.border)
                .child(
                    div()
                        .flex_row()
                        .items_center()
                        .gap(theme.spacing.sm)
                        .child(
                            label(adapter.name.clone())
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(theme.typography.md)
                                .weight(theme.typography.strong)
                                .text_color(c.text)
                                .truncate(),
                        )
                        .child(badge(if adapter.up { "up" } else { "down" }).variant(
                            if adapter.up { BadgeVariant::Success } else { BadgeVariant::Neutral },
                        )),
                )
                .child(
                    label(adapter.description.clone())
                        .text_size(theme.typography.xs)
                        .text_color(c.text_muted)
                        .truncate(),
                )
                .child(div().h(theme.spacing.xs))
                .child(readout(
                    theme,
                    "Received",
                    &format!(
                        "{}  ·  {} total",
                        sys::rate(adapter.rx_rate),
                        sys::bytes(adapter.rx_total)
                    ),
                ))
                .child(readout(
                    theme,
                    "Sent",
                    &format!(
                        "{}  ·  {} total",
                        sys::rate(adapter.tx_rate),
                        sys::bytes(adapter.tx_total)
                    ),
                ))
                .child(readout(theme, "Link", &link_speed(adapter.link_speed))),
        );
    }

    page(theme, Page::Network)
        .child(
            div()
                .flex_row()
                .gap(theme.spacing.md)
                .child(stat(theme, "Receiving", &sys::rate(rx)))
                .child(stat(theme, "Sending", &sys::rate(tx)))
                .child(stat(theme, "Adapters", &adapters.len().to_string()))
                .child(stat(theme, "Full scale", &sys::rate(scale as f64))),
        )
        .child(section(
            theme,
            "Throughput",
            "Both charts share one axis — the largest rate seen in the window — so the ratio \
             between them is readable. Independent axes would make a trickle of uploads look \
             like a flood.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .p(theme.spacing.md)
                .rounded(theme.radii.lg)
                .bg(c.surface)
                .border(px(1.0), c.border)
                .child(legend(theme, "Received", c.accent))
                .child(rx_chart)
                .child(legend(theme, "Sent", c.warning))
                .child(tx_chart)
                .into_element(),
        ))
        .child(section(
            theme,
            "Adapters",
            "Loopback is left out: it is not a network, and every local socket's traffic would \
             swamp a readout meant to describe the wire.",
            list.into_element(),
        ))
        .into_element()
}

/// A link rate, as the driver reports it, in decimal bits.
///
/// Decimal here and binary everywhere else, and both are right: a link is sold
/// in powers of ten and memory is addressed in powers of two. Converting one to
/// the other's convention would make a gigabit adapter read as 0.93 Gb/s.
fn link_speed(bits_per_second: u64) -> String {
    // A driver with nothing to report says `u64::MAX` rather than zero.
    if bits_per_second == 0 || bits_per_second == u64::MAX {
        return "not reported".into();
    }
    const UNITS: [&str; 4] = ["bit/s", "Mbit/s", "Gbit/s", "Tbit/s"];
    let mut value = bits_per_second as f64;
    let mut unit = 0;
    // The first step is a million, not a thousand: nothing is quoted in kbit/s.
    if value >= 1_000_000.0 {
        value /= 1_000_000.0;
        unit = 1;
        while value >= 1000.0 && unit < UNITS.len() - 1 {
            value /= 1000.0;
            unit += 1;
        }
    }
    format!("{value:.0} {}", UNITS[unit])
}

/// A colour chip and a name, for a chart that has more than one series.
fn legend(theme: &Theme, name: &str, color: Color) -> AnyElement {
    div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.sm)
        .child(div().size(px(8.0)).rounded(px(2.0)).bg(color))
        .child(
            label(name.to_string())
                .text_size(theme.typography.xs)
                .text_color(theme.colors.text_muted)
                .no_wrap(),
        )
        .into_element()
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

fn system(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let m = &state.machine;
    let snap = state.snapshot.borrow();
    let uptime = snap.uptime;
    let sample_ms = snap.sample_ms;
    let count = snap.processes.len();
    drop(snap);

    page(theme, Page::System)
        .child(section(
            theme,
            "This machine",
            "Read once at start-up. None of it changes while the window is open, so re-reading \
             the registry every frame to learn the same CPU name would be waste.",
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(info_row(theme, "Host", &m.host))
                .child(info_row(theme, "Signed in as", &m.user))
                .child(info_row(theme, "Operating system", m.os_name.trim()))
                .child(info_row(
                    theme,
                    "Release",
                    &if m.os_release.is_empty() { "—".into() } else { m.os_release.clone() },
                ))
                .child(info_row(theme, "Build", &m.os_build))
                .child(info_row(theme, "Processor", &m.cpu_name))
                .child(info_row(
                    theme,
                    "Cores",
                    &format!("{} logical, {} physical", m.logical_cores, m.physical_cores),
                ))
                .child(info_row(theme, "Architecture", m.arch))
                .child(info_row(theme, "Installed memory", &sys::bytes(m.total_ram)))
                .child(info_row(theme, "Page size", &sys::bytes(m.page_size)))
                .child(info_row(theme, "Uptime", &sys::duration(uptime)))
                .into_element(),
        ))
        .child(section(
            theme,
            "What this monitor costs",
            "One pass over every process with an OpenProcess each. It runs on its own interval \
             rather than per frame, which is what lets the window animate at the display's rate \
             while the numbers change once a second.",
            div()
                .flex_row()
                .gap(theme.spacing.md)
                .child(stat(theme, "Sample", &format!("{sample_ms:.2} ms")))
                .child(stat(theme, "Processes walked", &count.to_string()))
                .child(stat(theme, "Interval", &format!("{:.1} s", state.interval_secs())))
                .child(stat(theme, "State", if state.paused.get() { "paused" } else { "sampling" }))
                .into_element(),
        ))
        .child(section(
            theme,
            "Where the numbers come from",
            "Every figure in this window is one documented call away from the kernel. Nothing \
             is derived from a counter provider, and nothing is smoothed.",
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(info_row(theme, "CPU", "GetSystemTimes"))
                .child(info_row(theme, "Memory", "GlobalMemoryStatusEx, GetPerformanceInfo"))
                .child(info_row(theme, "Processes", "CreateToolhelp32Snapshot, GetProcessTimes"))
                .child(info_row(theme, "Process memory", "GetProcessMemoryInfo"))
                .child(info_row(theme, "Volumes", "GetLogicalDrives, GetDiskFreeSpaceEx"))
                .child(info_row(theme, "Network", "GetIfTable2"))
                .child(info_row(theme, "Ending a task", "OpenProcess, TerminateProcess"))
                .into_element(),
        ))
        .into_element()
}

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

/// The outer column of a page, with its heading already in place.
fn page(theme: &Theme, which: Page) -> spherekit::ui::Div {
    let c = theme.colors;
    div().flex_col().gap(theme.spacing.xl).child(
        div()
            .flex_col()
            .gap(theme.spacing.xs)
            .child(
                label(which.title())
                    .text_size(theme.typography.xl)
                    .weight(theme.typography.strong)
                    .text_color(c.text),
            )
            .child(label(which.blurb()).text_size(theme.typography.md).text_color(c.text_muted)),
    )
}

/// One titled block: a name, a note about what it means, and the figures.
fn section(theme: &Theme, title: &str, note: &str, body: AnyElement) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_col()
        .gap(theme.spacing.sm)
        .child(
            label(title)
                .text_size(theme.typography.md)
                .weight(theme.typography.strong)
                .text_color(c.text),
        )
        .child(label(note).text_size(theme.typography.sm).text_color(c.text_muted))
        .child(div().h(theme.spacing.xs))
        .child(body)
        .into_element()
}

/// One number, in a card.
fn stat(theme: &Theme, name: &str, value: &str) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .gap(theme.spacing.xs)
        .p(theme.spacing.md)
        .rounded(theme.radii.lg)
        .bg(c.surface)
        .border(px(1.0), c.border)
        .child(
            label(name.to_string())
                .text_size(theme.typography.xs)
                .text_color(c.text_muted)
                .no_wrap(),
        )
        .child(
            label(value.to_string())
                .text_size(theme.typography.md)
                .weight(theme.typography.strong)
                .text_color(c.text)
                .truncate(),
        )
        .into_element()
}

/// A label at a fixed width and a value beside it, so a column lines up.
fn info_row(theme: &Theme, name: &str, value: &str) -> AnyElement {
    div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.md)
        .child(
            label(name.to_string())
                .text_size(theme.typography.sm)
                .text_color(theme.colors.text_muted)
                .w(px(168.0))
                .shrink(0.0)
                .no_wrap(),
        )
        .child(
            label(value.to_string())
                .flex_1()
                .min_w(px(0.0))
                .text_size(theme.typography.sm)
                .text_color(theme.colors.text)
                .truncate(),
        )
        .into_element()
}

/// The same, narrower, for inside a card.
fn readout(theme: &Theme, name: &str, value: &str) -> AnyElement {
    div()
        .flex_row()
        .gap(theme.spacing.sm)
        .child(
            label(name.to_string())
                .text_size(theme.typography.xs)
                .text_color(theme.colors.text_muted)
                .w(px(72.0))
                .shrink(0.0)
                .no_wrap(),
        )
        .child(
            label(value.to_string())
                .flex_1()
                .min_w(px(0.0))
                .text_size(theme.typography.xs)
                .text_color(theme.colors.text)
                .truncate(),
        )
        .into_element()
}

/// A row whose children wrap onto the next line when they run out of room.
///
/// `Styled` has no `flex_wrap`, so this reaches for the style directly.
/// Wrapping is the one flex property a tile grid genuinely needs and the one a
/// fixed layout would otherwise have to fake with a column count.
fn wrapping_row(theme: &Theme) -> spherekit::ui::Div {
    let mut row = div().flex_row().gap(theme.spacing.md);
    row.style_mut().flex_wrap = spherekit::layout::FlexWrap::Wrap;
    row
}

/// How alarming a full meter should look.
///
/// Three steps rather than a ramp: a colour that slides continuously from green
/// to red has no threshold in it, so nothing ever *becomes* a warning — it just
/// gets slightly more orange than it was a minute ago, which is a change no one
/// notices. The steps are where the advice actually changes.
fn pressure_color(theme: &Theme, fraction: f32) -> Color {
    match fraction {
        f if f >= 0.90 => theme.colors.danger,
        f if f >= 0.75 => theme.colors.warning,
        _ => theme.colors.accent,
    }
}
