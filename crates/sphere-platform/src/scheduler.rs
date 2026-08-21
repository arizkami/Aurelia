//! Frame scheduling: deciding when the event loop is allowed to sleep.
//!
//! A plug-in editor shares a process with a realtime audio engine. Every
//! microsecond the UI thread spends spinning is a microsecond the host's
//! scheduler has to fight for, and users notice: "this plug-in makes my CPU
//! meter jump when its window is open" is a review-killing bug. So the default
//! state of a Sphere window is *blocked*, not polling.
//!
//! Four modes, in increasing order of appetite:
//!
//! | Mode | Meaning | Loop behaviour |
//! |---|---|---|
//! | [`RedrawPolicy::Idle`] | nothing to draw | block until an event arrives |
//! | [`RedrawPolicy::Dirty`] | one frame owed | wake immediately, draw once, fall back to `Idle` |
//! | [`RedrawPolicy::Animating`] | a transition is running | wake at the next display refresh boundary |
//! | [`RedrawPolicy::Realtime`] | a meter or scope is live | never sleep |
//!
//! The modes are ordered, and the effective mode is the maximum requested by
//! anything in the application. That is what lets a single animating tooltip
//! raise the whole loop out of idle without any component having to know about
//! the others.

use core::time::Duration;
use std::time::Instant;

use crate::monitor::RefreshRate;

/// What the event loop should do when it runs out of events.
///
/// Sphere's own type: the backend translates it. Keeping it here means the
/// scheduler is testable without an event loop, and a future native backend
/// implements three cases rather than inheriting winit's semantics.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ControlFlow {
    /// Block until the window system delivers something. Zero CPU.
    Wait,
    /// Return immediately and run the loop again. Burns a core.
    Poll,
    /// Block until the given instant, or until an event arrives, whichever
    /// comes first.
    WaitUntil(Instant),
}

/// How badly the application wants to be redrawn.
///
/// Ordered so that combining requests from independent components is a
/// `max`: a scrolling list asking for [`RedrawPolicy::Animating`] cannot be
/// starved by a static panel asking for [`RedrawPolicy::Idle`].
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum RedrawPolicy {
    /// Nothing needs drawing. The loop blocks.
    #[default]
    Idle,
    /// Something changed and one frame is owed.
    Dirty,
    /// A time-based transition is running; pace it against the display.
    Animating,
    /// Continuous redraw with no pacing, for a level meter or an oscilloscope
    /// that must show every buffer. The most expensive mode; use it only while
    /// the transport is actually rolling.
    Realtime,
}

impl RedrawPolicy {
    /// True when the loop may block indefinitely.
    #[inline]
    pub const fn is_idle(self) -> bool {
        matches!(self, RedrawPolicy::Idle)
    }

    /// True when a frame should be produced without waiting for input.
    ///
    /// Combining two requests is just [`Ord::max`], which is the reason the
    /// variants are declared in increasing order of appetite.
    #[inline]
    pub const fn wants_frame(self) -> bool {
        !matches!(self, RedrawPolicy::Idle)
    }
}

/// Decides the loop's control flow and paces animation frames.
///
/// One scheduler drives the whole event loop, because the loop's sleep
/// behaviour is global; per-window appetite is aggregated into it (see
/// [`crate::WindowRegistry::aggregate_policy`]).
#[derive(Clone, Debug)]
pub struct FrameScheduler {
    /// The rate animation frames are paced against.
    refresh_rate: RefreshRate,
    /// One frame is owed. Cleared by [`FrameScheduler::frame_presented`].
    dirty: bool,
    /// Outstanding [`FrameScheduler::begin_animation`] calls.
    animations: u32,
    /// Outstanding [`FrameScheduler::begin_realtime`] calls.
    realtime: u32,
    /// A floor applied to the aggregated policy, for callers that want to pin
    /// the loop into a mode without holding a token.
    floor: RedrawPolicy,
    /// When the last frame was presented; `None` before the first one.
    last_frame: Option<Instant>,
}

impl Default for FrameScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameScheduler {
    /// A scheduler paced at 60 Hz with nothing to draw.
    pub const fn new() -> Self {
        Self {
            refresh_rate: RefreshRate::HZ_60,
            dirty: false,
            animations: 0,
            realtime: 0,
            floor: RedrawPolicy::Idle,
            last_frame: None,
        }
    }

    /// A scheduler paced at a specific display rate.
    pub const fn with_refresh_rate(rate: RefreshRate) -> Self {
        Self {
            refresh_rate: rate,
            dirty: false,
            animations: 0,
            realtime: 0,
            floor: RedrawPolicy::Idle,
            last_frame: None,
        }
    }

    /// The rate animation is paced against.
    #[inline]
    pub const fn refresh_rate(&self) -> RefreshRate {
        self.refresh_rate
    }

    /// Re-paces animation, typically after the window moved to another display.
    #[inline]
    pub fn set_refresh_rate(&mut self, rate: RefreshRate) {
        self.refresh_rate = rate;
    }

    /// The interval between paced frames.
    #[inline]
    pub fn frame_interval(&self) -> Duration {
        self.refresh_rate.frame_duration()
    }

    /// Requests exactly one more frame.
    ///
    /// Idempotent: ten widgets invalidating in one event still produce one
    /// frame, which is the whole reason redraw is a flag and not a call.
    #[inline]
    pub fn request_redraw(&mut self) {
        self.dirty = true;
    }

    /// True when a frame is owed and has not yet been presented.
    #[inline]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the owed frame without recording a presentation.
    ///
    /// The runner uses this after handing the request down to the individual
    /// windows: at that point the platform has been asked for a redraw, so the
    /// loop-level flag has done its job, but no frame has actually reached the
    /// screen and the animation pacing must not be restarted.
    #[inline]
    pub fn clear_redraw_request(&mut self) {
        self.dirty = false;
    }

    /// Registers a running animation. Must be balanced with
    /// [`FrameScheduler::end_animation`].
    ///
    /// Counted rather than boolean so two overlapping transitions cannot have
    /// the first one's completion stop the second one's pacing.
    #[inline]
    pub fn begin_animation(&mut self) {
        self.animations = self.animations.saturating_add(1);
    }

    /// Retires a running animation. Saturating: an unbalanced extra call
    /// leaves the count at zero rather than wrapping to four billion and
    /// pinning the loop into `Animating` forever.
    #[inline]
    pub fn end_animation(&mut self) {
        self.animations = self.animations.saturating_sub(1);
    }

    /// Number of animations currently registered.
    #[inline]
    pub const fn animation_count(&self) -> u32 {
        self.animations
    }

    /// Registers a realtime consumer, such as a meter fed from the audio
    /// thread. Must be balanced with [`FrameScheduler::end_realtime`].
    #[inline]
    pub fn begin_realtime(&mut self) {
        self.realtime = self.realtime.saturating_add(1);
    }

    /// Retires a realtime consumer.
    #[inline]
    pub fn end_realtime(&mut self) {
        self.realtime = self.realtime.saturating_sub(1);
    }

    /// Number of realtime consumers currently registered.
    #[inline]
    pub const fn realtime_count(&self) -> u32 {
        self.realtime
    }

    /// Pins the scheduler to at least this policy until it is lowered again.
    ///
    /// The escape hatch for code that has its own lifetime management and does
    /// not want to hold a token, and for tests.
    #[inline]
    pub fn set_floor(&mut self, policy: RedrawPolicy) {
        self.floor = policy;
    }

    /// The floor currently applied.
    #[inline]
    pub const fn floor(&self) -> RedrawPolicy {
        self.floor
    }

    /// The effective policy: the strongest of the floor, the dirty flag, and
    /// the animation and realtime tokens.
    pub fn policy(&self) -> RedrawPolicy {
        let mut p = self.floor;
        if self.dirty {
            p = p.max(RedrawPolicy::Dirty);
        }
        if self.animations > 0 {
            p = p.max(RedrawPolicy::Animating);
        }
        if self.realtime > 0 {
            p = p.max(RedrawPolicy::Realtime);
        }
        p
    }

    /// Raises the effective policy by folding in an externally computed one,
    /// such as the aggregate over all windows.
    pub fn policy_with(&self, external: RedrawPolicy) -> RedrawPolicy {
        self.policy().max(external)
    }

    /// When the next paced frame is due.
    ///
    /// The deadline is measured from the *last presented frame*, not from a
    /// fixed grid: after a stall, the next frame is due immediately and the
    /// missed ones are dropped rather than queued. Catching up would produce a
    /// burst of frames the user perceives as a jump, and on a plug-in UI it
    /// would land exactly when the machine is already overloaded.
    pub fn next_frame_time(&self, now: Instant) -> Instant {
        match self.last_frame {
            None => now,
            Some(last) => match last.checked_add(self.frame_interval()) {
                Some(next) if next > now => next,
                _ => now,
            },
        }
    }

    /// Time since the last presented frame, for animation integration.
    /// `None` before the first frame.
    pub fn since_last_frame(&self, now: Instant) -> Option<Duration> {
        self.last_frame.map(|last| now.saturating_duration_since(last))
    }

    /// True when a frame should be drawn right now.
    pub fn should_redraw(&self, now: Instant) -> bool {
        match self.policy() {
            RedrawPolicy::Idle => false,
            RedrawPolicy::Dirty | RedrawPolicy::Realtime => true,
            RedrawPolicy::Animating => now >= self.next_frame_time(now),
        }
    }

    /// Records that a frame reached the screen: clears the dirty flag and
    /// restarts the pacing interval.
    ///
    /// Call this after presenting, not before drawing. The interval should
    /// measure display-to-display, so that a slow frame shortens the following
    /// wait instead of adding to it.
    pub fn frame_presented(&mut self, now: Instant) {
        self.dirty = false;
        self.last_frame = Some(now);
    }

    /// The control flow the event loop should adopt.
    ///
    /// `external` folds in appetite computed elsewhere, typically
    /// [`crate::WindowRegistry::aggregate_policy`]. Pass
    /// [`RedrawPolicy::Idle`] when there is none.
    pub fn control_flow(&self, now: Instant, external: RedrawPolicy) -> ControlFlow {
        match self.policy_with(external) {
            // Nothing owed: block. This is the case that must produce zero CPU
            // for a static window, and it is the default.
            RedrawPolicy::Idle => ControlFlow::Wait,
            // One frame owed: come straight back round. The frame clears the
            // flag, so this lasts exactly one iteration.
            RedrawPolicy::Dirty => ControlFlow::Poll,
            // Paced: sleep until the next refresh boundary, but wake early for
            // input so a click during an animation is not delayed by a frame.
            RedrawPolicy::Animating => ControlFlow::WaitUntil(self.next_frame_time(now)),
            // Continuous.
            RedrawPolicy::Realtime => ControlFlow::Poll,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_fresh_scheduler_lets_the_loop_sleep() {
        let s = FrameScheduler::new();
        assert_eq!(s.policy(), RedrawPolicy::Idle);
        assert_eq!(s.control_flow(t0(), RedrawPolicy::Idle), ControlFlow::Wait);
        assert!(!s.should_redraw(t0()));
    }

    #[test]
    fn a_dirty_window_draws_once_then_goes_back_to_sleep() {
        // The behaviour a static window depends on: no permanent spinning.
        let now = t0();
        let mut s = FrameScheduler::new();
        s.request_redraw();
        assert_eq!(s.policy(), RedrawPolicy::Dirty);
        assert_eq!(s.control_flow(now, RedrawPolicy::Idle), ControlFlow::Poll);
        assert!(s.should_redraw(now));

        s.frame_presented(now);
        assert!(!s.is_dirty());
        assert_eq!(s.policy(), RedrawPolicy::Idle);
        assert_eq!(s.control_flow(now, RedrawPolicy::Idle), ControlFlow::Wait);
    }

    #[test]
    fn repeated_invalidation_still_owes_exactly_one_frame() {
        let mut s = FrameScheduler::new();
        for _ in 0..10 {
            s.request_redraw();
        }
        assert_eq!(s.policy(), RedrawPolicy::Dirty);
        s.frame_presented(t0());
        assert_eq!(s.policy(), RedrawPolicy::Idle);
    }

    #[test]
    fn realtime_never_sleeps_and_survives_a_presented_frame() {
        let now = t0();
        let mut s = FrameScheduler::new();
        s.begin_realtime();
        s.frame_presented(now);
        assert_eq!(s.policy(), RedrawPolicy::Realtime);
        assert_eq!(s.control_flow(now, RedrawPolicy::Idle), ControlFlow::Poll);
        assert!(s.should_redraw(now));
        s.end_realtime();
        assert_eq!(s.control_flow(now, RedrawPolicy::Idle), ControlFlow::Wait);
    }

    #[test]
    fn animation_paces_against_the_refresh_rate() {
        let now = t0();
        let mut s = FrameScheduler::with_refresh_rate(RefreshRate::HZ_120);
        s.begin_animation();
        s.frame_presented(now);

        let expected = now + Duration::from_nanos(8_333_333);
        match s.control_flow(now, RedrawPolicy::Idle) {
            ControlFlow::WaitUntil(t) => assert_eq!(t, expected),
            other => panic!("expected WaitUntil, got {other:?}"),
        }
        // Half an interval in, the frame is not due yet.
        assert!(!s.should_redraw(now + Duration::from_millis(4)));
        // Past the boundary, it is.
        assert!(s.should_redraw(now + Duration::from_millis(9)));
    }

    #[test]
    fn a_missed_deadline_does_not_accumulate_lag() {
        let now = t0();
        let mut s = FrameScheduler::with_refresh_rate(RefreshRate::HZ_60);
        s.begin_animation();
        s.frame_presented(now);
        // The loop was blocked for 200 ms; twelve frames were missed. The next
        // frame is due immediately, and exactly one is drawn, not twelve.
        let late = now + Duration::from_millis(200);
        assert_eq!(s.next_frame_time(late), late);
        assert_eq!(s.control_flow(late, RedrawPolicy::Idle), ControlFlow::WaitUntil(late));
    }

    #[test]
    fn before_the_first_frame_animation_is_due_immediately() {
        let now = t0();
        let mut s = FrameScheduler::new();
        s.begin_animation();
        assert_eq!(s.next_frame_time(now), now);
        assert!(s.should_redraw(now));
        assert_eq!(s.since_last_frame(now), None);
    }

    #[test]
    fn re_pacing_after_a_monitor_change_changes_the_deadline() {
        let now = t0();
        let mut s = FrameScheduler::with_refresh_rate(RefreshRate::HZ_60);
        s.begin_animation();
        s.frame_presented(now);
        assert_eq!(s.frame_interval(), Duration::from_nanos(16_666_666));
        s.set_refresh_rate(RefreshRate::HZ_240);
        assert_eq!(s.frame_interval(), Duration::from_nanos(4_166_666));
        assert_eq!(s.next_frame_time(now), now + Duration::from_nanos(4_166_666));
        assert_eq!(s.refresh_rate(), RefreshRate::HZ_240);
    }

    #[test]
    fn animation_tokens_are_counted_not_boolean() {
        let mut s = FrameScheduler::new();
        s.begin_animation();
        s.begin_animation();
        assert_eq!(s.animation_count(), 2);
        s.end_animation();
        // The first transition finishing must not stop the second one.
        assert_eq!(s.policy(), RedrawPolicy::Animating);
        s.end_animation();
        assert_eq!(s.policy(), RedrawPolicy::Idle);
    }

    #[test]
    fn unbalanced_end_calls_cannot_underflow_into_a_permanent_spin() {
        let mut s = FrameScheduler::new();
        s.end_animation();
        s.end_realtime();
        assert_eq!(s.animation_count(), 0);
        assert_eq!(s.realtime_count(), 0);
        assert_eq!(s.policy(), RedrawPolicy::Idle);
    }

    #[test]
    fn the_strongest_request_wins() {
        let mut s = FrameScheduler::new();
        s.request_redraw();
        s.begin_animation();
        assert_eq!(s.policy(), RedrawPolicy::Animating);
        s.begin_realtime();
        assert_eq!(s.policy(), RedrawPolicy::Realtime);
        s.end_realtime();
        s.end_animation();
        assert_eq!(s.policy(), RedrawPolicy::Dirty);
    }

    #[test]
    fn an_external_policy_can_raise_but_never_lower() {
        let now = t0();
        let mut s = FrameScheduler::new();
        s.begin_realtime();
        // A window claiming Idle cannot silence a realtime meter.
        assert_eq!(s.control_flow(now, RedrawPolicy::Idle), ControlFlow::Poll);
        s.end_realtime();
        // ...and a window claiming Realtime raises an idle scheduler.
        assert_eq!(s.control_flow(now, RedrawPolicy::Realtime), ControlFlow::Poll);
        assert_eq!(s.policy_with(RedrawPolicy::Animating), RedrawPolicy::Animating);
    }

    #[test]
    fn floor_pins_the_loop_and_can_be_released() {
        let now = t0();
        let mut s = FrameScheduler::new();
        s.set_floor(RedrawPolicy::Animating);
        assert_eq!(s.floor(), RedrawPolicy::Animating);
        assert!(matches!(s.control_flow(now, RedrawPolicy::Idle), ControlFlow::WaitUntil(_)));
        s.set_floor(RedrawPolicy::Idle);
        assert_eq!(s.control_flow(now, RedrawPolicy::Idle), ControlFlow::Wait);
    }

    #[test]
    fn policy_ordering_is_the_aggregation_rule() {
        assert!(RedrawPolicy::Idle < RedrawPolicy::Dirty);
        assert!(RedrawPolicy::Dirty < RedrawPolicy::Animating);
        assert!(RedrawPolicy::Animating < RedrawPolicy::Realtime);
        assert_eq!(RedrawPolicy::Idle.max(RedrawPolicy::Animating), RedrawPolicy::Animating);
        assert_eq!(RedrawPolicy::Realtime.max(RedrawPolicy::Dirty), RedrawPolicy::Realtime);
        assert!(RedrawPolicy::default().is_idle());
        assert!(!RedrawPolicy::Idle.wants_frame());
        assert!(RedrawPolicy::Dirty.wants_frame());
    }

    #[test]
    fn since_last_frame_measures_from_presentation() {
        let now = t0();
        let mut s = FrameScheduler::new();
        s.frame_presented(now);
        assert_eq!(
            s.since_last_frame(now + Duration::from_millis(5)),
            Some(Duration::from_millis(5))
        );
        // A clock that appears to go backwards must not panic.
        assert_eq!(s.since_last_frame(now - Duration::from_millis(5)), Some(Duration::ZERO));
    }
}
