//! Keyboard focus.
//!
//! Focus is explicit engine state, not something inferred from the window
//! system. A plug-in editor is frequently a child window inside a host that has
//! its own idea of focus, and a floating mixer, a modal dialog and a menu all
//! need their own traversal scope. Deriving any of that from
//! `WM_SETFOCUS` would be wrong in every one of those cases.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use sphere_core::{ElementId, NodeId, Px, Rect};

/// A handle to a focusable element.
///
/// Cheap to copy and stable across rebuilds, so a widget can hold one in its
/// own state and ask whether it is focused without consulting the tree.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FocusHandle {
    /// The element this handle belongs to.
    pub element: ElementId,
}

impl FocusHandle {
    /// Creates a handle for an element.
    #[inline]
    pub fn new(element: ElementId) -> Self {
        Self { element }
    }
}

/// One focusable entry, as registered during a build pass.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Focusable {
    /// Stable element identity.
    pub element: ElementId,
    /// The layout node, for scrolling into view and drawing the ring.
    pub node: NodeId,
    /// Explicit traversal order. `None` means document order.
    pub tab_index: Option<i32>,
    /// The scope this entry belongs to.
    pub scope: ScopeId,
    /// Whether the entry currently accepts focus.
    pub enabled: bool,
    /// Absolute bounds, for directional navigation.
    pub bounds: Rect<Px>,
}

/// Identifies a focus scope.
///
/// Scope 0 is the window's root scope and always exists.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeId(pub u32);

impl ScopeId {
    /// The window's root scope.
    pub const ROOT: Self = Self(0);
}

/// A focus scope: a region traversal is confined to.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Scope {
    /// This scope's id.
    pub id: ScopeId,
    /// The scope that contains this one.
    pub parent: Option<ScopeId>,
    /// When true, Tab cycles within this scope and never leaves it.
    ///
    /// This is what makes a modal dialog modal: without trapping, Tab walks
    /// straight out of the dialog into the window behind it, which is both a
    /// usability failure and, for a plug-in editor, a way to lose focus to the
    /// host entirely.
    pub trap: bool,
}

/// Which way keyboard traversal is moving.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FocusDirection {
    /// Tab.
    Next,
    /// Shift-Tab.
    Previous,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
}

/// Tracks which element has focus and how traversal moves between them.
#[derive(Debug, Default)]
pub struct FocusRegistry {
    /// Focusables in document order, as registered this frame.
    entries: Vec<Focusable>,
    index_of: FxHashMap<ElementId, usize>,
    scopes: Vec<Scope>,
    focused: Option<ElementId>,
    /// The scope traversal is currently confined to.
    active_scope: ScopeId,
    /// Elements that were focusable last frame, so focus can survive a rebuild
    /// that briefly removes an element.
    previously_known: FxHashSet<ElementId>,
    /// True when focus changed since the last time it was polled.
    changed: bool,
}

impl FocusRegistry {
    /// A registry with only the root scope.
    pub fn new() -> Self {
        Self {
            scopes: vec![Scope { id: ScopeId::ROOT, parent: None, trap: false }],
            active_scope: ScopeId::ROOT,
            ..Default::default()
        }
    }

    /// Clears the per-frame entry list, keeping focus and scope state.
    ///
    /// Called at the start of every build. Focus itself deliberately survives:
    /// a rebuild is not a reason for a text field to lose the caret.
    pub fn begin_frame(&mut self) {
        self.previously_known.clear();
        self.previously_known.extend(self.entries.iter().map(|e| e.element));
        self.entries.clear();
        self.index_of.clear();
    }

    /// Registers a focusable element for this frame.
    pub fn register(&mut self, entry: Focusable) {
        self.index_of.insert(entry.element, self.entries.len());
        self.entries.push(entry);
    }

    /// Opens a scope, returning its id.
    pub fn push_scope(&mut self, parent: Option<ScopeId>, trap: bool) -> ScopeId {
        let id = ScopeId(self.scopes.len() as u32);
        self.scopes.push(Scope { id, parent: parent.or(Some(ScopeId::ROOT)), trap });
        if trap {
            self.active_scope = id;
        }
        id
    }

    /// Closes a trapping scope and returns traversal to its parent.
    pub fn pop_scope(&mut self, id: ScopeId) {
        if self.active_scope != id {
            return;
        }
        self.active_scope =
            self.scopes.get(id.0 as usize).and_then(|s| s.parent).unwrap_or(ScopeId::ROOT);
    }

    /// The scope traversal is currently confined to.
    #[inline]
    pub fn active_scope(&self) -> ScopeId {
        self.active_scope
    }

    /// The focused element, if any.
    #[inline]
    pub fn focused(&self) -> Option<ElementId> {
        self.focused
    }

    /// The focused element's node, if it is present this frame.
    pub fn focused_node(&self) -> Option<NodeId> {
        let id = self.focused?;
        self.entries.get(*self.index_of.get(&id)?).map(|e| e.node)
    }

    /// True when `element` has focus.
    #[inline]
    pub fn is_focused(&self, element: ElementId) -> bool {
        self.focused == Some(element)
    }

    /// True and resets when focus has changed since the last call.
    ///
    /// The driver uses this to emit `FocusIn` and `FocusOut` exactly once per
    /// change rather than every frame.
    pub fn take_changed(&mut self) -> bool {
        core::mem::take(&mut self.changed)
    }

    /// Moves focus to an element, or clears it with `None`.
    ///
    /// Refuses to focus a disabled or unregistered element, so a stale handle
    /// cannot strand focus somewhere the user cannot see or escape from.
    pub fn focus(&mut self, element: Option<ElementId>) -> bool {
        match element {
            None => {
                if self.focused.is_some() {
                    self.focused = None;
                    self.changed = true;
                }
                true
            }
            Some(id) => {
                let ok = self
                    .index_of
                    .get(&id)
                    .and_then(|i| self.entries.get(*i))
                    .is_some_and(|e| e.enabled);
                if ok && self.focused != Some(id) {
                    self.focused = Some(id);
                    self.changed = true;
                }
                ok
            }
        }
    }

    /// Drops focus if the focused element is no longer focusable.
    ///
    /// Called after a build. An element that merely moved keeps focus; one that
    /// disappeared or became disabled loses it, because keyboard input
    /// otherwise vanishes into nothing with no visible indication why.
    pub fn prune(&mut self) {
        let Some(id) = self.focused else { return };
        let still_valid =
            self.index_of.get(&id).and_then(|i| self.entries.get(*i)).is_some_and(|e| e.enabled);
        if !still_valid {
            self.focused = None;
            self.changed = true;
        }
    }

    /// The traversal order for the active scope.
    ///
    /// Entries with an explicit `tab_index` sort first, ascending; the rest
    /// follow in document order. That is the rule every toolkit converged on,
    /// and deviating from it surprises users who navigate by keyboard.
    fn ordered(&self) -> SmallVec<[usize; 32]> {
        let scope = self.active_scope;
        let trapped = self.scopes.get(scope.0 as usize).is_some_and(|s| s.trap);

        let mut out: SmallVec<[usize; 32]> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.enabled && (!trapped || self.in_scope(e.scope, scope)))
            .map(|(i, _)| i)
            .collect();

        out.sort_by(|a, b| {
            let ea = &self.entries[*a];
            let eb = &self.entries[*b];
            match (ea.tab_index, eb.tab_index) {
                (Some(x), Some(y)) => x.cmp(&y).then(a.cmp(b)),
                (Some(_), None) => core::cmp::Ordering::Less,
                (None, Some(_)) => core::cmp::Ordering::Greater,
                (None, None) => a.cmp(b),
            }
        });
        out
    }

    /// True when `scope` is `ancestor` or nested inside it.
    fn in_scope(&self, mut scope: ScopeId, ancestor: ScopeId) -> bool {
        let mut guard = 0;
        loop {
            if scope == ancestor {
                return true;
            }
            let Some(s) = self.scopes.get(scope.0 as usize) else { return false };
            let Some(parent) = s.parent else { return false };
            if parent == scope {
                return false;
            }
            scope = parent;
            guard += 1;
            if guard > 64 {
                // A malformed scope chain must not hang keyboard navigation.
                return false;
            }
        }
    }

    /// Moves focus in a direction, returning the newly focused element.
    pub fn navigate(&mut self, direction: FocusDirection) -> Option<ElementId> {
        match direction {
            FocusDirection::Next | FocusDirection::Previous => self.navigate_linear(direction),
            _ => self.navigate_spatial(direction),
        }
    }

    fn navigate_linear(&mut self, direction: FocusDirection) -> Option<ElementId> {
        let order = self.ordered();
        if order.is_empty() {
            return None;
        }
        let forward = direction == FocusDirection::Next;

        let current = self.focused.and_then(|id| {
            let idx = *self.index_of.get(&id)?;
            order.iter().position(|i| *i == idx)
        });

        let next_pos = match current {
            // Tab with nothing focused starts at the beginning; Shift-Tab at
            // the end, which is what makes both feel symmetric.
            None => {
                if forward {
                    0
                } else {
                    order.len() - 1
                }
            }
            Some(pos) => {
                if forward {
                    if pos + 1 < order.len() {
                        pos + 1
                    } else if self.is_trapped() {
                        0
                    } else {
                        return None;
                    }
                } else if pos > 0 {
                    pos - 1
                } else if self.is_trapped() {
                    order.len() - 1
                } else {
                    return None;
                }
            }
        };

        let element = self.entries[order[next_pos]].element;
        self.focus(Some(element));
        Some(element)
    }

    /// True when the active scope traps traversal, so Tab wraps instead of
    /// escaping.
    #[inline]
    pub fn is_trapped(&self) -> bool {
        self.scopes.get(self.active_scope.0 as usize).is_some_and(|s| s.trap)
    }

    /// Directional navigation: pick the nearest enabled entry in that
    /// direction.
    ///
    /// Scores by primary-axis distance plus a penalty for cross-axis
    /// misalignment. Pure Euclidean distance picks diagonal neighbours over the
    /// obvious one directly below, which feels wrong on a mixer strip.
    fn navigate_spatial(&mut self, direction: FocusDirection) -> Option<ElementId> {
        let current = self.focused.and_then(|id| self.index_of.get(&id).copied());
        let Some(from) = current.and_then(|i| self.entries.get(i)).map(|e| e.bounds.center())
        else {
            return self.navigate_linear(FocusDirection::Next);
        };

        let order = self.ordered();
        let mut best: Option<(f32, ElementId)> = None;

        for i in order {
            let e = &self.entries[i];
            if Some(e.element) == self.focused {
                continue;
            }
            let to = e.bounds.center();
            let dx = to.x.get() - from.x.get();
            let dy = to.y.get() - from.y.get();

            let (primary, cross) = match direction {
                FocusDirection::Left => (-dx, dy.abs()),
                FocusDirection::Right => (dx, dy.abs()),
                FocusDirection::Up => (-dy, dx.abs()),
                FocusDirection::Down => (dy, dx.abs()),
                _ => unreachable!("linear directions handled above"),
            };
            if primary <= 1.0 {
                continue;
            }
            let score = primary + cross * 2.0;
            if best.is_none_or(|(b, _)| score < b) {
                best = Some((score, e.element));
            }
        }

        let element = best.map(|(_, e)| e)?;
        self.focus(Some(element));
        Some(element)
    }

    /// Number of focusable entries registered this frame.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is focusable.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_core::{px, rect};

    fn eid(n: u32) -> ElementId {
        ElementId::from_key(n)
    }

    fn entry(n: u32, scope: ScopeId, tab_index: Option<i32>, x: f32, y: f32) -> Focusable {
        Focusable {
            element: eid(n),
            node: NodeId::new(n, 1),
            tab_index,
            scope,
            enabled: true,
            bounds: rect(px(x), px(y), px(40.0), px(20.0)),
        }
    }

    fn registry(count: u32) -> FocusRegistry {
        let mut r = FocusRegistry::new();
        r.begin_frame();
        for i in 0..count {
            r.register(entry(i, ScopeId::ROOT, None, i as f32 * 50.0, 0.0));
        }
        r
    }

    #[test]
    fn tab_walks_forward_in_document_order() {
        let mut r = registry(3);
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(0)));
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(1)));
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(2)));
    }

    #[test]
    fn shift_tab_with_nothing_focused_starts_at_the_end() {
        let mut r = registry(3);
        assert_eq!(r.navigate(FocusDirection::Previous), Some(eid(2)));
    }

    #[test]
    fn tab_off_the_end_of_an_untrapped_scope_yields_nothing() {
        // Returning None is what lets the window hand focus to the host, which
        // a plug-in editor must do.
        let mut r = registry(2);
        r.navigate(FocusDirection::Next);
        r.navigate(FocusDirection::Next);
        assert_eq!(r.navigate(FocusDirection::Next), None);
    }

    #[test]
    fn a_trapped_scope_wraps_instead_of_escaping() {
        let mut r = FocusRegistry::new();
        let modal = r.push_scope(Some(ScopeId::ROOT), true);
        r.begin_frame();
        for i in 0..2 {
            r.register(entry(i, modal, None, i as f32 * 50.0, 0.0));
        }
        assert!(r.is_trapped());
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(0)));
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(1)));
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(0)), "must wrap, not escape");
        assert_eq!(r.navigate(FocusDirection::Previous), Some(eid(1)));
    }

    #[test]
    fn a_trapped_scope_excludes_entries_outside_it() {
        let mut r = FocusRegistry::new();
        let modal = r.push_scope(Some(ScopeId::ROOT), true);
        r.begin_frame();
        r.register(entry(0, ScopeId::ROOT, None, 0.0, 0.0));
        r.register(entry(1, modal, None, 50.0, 0.0));
        r.register(entry(2, modal, None, 100.0, 0.0));
        // The root-scope entry must be unreachable while the modal is open.
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(r.navigate(FocusDirection::Next));
        }
        assert!(!seen.contains(&Some(eid(0))), "focus escaped the modal: {seen:?}");
    }

    #[test]
    fn popping_a_scope_returns_traversal_to_the_parent() {
        let mut r = FocusRegistry::new();
        let modal = r.push_scope(Some(ScopeId::ROOT), true);
        assert_eq!(r.active_scope(), modal);
        r.pop_scope(modal);
        assert_eq!(r.active_scope(), ScopeId::ROOT);
        assert!(!r.is_trapped());
    }

    #[test]
    fn explicit_tab_index_sorts_before_document_order() {
        let mut r = FocusRegistry::new();
        r.begin_frame();
        r.register(entry(0, ScopeId::ROOT, None, 0.0, 0.0));
        r.register(entry(1, ScopeId::ROOT, Some(2), 50.0, 0.0));
        r.register(entry(2, ScopeId::ROOT, Some(1), 100.0, 0.0));
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(2)), "tab_index 1 first");
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(1)), "then tab_index 2");
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(0)), "then document order");
    }

    #[test]
    fn disabled_entries_are_skipped() {
        let mut r = FocusRegistry::new();
        r.begin_frame();
        r.register(entry(0, ScopeId::ROOT, None, 0.0, 0.0));
        let mut disabled = entry(1, ScopeId::ROOT, None, 50.0, 0.0);
        disabled.enabled = false;
        r.register(disabled);
        r.register(entry(2, ScopeId::ROOT, None, 100.0, 0.0));
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(0)));
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(2)));
    }

    #[test]
    fn focusing_a_disabled_or_unknown_element_is_refused() {
        let mut r = registry(2);
        assert!(!r.focus(Some(eid(99))));
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn focus_survives_a_rebuild_that_keeps_the_element() {
        let mut r = registry(3);
        r.focus(Some(eid(1)));
        // Rebuild with the same elements.
        r.begin_frame();
        for i in 0..3 {
            r.register(entry(i, ScopeId::ROOT, None, i as f32 * 50.0, 0.0));
        }
        r.prune();
        assert_eq!(r.focused(), Some(eid(1)), "a rebuild must not steal the caret");
    }

    #[test]
    fn focus_is_dropped_when_its_element_disappears() {
        let mut r = registry(3);
        r.focus(Some(eid(1)));
        r.begin_frame();
        r.register(entry(0, ScopeId::ROOT, None, 0.0, 0.0));
        r.register(entry(2, ScopeId::ROOT, None, 100.0, 0.0));
        r.prune();
        assert_eq!(r.focused(), None, "keyboard input must not vanish silently");
    }

    #[test]
    fn focus_is_dropped_when_its_element_becomes_disabled() {
        let mut r = registry(2);
        r.focus(Some(eid(0)));
        r.begin_frame();
        let mut disabled = entry(0, ScopeId::ROOT, None, 0.0, 0.0);
        disabled.enabled = false;
        r.register(disabled);
        r.register(entry(1, ScopeId::ROOT, None, 50.0, 0.0));
        r.prune();
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn change_notification_fires_once_per_change() {
        let mut r = registry(2);
        r.take_changed();
        r.focus(Some(eid(0)));
        assert!(r.take_changed());
        assert!(!r.take_changed(), "polling twice must not report a second change");
        r.focus(Some(eid(0)));
        assert!(!r.take_changed(), "refocusing the same element is not a change");
    }

    #[test]
    fn arrow_navigation_prefers_the_aligned_neighbour() {
        // A mixer strip: two controls directly below, one far to the side and
        // slightly below. Down must pick the aligned one.
        let mut r = FocusRegistry::new();
        r.begin_frame();
        r.register(entry(0, ScopeId::ROOT, None, 100.0, 0.0));
        r.register(entry(1, ScopeId::ROOT, None, 100.0, 60.0));
        r.register(entry(2, ScopeId::ROOT, None, 400.0, 30.0));
        r.focus(Some(eid(0)));
        assert_eq!(r.navigate(FocusDirection::Down), Some(eid(1)));
    }

    #[test]
    fn arrow_navigation_ignores_entries_behind_it() {
        let mut r = FocusRegistry::new();
        r.begin_frame();
        r.register(entry(0, ScopeId::ROOT, None, 0.0, 0.0));
        r.register(entry(1, ScopeId::ROOT, None, 100.0, 0.0));
        r.focus(Some(eid(1)));
        assert_eq!(r.navigate(FocusDirection::Right), None, "nothing lies to the right");
        assert_eq!(r.navigate(FocusDirection::Left), Some(eid(0)));
    }

    #[test]
    fn navigating_an_empty_registry_is_a_no_op() {
        let mut r = FocusRegistry::new();
        r.begin_frame();
        assert_eq!(r.navigate(FocusDirection::Next), None);
        assert_eq!(r.navigate(FocusDirection::Down), None);
        assert!(r.is_empty());
    }

    #[test]
    fn a_cyclic_scope_chain_does_not_hang() {
        let mut r = FocusRegistry::new();
        // Hand-build a self-referential scope, which a malformed caller could
        // produce; traversal must terminate rather than spin.
        r.scopes.push(Scope { id: ScopeId(1), parent: Some(ScopeId(1)), trap: true });
        r.active_scope = ScopeId(1);
        r.begin_frame();
        r.register(entry(0, ScopeId(1), None, 0.0, 0.0));
        assert_eq!(r.navigate(FocusDirection::Next), Some(eid(0)));
    }
}
