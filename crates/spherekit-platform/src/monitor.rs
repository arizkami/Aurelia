//! Display enumeration: geometry, scale factor and refresh rate.
//!
//! Refresh rate is not decoration here. SphereKit's frame scheduler paces
//! animation against the display's actual rate, and a 144 Hz or 240 Hz panel
//! paced at a hard-coded 60 Hz looks visibly worse than one paced correctly —
//! a fader drag on a 240 Hz monitor animated at 60 Hz reads as lag, not as
//! smoothness.

use core::fmt;
use core::time::Duration;

use spherekit_core::{DevicePx, Point, Px, Rect, ScaleFactor, Size};

/// A display refresh rate, stored in millihertz.
///
/// Millihertz rather than a float because real panels report rates like
/// 59.94 Hz and 143.98 Hz, and an integer keeps equality and hashing sane while
/// still expressing them exactly as the platform reported them.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct RefreshRate(u32);

impl RefreshRate {
    /// 60 Hz.
    pub const HZ_60: Self = Self(60_000);
    /// 90 Hz.
    pub const HZ_90: Self = Self(90_000);
    /// 120 Hz.
    pub const HZ_120: Self = Self(120_000);
    /// 144 Hz.
    pub const HZ_144: Self = Self(144_000);
    /// 165 Hz.
    pub const HZ_165: Self = Self(165_000);
    /// 240 Hz.
    pub const HZ_240: Self = Self(240_000);

    /// The rates [`RefreshRate::nearest_standard`] snaps to.
    pub const STANDARD: [Self; 8] = [
        Self(30_000),
        Self::HZ_60,
        Self(75_000),
        Self::HZ_90,
        Self::HZ_120,
        Self::HZ_144,
        Self::HZ_165,
        Self::HZ_240,
    ];

    /// Wraps a raw millihertz value as reported by the platform.
    #[inline]
    pub const fn from_millihertz(mhz: u32) -> Self {
        Self(mhz)
    }

    /// Builds a rate from whole or fractional hertz.
    ///
    /// Non-finite or non-positive input yields [`RefreshRate::HZ_60`]: a
    /// zero-rate would make the frame scheduler's interval arithmetic
    /// degenerate, and platforms do transiently report nonsense while a display
    /// is being reconfigured.
    #[inline]
    pub fn from_hz(hz: f32) -> Self {
        if hz.is_finite() && hz > 0.0 { Self((hz * 1000.0).round() as u32) } else { Self::HZ_60 }
    }

    /// The raw millihertz value.
    #[inline]
    pub const fn millihertz(self) -> u32 {
        self.0
    }

    /// The rate in hertz.
    #[inline]
    pub fn hz(self) -> f32 {
        self.0 as f32 / 1000.0
    }

    /// The nominal interval between frames.
    ///
    /// A zero rate (which some virtual displays report) falls back to 60 Hz
    /// rather than dividing by zero.
    pub fn frame_duration(self) -> Duration {
        if self.0 == 0 {
            return Self::HZ_60.frame_duration();
        }
        // 1 s == 1e12 picohertz-seconds; dividing in integers keeps 59.94 Hz
        // exact to the nanosecond instead of drifting through an f32.
        Duration::from_nanos(1_000_000_000_000u64 / self.0 as u64)
    }

    /// The closest entry in [`RefreshRate::STANDARD`].
    ///
    /// Panels report 59.94 Hz and 143.981 Hz; snapping lets a UI say "144 Hz"
    /// and lets the scheduler pick a stable interval, while
    /// [`RefreshRate::frame_duration`] still uses the exact reported value.
    pub fn nearest_standard(self) -> Self {
        let mut best = Self::STANDARD[0];
        let mut best_delta = u32::MAX;
        for candidate in Self::STANDARD {
            let delta = candidate.0.abs_diff(self.0);
            if delta < best_delta {
                best_delta = delta;
                best = candidate;
            }
        }
        best
    }
}

impl Default for RefreshRate {
    fn default() -> Self {
        Self::HZ_60
    }
}

impl fmt::Display for RefreshRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3} Hz", self.hz())
    }
}

/// One mode a display can be switched into.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct VideoMode {
    /// Resolution in device pixels.
    pub size: Size<DevicePx>,
    /// Bits per pixel, summed over the colour channels.
    pub bit_depth: u16,
    /// Refresh rate in this mode.
    pub refresh_rate: RefreshRate,
}

/// A display, as the window system describes it.
#[derive(Clone, PartialEq, Debug)]
pub struct MonitorInfo {
    /// Human-readable name, when the platform provides one.
    pub name: Option<String>,
    /// Top-left corner in the desktop's device-pixel coordinate space.
    /// May be negative: a display to the left of the primary has a negative x.
    pub position: Point<DevicePx>,
    /// Extent in device pixels.
    pub size: Size<DevicePx>,
    /// The display's scale factor. Per-monitor: dragging a window between
    /// displays changes it, which is why nothing caches a global value.
    pub scale_factor: ScaleFactor,
    /// Current refresh rate, when the platform reports one. `None` on
    /// backends that do not expose it (some Wayland compositors, headless).
    pub refresh_rate: Option<RefreshRate>,
    /// True for the platform's primary display.
    pub is_primary: bool,
    /// Modes this display can be switched into. Empty when the platform does
    /// not enumerate them.
    pub video_modes: Vec<VideoMode>,
}

impl MonitorInfo {
    /// The display's rectangle in desktop device-pixel space.
    #[inline]
    pub fn bounds(&self) -> Rect<DevicePx> {
        Rect::new(self.position, self.size)
    }

    /// The display's extent in logical pixels, at its own scale factor.
    #[inline]
    pub fn logical_size(&self) -> Size<Px> {
        Size::new(
            self.scale_factor.to_logical(self.size.width),
            self.scale_factor.to_logical(self.size.height),
        )
    }

    /// The highest rate this display is capable of, preferring the enumerated
    /// modes and falling back to the current rate.
    ///
    /// A frame scheduler that wants "as smooth as this panel can go" asks for
    /// this; one that wants "match what the compositor is doing right now"
    /// reads [`MonitorInfo::refresh_rate`].
    pub fn max_refresh_rate(&self) -> Option<RefreshRate> {
        self.video_modes.iter().map(|m| m.refresh_rate).max().or(self.refresh_rate)
    }
}

/// Squared distance from a point to the nearest point of a rectangle, in
/// device pixels. Zero when the point is inside.
///
/// Computed in `i64` because a two-monitor desktop can easily span 8000 px and
/// the square of that overflows `i32`.
fn distance_sq(rect: Rect<DevicePx>, p: Point<DevicePx>) -> i64 {
    let dx = (rect.min_x().get() - p.x.get()).max(p.x.get() - rect.max_x().get()).max(0) as i64;
    let dy = (rect.min_y().get() - p.y.get()).max(p.y.get() - rect.max_y().get()).max(0) as i64;
    dx * dx + dy * dy
}

/// Area of a rectangle in device pixels, in `i64` to survive 8K displays.
fn area(rect: Rect<DevicePx>) -> i64 {
    let w = rect.width().get().max(0) as i64;
    let h = rect.height().get().max(0) as i64;
    w * h
}

/// The set of displays attached right now.
///
/// A snapshot, not a live view: displays come and go, and holding a snapshot
/// makes the selection logic pure and testable. Re-enumerate after a
/// display-change event rather than caching one of these forever.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct MonitorList {
    monitors: Vec<MonitorInfo>,
}

impl MonitorList {
    /// Builds a list from enumerated displays.
    #[inline]
    pub fn new(monitors: Vec<MonitorInfo>) -> Self {
        Self { monitors }
    }

    /// The displays, in platform enumeration order.
    #[inline]
    pub fn as_slice(&self) -> &[MonitorInfo] {
        &self.monitors
    }

    /// Number of displays.
    #[inline]
    pub fn len(&self) -> usize {
        self.monitors.len()
    }

    /// True when no display was reported. Legitimate on a headless machine, and
    /// every selection method must keep working in that case.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    /// Iterates the displays.
    pub fn iter(&self) -> core::slice::Iter<'_, MonitorInfo> {
        self.monitors.iter()
    }

    /// The primary display, falling back to the first enumerated one.
    ///
    /// The fallback matters: several Wayland compositors mark no output as
    /// primary, and "no primary display" would otherwise mean "cannot place a
    /// window".
    pub fn primary(&self) -> Option<&MonitorInfo> {
        self.monitors.iter().find(|m| m.is_primary).or_else(|| self.monitors.first())
    }

    /// The display whose bounds contain `p`.
    ///
    /// Containment is half-open, so a point on the shared edge of two adjacent
    /// displays belongs to exactly one of them.
    pub fn containing(&self, p: Point<DevicePx>) -> Option<&MonitorInfo> {
        self.monitors.iter().find(|m| m.bounds().contains(p))
    }

    /// The display containing `p`, or the closest one when `p` is in a gap
    /// between or outside all displays.
    ///
    /// Restoring a saved window position must never fail because a display was
    /// unplugged since the position was written.
    pub fn nearest(&self, p: Point<DevicePx>) -> Option<&MonitorInfo> {
        self.monitors.iter().min_by_key(|m| distance_sq(m.bounds(), p))
    }

    /// The display a window with these bounds mostly sits on.
    ///
    /// Chooses the largest overlap; with no overlap at all, falls back to the
    /// display nearest the rectangle's centre. Ties keep the earlier
    /// enumeration entry, which makes the choice deterministic.
    pub fn best_for(&self, r: Rect<DevicePx>) -> Option<&MonitorInfo> {
        // Written as a fold rather than `max_by_key` because that returns the
        // *last* maximum, which would make a tie between two identical displays
        // depend on enumeration order in the opposite direction from `nearest`.
        let mut best: Option<(&MonitorInfo, i64)> = None;
        for m in &self.monitors {
            let overlap = area(m.bounds().intersection(r));
            match best {
                Some((_, best_overlap)) if best_overlap >= overlap => {}
                _ => best = Some((m, overlap)),
            }
        }
        match best {
            Some((m, overlap)) if overlap > 0 => Some(m),
            _ => self.nearest(r.center()),
        }
    }

    /// The highest refresh rate across all displays.
    ///
    /// A window spanning a 60 Hz and a 144 Hz panel should be paced at 144 Hz;
    /// pacing at the slower rate makes the fast half stutter.
    pub fn highest_refresh_rate(&self) -> Option<RefreshRate> {
        self.monitors.iter().filter_map(|m| m.refresh_rate).max()
    }
}

impl<'a> IntoIterator for &'a MonitorList {
    type Item = &'a MonitorInfo;
    type IntoIter = core::slice::Iter<'a, MonitorInfo>;
    fn into_iter(self) -> Self::IntoIter {
        self.monitors.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{point, size};

    fn dp(v: i32) -> DevicePx {
        DevicePx(v)
    }

    fn monitor(
        name: &str,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        scale: f32,
        hz: Option<f32>,
        primary: bool,
    ) -> MonitorInfo {
        MonitorInfo {
            name: Some(name.into()),
            position: point(dp(x), dp(y)),
            size: size(dp(w), dp(h)),
            scale_factor: ScaleFactor::new(scale),
            refresh_rate: hz.map(RefreshRate::from_hz),
            is_primary: primary,
            video_modes: Vec::new(),
        }
    }

    fn two_monitor_desktop() -> MonitorList {
        // A 4K primary at the origin with a 1080p panel to its left, which is
        // the arrangement that breaks naive "position is always positive" code.
        MonitorList::new(vec![
            monitor("primary", 0, 0, 3840, 2160, 2.0, Some(144.0), true),
            monitor("left", -1920, 0, 1920, 1080, 1.0, Some(60.0), false),
        ])
    }

    #[test]
    fn frame_duration_matches_the_rate() {
        assert_eq!(RefreshRate::HZ_60.frame_duration(), Duration::from_nanos(16_666_666));
        assert_eq!(RefreshRate::HZ_120.frame_duration(), Duration::from_nanos(8_333_333));
        assert_eq!(RefreshRate::HZ_240.frame_duration(), Duration::from_nanos(4_166_666));
        // 144 Hz is the one people notice when it is wrong.
        assert_eq!(RefreshRate::HZ_144.frame_duration(), Duration::from_nanos(6_944_444));
    }

    #[test]
    fn zero_and_nonsense_rates_degrade_to_sixty() {
        assert_eq!(
            RefreshRate::from_millihertz(0).frame_duration(),
            RefreshRate::HZ_60.frame_duration()
        );
        assert_eq!(RefreshRate::from_hz(0.0), RefreshRate::HZ_60);
        assert_eq!(RefreshRate::from_hz(-144.0), RefreshRate::HZ_60);
        assert_eq!(RefreshRate::from_hz(f32::NAN), RefreshRate::HZ_60);
        assert_eq!(RefreshRate::default(), RefreshRate::HZ_60);
    }

    #[test]
    fn real_panel_rates_snap_to_the_standard_ladder() {
        assert_eq!(RefreshRate::from_hz(59.94).nearest_standard(), RefreshRate::HZ_60);
        assert_eq!(RefreshRate::from_hz(143.981).nearest_standard(), RefreshRate::HZ_144);
        assert_eq!(RefreshRate::from_hz(239.76).nearest_standard(), RefreshRate::HZ_240);
        assert_eq!(RefreshRate::from_hz(119.88).nearest_standard(), RefreshRate::HZ_120);
        // Something far off the ladder still resolves to the closest entry
        // rather than to a default.
        assert_eq!(RefreshRate::from_hz(200.0).nearest_standard(), RefreshRate::HZ_165);
        assert_eq!(
            RefreshRate::from_hz(1.0).nearest_standard(),
            RefreshRate::from_millihertz(30_000)
        );
    }

    #[test]
    fn empty_list_never_panics() {
        let list = MonitorList::default();
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert!(list.primary().is_none());
        assert!(list.containing(point(dp(0), dp(0))).is_none());
        assert!(list.nearest(point(dp(0), dp(0))).is_none());
        assert!(list.best_for(Rect::new(point(dp(0), dp(0)), size(dp(10), dp(10)))).is_none());
        assert!(list.highest_refresh_rate().is_none());
    }

    #[test]
    fn primary_falls_back_to_the_first_when_none_is_marked() {
        let list = MonitorList::new(vec![
            monitor("a", 0, 0, 800, 600, 1.0, None, false),
            monitor("b", 800, 0, 800, 600, 1.0, None, false),
        ]);
        assert_eq!(list.primary().unwrap().name.as_deref(), Some("a"));
        assert_eq!(two_monitor_desktop().primary().unwrap().name.as_deref(), Some("primary"));
    }

    #[test]
    fn containing_handles_negative_origins_and_shared_edges() {
        let list = two_monitor_desktop();
        assert_eq!(list.containing(point(dp(-1), dp(10))).unwrap().name.as_deref(), Some("left"));
        assert_eq!(
            list.containing(point(dp(10), dp(10))).unwrap().name.as_deref(),
            Some("primary")
        );
        // x == 0 is the shared edge: half-open containment gives it to the
        // display whose origin it is, and to exactly one display.
        let hits = list.iter().filter(|m| m.bounds().contains(point(dp(0), dp(0)))).count();
        assert_eq!(hits, 1);
        // Below both displays: contained by neither.
        assert!(list.containing(point(dp(10), dp(5000))).is_none());
    }

    #[test]
    fn nearest_recovers_a_point_that_is_off_every_display() {
        let list = two_monitor_desktop();
        // Far right of the primary.
        assert_eq!(
            list.nearest(point(dp(9000), dp(100))).unwrap().name.as_deref(),
            Some("primary")
        );
        // Far left of the secondary.
        assert_eq!(list.nearest(point(dp(-9000), dp(100))).unwrap().name.as_deref(), Some("left"));
        // Inside a display: distance zero wins.
        assert_eq!(list.nearest(point(dp(100), dp(100))).unwrap().name.as_deref(), Some("primary"));
    }

    #[test]
    fn best_for_picks_the_largest_overlap() {
        let list = two_monitor_desktop();
        // Mostly on the left panel: 1500 px of the 2000 px width.
        let r = Rect::new(point(dp(-1500), dp(0)), size(dp(2000), dp(500)));
        assert_eq!(list.best_for(r).unwrap().name.as_deref(), Some("left"));
        // Mostly on the primary.
        let r = Rect::new(point(dp(-100), dp(0)), size(dp(2000), dp(500)));
        assert_eq!(list.best_for(r).unwrap().name.as_deref(), Some("primary"));
    }

    #[test]
    fn best_for_falls_back_to_nearest_when_nothing_overlaps() {
        let list = two_monitor_desktop();
        // A window restored from a display that no longer exists.
        let r = Rect::new(point(dp(-9000), dp(-9000)), size(dp(100), dp(100)));
        assert_eq!(list.best_for(r).unwrap().name.as_deref(), Some("left"));
        // A zero-area rectangle overlaps nothing, so it must still resolve.
        let r = Rect::new(point(dp(10), dp(10)), size(dp(0), dp(0)));
        assert_eq!(list.best_for(r).unwrap().name.as_deref(), Some("primary"));
    }

    #[test]
    fn logical_size_divides_by_the_monitors_own_scale() {
        let list = two_monitor_desktop();
        let primary = list.primary().unwrap();
        assert_eq!(primary.logical_size(), size(Px(1920.0), Px(1080.0)));
        let left = &list.as_slice()[1];
        assert_eq!(left.logical_size(), size(Px(1920.0), Px(1080.0)));
    }

    #[test]
    fn highest_refresh_rate_ignores_displays_that_report_none() {
        let list = MonitorList::new(vec![
            monitor("a", 0, 0, 800, 600, 1.0, None, true),
            monitor("b", 800, 0, 800, 600, 1.0, Some(240.0), false),
            monitor("c", 1600, 0, 800, 600, 1.0, Some(60.0), false),
        ]);
        assert_eq!(list.highest_refresh_rate(), Some(RefreshRate::HZ_240));
    }

    #[test]
    fn max_refresh_rate_prefers_enumerated_modes() {
        let mut m = monitor("a", 0, 0, 1920, 1080, 1.0, Some(60.0), true);
        assert_eq!(m.max_refresh_rate(), Some(RefreshRate::HZ_60));
        m.video_modes = vec![
            VideoMode {
                size: size(dp(1920), dp(1080)),
                bit_depth: 24,
                refresh_rate: RefreshRate::HZ_60,
            },
            VideoMode {
                size: size(dp(1920), dp(1080)),
                bit_depth: 24,
                refresh_rate: RefreshRate::HZ_144,
            },
        ];
        assert_eq!(m.max_refresh_rate(), Some(RefreshRate::HZ_144));
    }

    #[test]
    fn distance_is_zero_inside_and_grows_outside() {
        let r = Rect::new(point(dp(0), dp(0)), size(dp(100), dp(100)));
        assert_eq!(distance_sq(r, point(dp(50), dp(50))), 0);
        assert_eq!(distance_sq(r, point(dp(103), dp(50))), 9);
        assert_eq!(distance_sq(r, point(dp(-3), dp(-4))), 25);
        // Far apart enough that an i32 square would overflow.
        let far = distance_sq(r, point(dp(100_000), dp(100_000)));
        assert!(far > i32::MAX as i64);
    }
}
