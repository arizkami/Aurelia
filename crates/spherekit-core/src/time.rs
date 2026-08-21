//! One clock, and the per-frame view of it.
//!
//! Before this module the engine ran several clocks in parallel: the frame
//! scheduler kept an `Instant`, the surface kept another to derive the `f32`
//! seconds `PaintContext::time` carries, and each example kept a third for its
//! own animation. They were sampled at different points in the same iteration
//! and nothing reconciled them.
//!
//! ## Why not `Instant`
//!
//! An animation has to be evaluable at a time a *test* chose, and `Instant`
//! cannot be constructed at an arbitrary value. [`Nanos`] can, so the whole of
//! the animation system is testable with no clock in scope at all.
//!
//! ## Why not `f32` seconds
//!
//! `PaintContext::time` is `f32` seconds since start-up. Past about four and a
//! half hours its ULP exceeds a millisecond — inside one plug-in editor
//! session. `u64` nanoseconds is exact for 584 years.
//!
//! ## Absolute at the boundary, deltas inside
//!
//! Absolute timestamps enter through [`Timeline::advance_to`], which is the one
//! place a delta is derived and the one place a long stall is clamped.
//! Everything downstream works in [`core::time::Duration`] intervals, because
//! an interval is what a spring integrates over and what a test can supply
//! without inventing a timestamp.

use core::cell::Cell;
use core::time::Duration;

/// A monotonic timestamp in nanoseconds from an arbitrary epoch.
///
/// The epoch is not defined and must not be relied on: two `Nanos` from
/// different [`Clock`]s are not comparable. Only differences are meaningful.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(transparent)]
pub struct Nanos(pub u64);

impl Nanos {
    /// The epoch itself.
    pub const ZERO: Self = Self(0);

    /// From whole milliseconds.
    #[inline]
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms.saturating_mul(1_000_000))
    }

    /// From a duration since the epoch, saturating rather than wrapping.
    #[inline]
    pub const fn from_duration(d: Duration) -> Self {
        let nanos = d.as_nanos();
        Self(if nanos > u64::MAX as u128 { u64::MAX } else { nanos as u64 })
    }

    /// As a duration since the epoch.
    #[inline]
    pub const fn as_duration(self) -> Duration {
        Duration::from_nanos(self.0)
    }

    /// The interval from `earlier` to `self`.
    ///
    /// Saturating, so a host clock that goes backwards yields zero rather than
    /// panicking. This matches [`Duration::saturating_sub`]'s contract and the
    /// frame scheduler's existing behaviour: a non-monotonic clock is a
    /// hardware or virtualisation fault the engine cannot fix, and turning it
    /// into a crash helps nobody.
    #[inline]
    pub const fn saturating_sub(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

/// The source of frame timestamps.
///
/// Exists so that nothing below the `spherekit` facade calls `Instant::now`. A
/// plug-in whose host owns the transport can supply the host's clock and get
/// animation that stays in step with it.
pub trait Clock {
    /// The current time.
    fn now(&self) -> Nanos;
}

/// A clock backed by [`std::time::Instant`].
#[derive(Debug)]
pub struct SystemClock {
    origin: std::time::Instant,
}

impl SystemClock {
    /// Starts a clock whose epoch is now.
    pub fn new() -> Self {
        Self { origin: std::time::Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    #[inline]
    fn now(&self) -> Nanos {
        Nanos::from_duration(self.origin.elapsed())
    }
}

/// A clock a test drives by hand.
///
/// Mutates through a [`Cell`] so it can be advanced while something else holds
/// a `&dyn Clock` to it, which is what a test that steps a whole frame loop
/// needs.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: Cell<Nanos>,
}

impl ManualClock {
    /// Starts at the epoch.
    pub const fn new() -> Self {
        Self { now: Cell::new(Nanos::ZERO) }
    }

    /// Moves forward by `by`.
    pub fn advance(&self, by: Duration) {
        let next = self.now.get().0.saturating_add(Nanos::from_duration(by).0);
        self.now.set(Nanos(next));
    }

    /// Jumps to an absolute time.
    pub fn set(&self, to: Nanos) {
        self.now.set(to);
    }
}

impl Clock for ManualClock {
    #[inline]
    fn now(&self) -> Nanos {
        self.now.get()
    }
}

/// One frame's view of time.
///
/// The animator, the paint pass and the batch compiler all read this same
/// value, which is what stops them drifting apart.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct FrameTime {
    /// Since the timeline's first frame.
    pub elapsed: Duration,
    /// Since the previous frame, clamped to [`Timeline::max_step`].
    pub delta: Duration,
    /// Monotonically increasing frame index, starting at zero.
    pub frame: u64,
}

impl FrameTime {
    /// Elapsed seconds as `f32`.
    ///
    /// Knowingly imprecise past a few hours; it exists because
    /// `PaintContext::time` is `f32` and predates this module. New code should
    /// read [`FrameTime::elapsed`].
    #[inline]
    pub fn elapsed_secs_f32(self) -> f32 {
        self.elapsed.as_secs_f32()
    }

    /// Frame delta in seconds as `f32`.
    #[inline]
    pub fn delta_secs_f32(self) -> f32 {
        self.delta.as_secs_f32()
    }
}

/// Turns a stream of absolute timestamps into per-frame deltas.
#[derive(Debug)]
pub struct Timeline {
    origin: Option<Nanos>,
    last: Option<Nanos>,
    current: FrameTime,
    max_step: Duration,
}

impl Timeline {
    /// The longest delta any single frame may report.
    ///
    /// Nothing numerical needs this: the spring step is analytic and stable at
    /// any interval. It exists so that a five-second debugger pause, a
    /// minimised window or a laptop resuming from sleep does not make every
    /// animation appear to have already finished. The user sees motion resume
    /// from where it was rather than having silently completed while they were
    /// not looking.
    pub const DEFAULT_MAX_STEP: Duration = Duration::from_millis(250);

    /// A timeline whose first `advance_to` becomes its origin.
    pub const fn new() -> Self {
        Self::with_max_step(Self::DEFAULT_MAX_STEP)
    }

    /// A timeline with a custom clamp.
    pub const fn with_max_step(max_step: Duration) -> Self {
        Self {
            origin: None,
            last: None,
            current: FrameTime { elapsed: Duration::ZERO, delta: Duration::ZERO, frame: 0 },
            max_step,
        }
    }

    /// The clamp this timeline applies.
    #[inline]
    pub const fn max_step(&self) -> Duration {
        self.max_step
    }

    /// Advances to an absolute timestamp and returns the frame's view of time.
    ///
    /// The first call establishes the origin and reports a zero delta: there is
    /// no previous frame to have elapsed from, and reporting the time since the
    /// process started would make every animation jump on its first frame.
    ///
    /// A repeated or backwards timestamp also yields a zero delta rather than
    /// an error. Two renders in one frame is a legitimate thing for a host to
    /// ask for, and it must not advance the animation twice.
    pub fn advance_to(&mut self, now: Nanos) -> FrameTime {
        let origin = *self.origin.get_or_insert(now);
        let delta = match self.last {
            Some(last) => now.saturating_sub(last).min(self.max_step),
            None => Duration::ZERO,
        };
        self.last = Some(now.max(self.last.unwrap_or(now)));
        self.current = FrameTime {
            elapsed: now.saturating_sub(origin),
            delta,
            frame: self.current.frame.saturating_add(1),
        };
        self.current
    }

    /// Advances by an interval, with no clock involved.
    ///
    /// The test path, and the path for a host that hands out deltas rather than
    /// timestamps. Applies the same clamp, so a test cannot accidentally exceed
    /// what production would allow.
    pub fn advance_by(&mut self, delta: Duration) -> FrameTime {
        let delta = delta.min(self.max_step);
        self.current = FrameTime {
            elapsed: self.current.elapsed.saturating_add(delta),
            delta,
            frame: self.current.frame.saturating_add(1),
        };
        // Keep the absolute path consistent if the two are ever mixed: the next
        // `advance_to` must not report the whole interval since the origin as
        // one delta.
        if let Some(last) = self.last {
            self.last = Some(Nanos(last.0.saturating_add(Nanos::from_duration(delta).0)));
        }
        self.current
    }

    /// The most recent frame's view of time.
    #[inline]
    pub const fn current(&self) -> FrameTime {
        self.current
    }

    /// Forgets the origin and the last stamp, keeping the clamp.
    ///
    /// For a surface whose window was recreated: the frame counter restarting
    /// is correct there, because every motion keyed on the old window is gone
    /// too.
    pub fn reset(&mut self) {
        self.origin = None;
        self.last = None;
        self.current = FrameTime::default();
    }
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_frame_has_no_delta() {
        // There is no previous frame to have elapsed from. Reporting the time
        // since the process started would make every animation jump on its
        // first frame.
        let mut t = Timeline::new();
        let f = t.advance_to(Nanos::from_millis(9_000));
        assert_eq!(f.delta, Duration::ZERO);
        assert_eq!(f.elapsed, Duration::ZERO, "the first stamp is the origin");
        assert_eq!(f.frame, 1);
    }

    #[test]
    fn elapsed_is_measured_from_the_first_frame_not_from_the_epoch() {
        let mut t = Timeline::new();
        t.advance_to(Nanos::from_millis(9_000));
        let f = t.advance_to(Nanos::from_millis(9_016));
        assert_eq!(f.elapsed, Duration::from_millis(16));
        assert_eq!(f.delta, Duration::from_millis(16));
    }

    #[test]
    fn a_long_stall_is_clamped_so_animations_do_not_finish_while_hidden() {
        let mut t = Timeline::new();
        t.advance_to(Nanos::from_millis(0));
        let f = t.advance_to(Nanos::from_millis(5_000));
        assert_eq!(f.delta, Timeline::DEFAULT_MAX_STEP);
        // Elapsed is *not* clamped: it is an absolute position, and clamping it
        // would make the timeline disagree with the wall clock forever.
        assert_eq!(f.elapsed, Duration::from_millis(5_000));
    }

    #[test]
    fn a_repeated_timestamp_yields_a_zero_delta() {
        // Two renders in one frame is a legitimate host request and must not
        // advance the animation twice.
        let mut t = Timeline::new();
        t.advance_to(Nanos::from_millis(100));
        t.advance_to(Nanos::from_millis(116));
        let f = t.advance_to(Nanos::from_millis(116));
        assert_eq!(f.delta, Duration::ZERO);
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_panic_or_rewind() {
        let mut t = Timeline::new();
        t.advance_to(Nanos::from_millis(100));
        t.advance_to(Nanos::from_millis(200));
        let f = t.advance_to(Nanos::from_millis(150));
        assert_eq!(f.delta, Duration::ZERO, "no negative interval, no panic");
        // The high-water mark is kept, so the next forward stamp measures from
        // there rather than replaying the interval.
        let g = t.advance_to(Nanos::from_millis(216));
        assert_eq!(g.delta, Duration::from_millis(16));
    }

    #[test]
    fn the_frame_counter_increments_on_every_advance() {
        let mut t = Timeline::new();
        for expected in 1..=5u64 {
            assert_eq!(t.advance_to(Nanos::from_millis(expected * 16)).frame, expected);
        }
    }

    #[test]
    fn advancing_by_an_interval_needs_no_clock() {
        let mut t = Timeline::new();
        let f = t.advance_by(Duration::from_millis(16));
        assert_eq!(f.delta, Duration::from_millis(16));
        assert_eq!(f.elapsed, Duration::from_millis(16));
        assert_eq!(t.advance_by(Duration::from_millis(16)).elapsed, Duration::from_millis(32));
    }

    #[test]
    fn the_step_clamp_applies_to_the_test_path_too() {
        // Or a test could exercise an interval production can never produce.
        let mut t = Timeline::new();
        assert_eq!(t.advance_by(Duration::from_secs(10)).delta, Timeline::DEFAULT_MAX_STEP);
    }

    #[test]
    fn a_manual_clock_reports_exactly_what_it_was_told() {
        let c = ManualClock::new();
        assert_eq!(c.now(), Nanos::ZERO);
        c.advance(Duration::from_millis(16));
        assert_eq!(c.now(), Nanos::from_millis(16));
        c.set(Nanos::from_millis(1_000));
        assert_eq!(c.now(), Nanos::from_millis(1_000));
    }

    #[test]
    fn a_manual_clock_advances_through_a_shared_reference() {
        // The property that makes it usable while something else holds a
        // `&dyn Clock` to it.
        let c = ManualClock::new();
        let borrowed: &dyn Clock = &c;
        c.advance(Duration::from_millis(8));
        assert_eq!(borrowed.now(), Nanos::from_millis(8));
    }

    #[test]
    fn a_system_clock_starts_at_its_own_epoch_and_moves_forward() {
        let c = SystemClock::new();
        let a = c.now();
        let b = c.now();
        assert!(b >= a, "monotonic");
        assert!(a.0 < Duration::from_secs(1).as_nanos() as u64, "epoch is construction time");
    }

    #[test]
    fn nanos_subtraction_saturates_rather_than_underflowing() {
        assert_eq!(Nanos::from_millis(5).saturating_sub(Nanos::from_millis(9)), Duration::ZERO);
    }

    #[test]
    fn resetting_forgets_the_origin_but_keeps_the_clamp() {
        let mut t = Timeline::with_max_step(Duration::from_millis(50));
        t.advance_to(Nanos::from_millis(1_000));
        t.advance_to(Nanos::from_millis(1_016));
        t.reset();
        let f = t.advance_to(Nanos::from_millis(9_000));
        assert_eq!(f.elapsed, Duration::ZERO);
        assert_eq!(f.frame, 1);
        assert_eq!(t.max_step(), Duration::from_millis(50));
    }
}
