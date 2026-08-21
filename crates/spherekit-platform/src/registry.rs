//! Per-window state and the multi-window registry.
//!
//! A DAW opens many SphereKit windows at once — a main editor, a detached mixer,
//! one editor per plug-in instance — and each has its own size, scale factor,
//! focus and redraw appetite. Nothing here is global, and nothing assumes a
//! "main" window: the process may not even own the event loop.
//!
//! [`WindowRegistry`] is generic over the window payload so that the whole
//! bookkeeping layer can be tested without opening a window, and so that a
//! future backend can store its own window type in it.

use spherekit_core::{DevicePx, Point, Px, ScaleFactor, Size, WindowId};

use crate::event::{Theme, WindowEvent};
use crate::keyboard::Modifiers;
use crate::scheduler::RedrawPolicy;

/// Everything the platform layer knows about one window right now.
///
/// Derived from the event stream, so it is always consistent with what the
/// application has been told — as opposed to querying the platform, which can
/// return a size the application has not yet seen an event for.
#[derive(Clone, PartialEq, Debug)]
pub struct WindowState {
    /// Drawable extent in device pixels: what the surface is configured with.
    physical_size: Size<DevicePx>,
    /// The window's current scale factor.
    scale_factor: ScaleFactor,
    /// Outer position in desktop device pixels, once the platform reports one.
    position: Option<Point<DevicePx>>,
    /// Whether this window has keyboard focus.
    focused: bool,
    /// Whether the window is entirely hidden.
    occluded: bool,
    /// Whether a close has been requested and not yet honoured.
    close_requested: bool,
    /// Whether the platform window is gone.
    destroyed: bool,
    /// Modifier chord as of the last input event.
    modifiers: Modifiers,
    /// Pointer position in logical pixels, when the pointer is over the window.
    cursor: Option<Point<Px>>,
    /// Whether the pointer is inside, even if no position has arrived yet.
    cursor_inside: bool,
    /// The appearance the platform last reported.
    theme: Option<Theme>,
    /// A frame is owed.
    needs_redraw: bool,
    /// The floor this window puts under the loop's redraw policy.
    redraw_policy: RedrawPolicy,
}

impl WindowState {
    /// State for a freshly created window.
    ///
    /// Starts dirty: a window that has never been painted needs its first
    /// frame, and waiting for the platform to ask would leave it blank on
    /// backends that do not send an initial expose.
    pub fn new(physical_size: Size<DevicePx>, scale_factor: ScaleFactor) -> Self {
        Self {
            physical_size,
            scale_factor,
            position: None,
            focused: false,
            occluded: false,
            close_requested: false,
            destroyed: false,
            modifiers: Modifiers::empty(),
            cursor: None,
            cursor_inside: false,
            theme: None,
            needs_redraw: true,
            redraw_policy: RedrawPolicy::Idle,
        }
    }

    /// Drawable extent in device pixels.
    #[inline]
    pub fn physical_size(&self) -> Size<DevicePx> {
        self.physical_size
    }

    /// Drawable extent in logical pixels.
    ///
    /// Derived, never stored: storing both is how they end up disagreeing after
    /// a scale-factor change that arrives before the matching resize.
    #[inline]
    pub fn logical_size(&self) -> Size<Px> {
        Size::new(
            self.scale_factor.to_logical(self.physical_size.width),
            self.scale_factor.to_logical(self.physical_size.height),
        )
    }

    /// The window's scale factor.
    #[inline]
    pub fn scale_factor(&self) -> ScaleFactor {
        self.scale_factor
    }

    /// Outer position in desktop device pixels, if reported.
    #[inline]
    pub fn position(&self) -> Option<Point<DevicePx>> {
        self.position
    }

    /// Whether this window has keyboard focus.
    #[inline]
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Whether the window is entirely hidden.
    #[inline]
    pub fn is_occluded(&self) -> bool {
        self.occluded
    }

    /// Whether a close was requested and not yet honoured.
    #[inline]
    pub fn close_requested(&self) -> bool {
        self.close_requested
    }

    /// Clears the pending close request, for an application that asked the user
    /// and was told not to close.
    #[inline]
    pub fn clear_close_request(&mut self) {
        self.close_requested = false;
    }

    /// Whether the platform window is gone.
    #[inline]
    pub fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    /// The modifier chord as of the last input event.
    #[inline]
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// Pointer position in logical pixels, or `None` when the pointer is
    /// elsewhere.
    #[inline]
    pub fn cursor_position(&self) -> Option<Point<Px>> {
        self.cursor
    }

    /// Whether the pointer is over this window.
    #[inline]
    pub fn cursor_inside(&self) -> bool {
        self.cursor_inside
    }

    /// The appearance the platform last reported.
    #[inline]
    pub fn theme(&self) -> Option<Theme> {
        self.theme
    }

    /// Sets focus directly. Used by [`WindowRegistry`] to keep the rest of the
    /// registry consistent when another window takes focus.
    #[inline]
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if !focused {
            // The key-up for a modifier held across an alt-tab is delivered to
            // whoever gained focus, so a stuck Ctrl is guaranteed unless the
            // chord is cleared here.
            self.modifiers = Modifiers::empty();
        }
    }

    /// Whether a frame is owed.
    #[inline]
    pub fn needs_redraw(&self) -> bool {
        self.needs_redraw
    }

    /// Requests one more frame.
    #[inline]
    pub fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// Records that a frame reached the screen.
    #[inline]
    pub fn mark_presented(&mut self) {
        self.needs_redraw = false;
    }

    /// This window's standing redraw appetite, independent of the dirty flag.
    #[inline]
    pub fn redraw_policy(&self) -> RedrawPolicy {
        self.redraw_policy
    }

    /// Sets this window's standing redraw appetite. A window holding a live
    /// meter sets [`RedrawPolicy::Realtime`] while the transport rolls and
    /// drops back to [`RedrawPolicy::Idle`] when it stops.
    #[inline]
    pub fn set_redraw_policy(&mut self, policy: RedrawPolicy) {
        self.redraw_policy = policy;
    }

    /// Whether drawing this window can succeed at all.
    ///
    /// A zero-extent surface is rejected by every GPU backend, and an occluded
    /// or destroyed window has nothing to present to.
    #[inline]
    pub fn is_renderable(&self) -> bool {
        !self.destroyed && !self.occluded && !self.physical_size.is_empty()
    }

    /// What this window contributes to the loop's control flow.
    ///
    /// A window that cannot be rendered contributes [`RedrawPolicy::Idle`] no
    /// matter what it asked for. That is what makes minimising a plug-in editor
    /// with a live meter actually drop to zero CPU rather than spinning on
    /// frames that cannot be presented.
    pub fn effective_policy(&self) -> RedrawPolicy {
        if !self.is_renderable() {
            return RedrawPolicy::Idle;
        }
        if self.needs_redraw {
            self.redraw_policy.max(RedrawPolicy::Dirty)
        } else {
            self.redraw_policy
        }
    }

    /// Folds one event into this state.
    ///
    /// Input events update the cached chord and pointer position; geometry
    /// events update size, scale and position; anything that makes the current
    /// frame stale sets the dirty flag.
    pub fn apply(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::Resized(s) => self.physical_size = *s,
            WindowEvent::ScaleFactorChanged(sf) => self.scale_factor = *sf,
            WindowEvent::Moved(p) => self.position = Some(*p),
            WindowEvent::Focused(f) => self.set_focused(*f),
            WindowEvent::CursorMoved(p) => {
                self.cursor = Some(*p);
                self.cursor_inside = true;
            }
            WindowEvent::CursorEntered => self.cursor_inside = true,
            WindowEvent::CursorLeft => {
                self.cursor = None;
                self.cursor_inside = false;
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = *m,
            WindowEvent::MouseInput { modifiers, .. }
            | WindowEvent::KeyboardInput { modifiers, .. } => self.modifiers = *modifiers,
            WindowEvent::ThemeChanged(t) => self.theme = Some(*t),
            WindowEvent::Occluded(o) => {
                self.occluded = *o;
                if !*o {
                    // Coming back into view: the contents are whatever was left
                    // on screen before, which on most compositors is nothing.
                    self.needs_redraw = true;
                }
            }
            WindowEvent::CloseRequested => self.close_requested = true,
            WindowEvent::Destroyed => {
                self.destroyed = true;
                self.needs_redraw = false;
            }
            _ => {}
        }
        if event.invalidates_contents() && !self.destroyed {
            self.needs_redraw = true;
        }
    }
}

/// One registered window and its state.
#[derive(Clone, Debug)]
struct WindowEntry<W> {
    /// The registry-assigned identity.
    id: WindowId,
    /// The backend's window object.
    window: W,
    /// Everything derived from the event stream.
    state: WindowState,
}

/// Every window the application currently has open.
///
/// Backed by a `Vec` with linear lookup rather than a hash map: a process has
/// single-digit windows, iteration order stays creation order (which makes
/// redraw dispatch deterministic), and a linear scan over eight entries beats
/// hashing a key.
///
/// Ids are minted from a counter and **never reused**. A generational store
/// would recycle slots, and a plug-in host that destroys and recreates an
/// editor in the same tick would then hand a stale id to a live window.
#[derive(Clone, Debug)]
pub struct WindowRegistry<W> {
    /// Live windows, in creation order.
    entries: Vec<WindowEntry<W>>,
    /// Next index to mint.
    next_index: u32,
    /// Generation to mint, bumped only if the index counter wraps.
    generation: u32,
    /// Which window last reported gaining focus.
    focused: Option<WindowId>,
}

impl<W> Default for WindowRegistry<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W> WindowRegistry<W> {
    /// An empty registry.
    pub const fn new() -> Self {
        Self { entries: Vec::new(), next_index: 0, generation: 1, focused: None }
    }

    /// Number of live windows.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no window is open. The application should usually exit.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Mints a fresh, never-reused identity.
    fn allocate_id(&mut self) -> WindowId {
        let id = WindowId::new(self.next_index, self.generation);
        match self.next_index.checked_add(1) {
            Some(next) => self.next_index = next,
            None => {
                // Four billion windows in one process is not a real scenario,
                // but wrapping the index silently would alias ids, so bump the
                // generation instead of pretending it cannot happen.
                self.next_index = 0;
                self.generation = self.generation.wrapping_add(1).max(1);
            }
        }
        id
    }

    /// Registers a window built from its own id.
    ///
    /// The closure form exists because a backend window usually wants to carry
    /// its own id, and the id is not known until the registry mints it.
    pub fn insert_with(
        &mut self,
        state: WindowState,
        build: impl FnOnce(WindowId) -> W,
    ) -> WindowId {
        match self.try_insert_with::<core::convert::Infallible>(|id| Ok((build(id), state))) {
            Ok(id) => id,
            Err(never) => match never {},
        }
    }

    /// Registers a window whose construction can fail, and whose initial state
    /// depends on the window itself.
    ///
    /// Creating a platform window needs the id up front (so the window can
    /// carry it) but produces the size and scale factor that the state needs,
    /// which is why both come out of the closure together. A failed build
    /// consumes an id and leaves the registry unchanged; ids are never reused
    /// anyway, so nothing is leaked but a counter increment.
    pub fn try_insert_with<E>(
        &mut self,
        build: impl FnOnce(WindowId) -> Result<(W, WindowState), E>,
    ) -> Result<WindowId, E> {
        let id = self.allocate_id();
        let (window, state) = build(id)?;
        self.entries.push(WindowEntry { id, window, state });
        Ok(id)
    }

    /// Registers an already-built window.
    pub fn insert(&mut self, window: W, state: WindowState) -> WindowId {
        self.insert_with(state, |_| window)
    }

    /// Removes a window, returning it so the caller controls when it is
    /// dropped — which matters, because dropping a window destroys its
    /// platform handle and any GPU surface built from it must go first.
    pub fn remove(&mut self, id: WindowId) -> Option<W> {
        let pos = self.entries.iter().position(|e| e.id == id)?;
        if self.focused == Some(id) {
            self.focused = None;
        }
        Some(self.entries.remove(pos).window)
    }

    /// True when the id addresses a live window.
    #[inline]
    pub fn contains(&self, id: WindowId) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    /// The backend window for an id.
    pub fn window(&self, id: WindowId) -> Option<&W> {
        self.entries.iter().find(|e| e.id == id).map(|e| &e.window)
    }

    /// The state for an id.
    pub fn state(&self, id: WindowId) -> Option<&WindowState> {
        self.entries.iter().find(|e| e.id == id).map(|e| &e.state)
    }

    /// The state for an id, mutably.
    pub fn state_mut(&mut self, id: WindowId) -> Option<&mut WindowState> {
        self.entries.iter_mut().find(|e| e.id == id).map(|e| &mut e.state)
    }

    /// The window and its state together.
    pub fn get(&self, id: WindowId) -> Option<(&W, &WindowState)> {
        self.entries.iter().find(|e| e.id == id).map(|e| (&e.window, &e.state))
    }

    /// The window and its state, with the state mutable.
    pub fn get_mut(&mut self, id: WindowId) -> Option<(&W, &mut WindowState)> {
        self.entries.iter_mut().find(|e| e.id == id).map(|e| (&e.window, &mut e.state))
    }

    /// Iterates every window in creation order.
    pub fn iter(&self) -> impl Iterator<Item = (WindowId, &W, &WindowState)> {
        self.entries.iter().map(|e| (e.id, &e.window, &e.state))
    }

    /// Iterates every window's state mutably.
    pub fn states_mut(&mut self) -> impl Iterator<Item = (WindowId, &mut WindowState)> {
        self.entries.iter_mut().map(|e| (e.id, &mut e.state))
    }

    /// Every live id, in creation order.
    pub fn ids(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.entries.iter().map(|e| e.id)
    }

    /// The window that last reported gaining focus.
    #[inline]
    pub fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    /// Folds an event into one window's state, keeping cross-window invariants.
    ///
    /// Returns `false` when the id is unknown, which happens legitimately:
    /// events for a window can still be in flight after it has been removed.
    pub fn apply(&mut self, id: WindowId, event: &WindowEvent) -> bool {
        if !self.contains(id) {
            return false;
        }
        match event {
            WindowEvent::Focused(true) => {
                // Exactly one window is focused. Some platforms do not send the
                // matching `Focused(false)` to the window that lost it, and a
                // second window believing it is focused means two blinking
                // carets.
                for entry in &mut self.entries {
                    if entry.id != id && entry.state.is_focused() {
                        entry.state.apply(&WindowEvent::Focused(false));
                    }
                }
                self.focused = Some(id);
            }
            WindowEvent::Focused(false) if self.focused == Some(id) => self.focused = None,
            WindowEvent::Destroyed if self.focused == Some(id) => self.focused = None,
            _ => {}
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.state.apply(event);
        }
        true
    }

    /// The strongest redraw appetite across every window.
    ///
    /// This is what the event loop's control flow is derived from: one
    /// animating window keeps the loop awake, and all-idle windows let it
    /// block.
    pub fn aggregate_policy(&self) -> RedrawPolicy {
        self.entries.iter().map(|e| e.state.effective_policy()).max().unwrap_or(RedrawPolicy::Idle)
    }

    /// The windows that owe a frame and can actually be drawn.
    pub fn needing_redraw(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.entries
            .iter()
            .filter(|e| e.state.needs_redraw() && e.state.is_renderable())
            .map(|e| e.id)
    }

    /// Removes every window, returning them in creation order.
    pub fn drain(&mut self) -> Vec<W> {
        self.focused = None;
        self.entries.drain(..).map(|e| e.window).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{point, px, size};

    fn dp(w: i32, h: i32) -> Size<DevicePx> {
        size(DevicePx(w), DevicePx(h))
    }

    fn state(w: i32, h: i32, scale: f32) -> WindowState {
        WindowState::new(dp(w, h), ScaleFactor::new(scale))
    }

    #[test]
    fn logical_size_tracks_the_scale_factor_at_every_common_step() {
        for (scale, expected_w, expected_h) in [
            (1.0, 1920.0, 1080.0),
            (1.25, 1536.0, 864.0),
            (1.5, 1280.0, 720.0),
            (2.0, 960.0, 540.0),
        ] {
            let s = state(1920, 1080, scale);
            let logical = s.logical_size();
            assert!(
                (logical.width.get() - expected_w).abs() < 0.001
                    && (logical.height.get() - expected_h).abs() < 0.001,
                "scale {scale} gave {logical:?}"
            );
        }
    }

    #[test]
    fn a_scale_change_alone_changes_the_logical_size() {
        // The platform sends the scale change first and the resize second; in
        // between, the logical size must already reflect the new scale rather
        // than a stale cached value.
        let mut s = state(1000, 500, 1.0);
        assert_eq!(s.logical_size(), size(px(1000.0), px(500.0)));
        s.apply(&WindowEvent::ScaleFactorChanged(ScaleFactor::new(2.0)));
        assert_eq!(s.logical_size(), size(px(500.0), px(250.0)));
        s.apply(&WindowEvent::Resized(dp(2000, 1000)));
        assert_eq!(s.logical_size(), size(px(1000.0), px(500.0)));
    }

    #[test]
    fn a_new_window_owes_its_first_frame() {
        let s = state(100, 100, 1.0);
        assert!(s.needs_redraw());
        assert_eq!(s.effective_policy(), RedrawPolicy::Dirty);
    }

    #[test]
    fn presenting_clears_the_dirty_flag_and_lets_the_window_idle() {
        let mut s = state(100, 100, 1.0);
        s.mark_presented();
        assert!(!s.needs_redraw());
        assert_eq!(s.effective_policy(), RedrawPolicy::Idle);
    }

    #[test]
    fn a_zero_sized_window_is_not_renderable() {
        // Minimising reports a zero extent on Windows; drawing it would fail
        // in the swapchain rather than here.
        let mut s = state(0, 0, 1.0);
        assert!(!s.is_renderable());
        assert_eq!(s.effective_policy(), RedrawPolicy::Idle);
        s.apply(&WindowEvent::Resized(dp(800, 600)));
        assert!(s.is_renderable());
    }

    #[test]
    fn an_occluded_window_with_a_live_meter_still_goes_idle() {
        let mut s = state(800, 600, 1.0);
        s.set_redraw_policy(RedrawPolicy::Realtime);
        assert_eq!(s.effective_policy(), RedrawPolicy::Realtime);
        s.apply(&WindowEvent::Occluded(true));
        assert!(s.is_occluded());
        assert_eq!(s.effective_policy(), RedrawPolicy::Idle, "occluded windows must not spin");
        // Coming back needs a repaint, and the meter resumes.
        s.apply(&WindowEvent::Occluded(false));
        assert!(s.needs_redraw());
        assert_eq!(s.effective_policy(), RedrawPolicy::Realtime);
    }

    #[test]
    fn losing_focus_clears_the_modifier_chord() {
        // Otherwise Ctrl stays stuck down after Ctrl+Tab away from the window.
        let mut s = state(100, 100, 1.0);
        s.apply(&WindowEvent::ModifiersChanged(Modifiers::CTRL | Modifiers::SHIFT));
        assert_eq!(s.modifiers(), Modifiers::CTRL | Modifiers::SHIFT);
        s.apply(&WindowEvent::Focused(false));
        assert_eq!(s.modifiers(), Modifiers::empty());
        assert!(!s.is_focused());
    }

    #[test]
    fn pointer_state_is_cleared_when_the_pointer_leaves() {
        let mut s = state(100, 100, 1.0);
        s.apply(&WindowEvent::CursorEntered);
        assert!(s.cursor_inside());
        assert_eq!(s.cursor_position(), None, "entering says nothing about where");
        s.apply(&WindowEvent::CursorMoved(point(px(4.0), px(9.0))));
        assert_eq!(s.cursor_position(), Some(point(px(4.0), px(9.0))));
        s.apply(&WindowEvent::CursorLeft);
        assert_eq!(s.cursor_position(), None);
        assert!(!s.cursor_inside());
    }

    #[test]
    fn a_close_request_is_a_question_not_an_order() {
        let mut s = state(100, 100, 1.0);
        s.apply(&WindowEvent::CloseRequested);
        assert!(s.close_requested());
        assert!(!s.is_destroyed(), "the window is still alive until we close it");
        s.clear_close_request();
        assert!(!s.close_requested());
    }

    #[test]
    fn a_destroyed_window_stops_asking_for_frames() {
        let mut s = state(100, 100, 1.0);
        s.request_redraw();
        s.apply(&WindowEvent::Destroyed);
        assert!(s.is_destroyed());
        assert!(!s.needs_redraw());
        assert!(!s.is_renderable());
        // A redraw request arriving after destruction must not resurrect it in
        // the aggregate policy.
        assert_eq!(s.effective_policy(), RedrawPolicy::Idle);
    }

    #[test]
    fn input_events_keep_the_cached_chord_in_sync() {
        use crate::event::{ElementState, MouseButton};
        let mut s = state(100, 100, 1.0);
        s.apply(&WindowEvent::MouseInput {
            button: MouseButton::Left,
            state: ElementState::Pressed,
            modifiers: Modifiers::ALT,
        });
        assert_eq!(s.modifiers(), Modifiers::ALT);
    }

    #[test]
    fn ids_are_never_reused_so_a_stale_id_cannot_alias() {
        let mut r: WindowRegistry<&str> = WindowRegistry::new();
        let a = r.insert("a", state(10, 10, 1.0));
        r.insert("b", state(10, 10, 1.0));
        assert_eq!(r.remove(a), Some("a"));
        let c = r.insert("c", state(10, 10, 1.0));
        assert_ne!(a, c);
        assert!(!r.contains(a));
        assert_eq!(r.window(a), None);
        assert_eq!(r.window(c), Some(&"c"));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn insert_with_hands_the_window_its_own_id() {
        let mut r: WindowRegistry<WindowId> = WindowRegistry::new();
        let id = r.insert_with(state(10, 10, 1.0), |id| id);
        assert_eq!(r.window(id), Some(&id));
    }

    #[test]
    fn removing_an_unknown_id_is_a_no_op() {
        let mut r: WindowRegistry<&str> = WindowRegistry::new();
        let a = r.insert("a", state(10, 10, 1.0));
        assert_eq!(r.remove(a), Some("a"));
        assert_eq!(r.remove(a), None);
        assert!(r.is_empty());
        assert_eq!(r.aggregate_policy(), RedrawPolicy::Idle);
    }

    #[test]
    fn exactly_one_window_holds_focus() {
        let mut r: WindowRegistry<&str> = WindowRegistry::new();
        let a = r.insert("a", state(10, 10, 1.0));
        let b = r.insert("b", state(10, 10, 1.0));
        r.apply(a, &WindowEvent::Focused(true));
        assert_eq!(r.focused(), Some(a));
        assert!(r.state(a).unwrap().is_focused());

        // The platform may never tell `a` it lost focus.
        r.apply(b, &WindowEvent::Focused(true));
        assert_eq!(r.focused(), Some(b));
        assert!(!r.state(a).unwrap().is_focused());
        assert!(r.state(b).unwrap().is_focused());
        assert_eq!(r.iter().filter(|(_, _, s)| s.is_focused()).count(), 1);
    }

    #[test]
    fn focus_is_dropped_when_the_focused_window_goes_away() {
        let mut r: WindowRegistry<&str> = WindowRegistry::new();
        let a = r.insert("a", state(10, 10, 1.0));
        r.apply(a, &WindowEvent::Focused(true));
        r.apply(a, &WindowEvent::Destroyed);
        assert_eq!(r.focused(), None);

        let b = r.insert("b", state(10, 10, 1.0));
        r.apply(b, &WindowEvent::Focused(true));
        r.remove(b);
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn events_for_an_unknown_window_are_reported_not_swallowed() {
        let mut r: WindowRegistry<&str> = WindowRegistry::new();
        let a = r.insert("a", state(10, 10, 1.0));
        r.remove(a);
        assert!(!r.apply(a, &WindowEvent::RedrawRequested));
        assert!(!r.apply(WindowId::new(999, 1), &WindowEvent::CloseRequested));
    }

    #[test]
    fn the_aggregate_is_the_strongest_window_not_the_last_one() {
        let mut r: WindowRegistry<&str> = WindowRegistry::new();
        let a = r.insert("a", state(10, 10, 1.0));
        let b = r.insert("b", state(10, 10, 1.0));
        r.state_mut(a).unwrap().mark_presented();
        r.state_mut(b).unwrap().mark_presented();
        assert_eq!(r.aggregate_policy(), RedrawPolicy::Idle);

        r.state_mut(a).unwrap().set_redraw_policy(RedrawPolicy::Animating);
        assert_eq!(r.aggregate_policy(), RedrawPolicy::Animating);
        r.state_mut(b).unwrap().set_redraw_policy(RedrawPolicy::Realtime);
        assert_eq!(r.aggregate_policy(), RedrawPolicy::Realtime);
        // The strongest window going idle drops the aggregate back.
        r.state_mut(b).unwrap().set_redraw_policy(RedrawPolicy::Idle);
        assert_eq!(r.aggregate_policy(), RedrawPolicy::Animating);
    }

    #[test]
    fn redraw_dispatch_skips_windows_that_cannot_be_drawn() {
        let mut r: WindowRegistry<&str> = WindowRegistry::new();
        let a = r.insert("a", state(10, 10, 1.0));
        let b = r.insert("b", state(0, 0, 1.0));
        let c = r.insert("c", state(10, 10, 1.0));
        r.apply(c, &WindowEvent::Occluded(true));
        let pending: Vec<_> = r.needing_redraw().collect();
        assert_eq!(pending, vec![a], "zero-sized and occluded windows must be skipped");
        assert!(r.contains(b));
    }

    #[test]
    fn iteration_follows_creation_order() {
        let mut r: WindowRegistry<u8> = WindowRegistry::new();
        for i in 0..5u8 {
            r.insert(i, state(10, 10, 1.0));
        }
        let seen: Vec<u8> = r.iter().map(|(_, w, _)| *w).collect();
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
        // Removing from the middle keeps the rest in order.
        let ids: Vec<_> = r.ids().collect();
        r.remove(ids[2]);
        let seen: Vec<u8> = r.iter().map(|(_, w, _)| *w).collect();
        assert_eq!(seen, vec![0, 1, 3, 4]);
    }

    #[test]
    fn drain_hands_back_every_window_for_ordered_teardown() {
        let mut r: WindowRegistry<u8> = WindowRegistry::new();
        r.insert(1, state(10, 10, 1.0));
        let b = r.insert(2, state(10, 10, 1.0));
        r.apply(b, &WindowEvent::Focused(true));
        assert_eq!(r.drain(), vec![1, 2]);
        assert!(r.is_empty());
        assert_eq!(r.focused(), None);
    }
}
