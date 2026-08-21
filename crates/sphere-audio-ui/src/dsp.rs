//! The measurement maths behind the visualisations.
//!
//! Kept separate from the drawing because it is the part that is easy to get
//! subtly, invisibly wrong. A meter whose scale is linear in amplitude looks
//! plausible and is useless: everything below −20 dB collapses into the bottom
//! two per cent of the widget, which is precisely the range a mix engineer
//! spends their time in.
//!
//! Everything here is pure and unit-tested. None of it touches a canvas.

use sphere_core::Px;

/// The floor of the dB scale.
///
/// Digital silence is negative infinity, which no widget can plot. −100 dB is
/// far below the noise floor of any real signal path, so clamping there loses
/// nothing and keeps every downstream calculation finite.
pub const MIN_DB: f32 = -100.0;

/// Converts linear amplitude to decibels.
///
/// Silence and negative amplitudes both map to [`MIN_DB`] rather than to
/// negative infinity or NaN, because those propagate into vertex positions and
/// corrupt an entire draw call.
#[inline]
pub fn linear_to_db(amplitude: f32) -> f32 {
    if !amplitude.is_finite() || amplitude <= 0.0 {
        return MIN_DB;
    }
    (20.0 * amplitude.log10()).max(MIN_DB)
}

/// Converts decibels to linear amplitude.
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if !db.is_finite() || db <= MIN_DB {
        return 0.0;
    }
    10.0f32.powf(db / 20.0)
}

/// Maps a decibel range onto a widget's length.
///
/// The mapping is not linear in dB either. Meters devote more space to the top
/// of the range, because the difference between −3 and 0 dB matters far more
/// than the difference between −60 and −57. [`MeterScale::skew`] controls how
/// much; `1.0` is a plain linear-in-dB scale.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MeterScale {
    /// Bottom of the visible range, in dB.
    pub min_db: f32,
    /// Top of the visible range, in dB. Usually `0.0`, or a few dB above for
    /// headroom on a peak meter.
    pub max_db: f32,
    /// Exponent applied to the normalised position.
    ///
    /// Values below `1.0` expand the top of the range. `0.6` is close to the
    /// weighting of a traditional hardware meter face.
    pub skew: f32,
}

impl Default for MeterScale {
    fn default() -> Self {
        // −60 to 0 dB is the range a channel meter in a DAW conventionally
        // shows; +6 of headroom above unity is left to the peak indicator.
        Self { min_db: -60.0, max_db: 0.0, skew: 0.6 }
    }
}

impl MeterScale {
    /// A scale over the given dB range with no skew.
    pub fn linear_db(min_db: f32, max_db: f32) -> Self {
        Self { min_db, max_db, skew: 1.0 }
    }

    /// Maps a dB value to a `0..=1` position along the meter.
    pub fn position_of_db(&self, db: f32) -> f32 {
        let span = self.max_db - self.min_db;
        if span.abs() < f32::EPSILON || !db.is_finite() {
            return 0.0;
        }
        let t = ((db - self.min_db) / span).clamp(0.0, 1.0);
        let skew = if self.skew.is_finite() && self.skew > 0.0 { self.skew } else { 1.0 };
        t.powf(skew)
    }

    /// Maps a linear amplitude to a `0..=1` position along the meter.
    #[inline]
    pub fn position_of(&self, amplitude: f32) -> f32 {
        self.position_of_db(linear_to_db(amplitude))
    }

    /// The inverse of [`MeterScale::position_of_db`], for hit testing a scale.
    pub fn db_at_position(&self, position: f32) -> f32 {
        let skew = if self.skew.is_finite() && self.skew > 0.0 { self.skew } else { 1.0 };
        let t = position.clamp(0.0, 1.0).powf(1.0 / skew);
        self.min_db + t * (self.max_db - self.min_db)
    }

    /// dB values worth drawing a tick at, coarsest first.
    ///
    /// Returns only the ticks that fall inside the range, so a meter configured
    /// for −20..0 does not try to label −60.
    pub fn ticks(&self) -> impl Iterator<Item = f32> + '_ {
        const CANDIDATES: [f32; 13] =
            [6.0, 3.0, 0.0, -3.0, -6.0, -10.0, -15.0, -20.0, -30.0, -40.0, -50.0, -60.0, -80.0];
        CANDIDATES.into_iter().filter(move |db| *db >= self.min_db && *db <= self.max_db)
    }
}

/// Peak and RMS ballistics: how a meter's displayed value chases the real one.
///
/// A meter that simply shows the current block's peak flickers unreadably. Real
/// meters rise instantly and fall slowly, and hold the highest recent peak for
/// a moment so the eye can catch it.
#[derive(Copy, Clone, Debug)]
pub struct MeterBallistics {
    /// How far the displayed level falls per second, in dB.
    ///
    /// 20 dB/s is the IEC standard for a PPM-style meter and reads as smooth
    /// without lagging the music.
    pub release_db_per_second: f32,
    /// How long the peak marker stays before it starts falling, in seconds.
    pub peak_hold_seconds: f32,
    /// How fast the peak marker falls once the hold expires, in dB per second.
    pub peak_fall_db_per_second: f32,
    /// How long the clip indicator stays lit, in seconds.
    pub clip_hold_seconds: f32,
}

impl Default for MeterBallistics {
    fn default() -> Self {
        Self {
            release_db_per_second: 20.0,
            peak_hold_seconds: 1.5,
            peak_fall_db_per_second: 12.0,
            clip_hold_seconds: 2.0,
        }
    }
}

/// The animated state of one meter channel.
///
/// Lives on the UI thread and is advanced once per frame from the newest
/// snapshot. Deliberately not on the audio thread: ballistics depend on frame
/// timing, and the audio thread has no idea when a frame happened.
#[derive(Copy, Clone, Debug, Default)]
pub struct MeterState {
    /// The currently displayed level, in dB.
    pub level_db: f32,
    /// The held peak marker, in dB.
    pub peak_db: f32,
    /// Seconds remaining on the peak hold.
    peak_hold_remaining: f32,
    /// Seconds remaining on the clip indicator.
    clip_remaining: f32,
    /// Whether this state has ever been advanced.
    started: bool,
}

impl MeterState {
    /// A meter sitting at silence.
    pub fn silent() -> Self {
        Self {
            level_db: MIN_DB,
            peak_db: MIN_DB,
            peak_hold_remaining: 0.0,
            clip_remaining: 0.0,
            started: true,
        }
    }

    /// Advances the meter by `dt` seconds toward `target_db`.
    ///
    /// Rise is instant, fall is rate-limited. A rate rather than an exponential
    /// decay because dB-per-second is what meter specifications are written in,
    /// and it makes the fall time predictable regardless of how far it has to
    /// travel.
    pub fn advance(&mut self, target_db: f32, clipped: bool, dt: f32, b: &MeterBallistics) {
        if !self.started {
            *self = Self::silent();
        }
        let dt = if dt.is_finite() { dt.clamp(0.0, 0.25) } else { 0.0 };
        let target = if target_db.is_finite() { target_db.max(MIN_DB) } else { MIN_DB };

        self.level_db = if target >= self.level_db {
            target
        } else {
            (self.level_db - b.release_db_per_second * dt).max(target)
        };

        if target >= self.peak_db {
            self.peak_db = target;
            self.peak_hold_remaining = b.peak_hold_seconds;
        } else if self.peak_hold_remaining > 0.0 {
            self.peak_hold_remaining -= dt;
        } else {
            self.peak_db = (self.peak_db - b.peak_fall_db_per_second * dt).max(self.level_db);
        }

        if clipped {
            self.clip_remaining = b.clip_hold_seconds;
        } else if self.clip_remaining > 0.0 {
            self.clip_remaining -= dt;
        }
    }

    /// True while the clip indicator should be lit.
    #[inline]
    pub fn clipping(&self) -> bool {
        self.clip_remaining > 0.0
    }

    /// Clears the clip indicator, for a click-to-reset affordance.
    #[inline]
    pub fn clear_clip(&mut self) {
        self.clip_remaining = 0.0;
    }
}

/// Maps a frequency range onto a widget's width, logarithmically.
///
/// Frequency must be logarithmic or a spectrum is unreadable: linearly, half
/// the width goes to 10–20 kHz, where almost nothing musically interesting
/// happens, and the two octaves that carry the bass are squeezed into a few
/// pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LogFrequencyScale {
    /// Lowest frequency shown, in hertz.
    pub min_hz: f32,
    /// Highest frequency shown, in hertz.
    pub max_hz: f32,
}

impl Default for LogFrequencyScale {
    fn default() -> Self {
        // The conventional audible range, and what every analyser draws.
        Self { min_hz: 20.0, max_hz: 20_000.0 }
    }
}

impl LogFrequencyScale {
    /// A scale over the given range.
    ///
    /// Both bounds are clamped above zero: `log10(0)` is negative infinity, and
    /// a single infinity here becomes a NaN vertex position downstream.
    pub fn new(min_hz: f32, max_hz: f32) -> Self {
        let min = if min_hz.is_finite() { min_hz.max(1.0) } else { 20.0 };
        let max = if max_hz.is_finite() { max_hz.max(min * 1.0001) } else { 20_000.0 };
        Self { min_hz: min, max_hz: max }
    }

    /// Maps a frequency to a `0..=1` position.
    pub fn position_of(&self, hz: f32) -> f32 {
        if !hz.is_finite() || hz <= 0.0 {
            return 0.0;
        }
        let lo = self.min_hz.log10();
        let hi = self.max_hz.log10();
        let span = hi - lo;
        if span.abs() < f32::EPSILON {
            return 0.0;
        }
        ((hz.log10() - lo) / span).clamp(0.0, 1.0)
    }

    /// The inverse of [`LogFrequencyScale::position_of`].
    pub fn hz_at_position(&self, position: f32) -> f32 {
        let lo = self.min_hz.log10();
        let hi = self.max_hz.log10();
        10.0f32.powf(lo + position.clamp(0.0, 1.0) * (hi - lo))
    }

    /// Frequencies worth drawing a gridline at, within the range.
    ///
    /// The conventional 1-2-5 decade sequence, which is what every analyser
    /// uses and what a reader's eye expects.
    pub fn gridlines(&self) -> impl Iterator<Item = f32> + '_ {
        const CANDIDATES: [f32; 25] = [
            10.0, 20.0, 30.0, 50.0, 100.0, 200.0, 300.0, 500.0, 1_000.0, 2_000.0, 3_000.0, 5_000.0,
            10_000.0, 20_000.0, 40.0, 60.0, 80.0, 400.0, 600.0, 800.0, 4_000.0, 6_000.0, 8_000.0,
            15_000.0, 150.0,
        ];
        CANDIDATES.into_iter().filter(move |hz| *hz >= self.min_hz && *hz <= self.max_hz)
    }

    /// Maps a frequency directly to an x offset within `width`.
    #[inline]
    pub fn x_of(&self, hz: f32, width: Px) -> Px {
        Px(self.position_of(hz) * width.get())
    }
}

/// Reduces `samples` to per-column minimum and maximum pairs.
///
/// This is the correct way to draw a waveform, and decimation is not. Taking
/// every Nth sample makes a waveform *look* thinner than it is and drops
/// transients entirely — the click at the start of a snare simply vanishes.
/// Taking the extremes of each column preserves the envelope exactly.
///
/// Writes `2 * columns` values into `out` as `[min, max]` pairs. Non-finite
/// samples are skipped rather than allowed to become vertex coordinates.
pub fn min_max_envelope(samples: &[f32], columns: usize, out: &mut Vec<f32>) {
    out.clear();
    if columns == 0 {
        return;
    }
    out.reserve(columns * 2);
    if samples.is_empty() {
        out.resize(columns * 2, 0.0);
        return;
    }

    for column in 0..columns {
        // Integer arithmetic on the boundaries so every sample lands in exactly
        // one column and none is skipped between columns.
        let start = column * samples.len() / columns;
        let end = ((column + 1) * samples.len() / columns).max(start + 1).min(samples.len());

        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for &s in &samples[start..end] {
            if !s.is_finite() {
                continue;
            }
            if s < min {
                min = s;
            }
            if s > max {
                max = s;
            }
        }
        if min > max {
            // Every sample in this column was non-finite.
            min = 0.0;
            max = 0.0;
        }
        out.push(min);
        out.push(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_conversion_round_trips() {
        for db in [-60.0, -20.0, -6.0, -3.0, 0.0] {
            let back = linear_to_db(db_to_linear(db));
            assert!((back - db).abs() < 1e-3, "{db} -> {back}");
        }
    }

    #[test]
    fn known_db_values_are_exact() {
        assert!((linear_to_db(1.0) - 0.0).abs() < 1e-5);
        assert!((linear_to_db(0.5) + 6.0206).abs() < 1e-3, "{}", linear_to_db(0.5));
        assert!((db_to_linear(-6.0206) - 0.5).abs() < 1e-4);
        assert!((linear_to_db(2.0) - 6.0206).abs() < 1e-3);
    }

    #[test]
    fn silence_and_garbage_clamp_instead_of_producing_infinities() {
        // A single NaN reaching a vertex position corrupts a whole draw call.
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let db = linear_to_db(bad);
            assert!(db.is_finite(), "linear_to_db({bad}) = {db}");
            assert!(db <= MIN_DB + 1e-3);
        }
        for bad in [f32::NAN, f32::NEG_INFINITY, -1000.0] {
            assert_eq!(db_to_linear(bad), 0.0, "db_to_linear({bad})");
        }
    }

    #[test]
    fn meter_scale_spans_its_range_end_to_end() {
        let s = MeterScale::default();
        assert!((s.position_of_db(s.max_db) - 1.0).abs() < 1e-5);
        assert!(s.position_of_db(s.min_db).abs() < 1e-5);
    }

    #[test]
    fn meter_scale_clamps_outside_its_range() {
        let s = MeterScale::default();
        assert_eq!(s.position_of_db(50.0), 1.0);
        assert_eq!(s.position_of_db(-500.0), 0.0);
    }

    #[test]
    fn a_skewed_meter_gives_the_top_of_the_range_more_room() {
        // The whole reason skew exists: -3 dB must not sit at 95 % of the way
        // up with everything interesting crushed against the top.
        let skewed = MeterScale::default();
        let linear = MeterScale::linear_db(-60.0, 0.0);
        let at_minus_20 = skewed.position_of_db(-20.0);
        assert!(
            at_minus_20 > linear.position_of_db(-20.0),
            "skew should push -20 dB higher up the meter: {at_minus_20}"
        );
        assert!(at_minus_20 < 1.0);
    }

    #[test]
    fn meter_scale_position_round_trips_through_its_inverse() {
        let s = MeterScale::default();
        for db in [-60.0, -40.0, -20.0, -12.0, -6.0, -3.0, 0.0] {
            let back = s.db_at_position(s.position_of_db(db));
            assert!((back - db).abs() < 1e-2, "{db} -> {back}");
        }
    }

    #[test]
    fn a_degenerate_meter_scale_does_not_divide_by_zero() {
        let s = MeterScale { min_db: -20.0, max_db: -20.0, skew: 1.0 };
        assert!(s.position_of_db(-20.0).is_finite());
        let bad_skew = MeterScale { min_db: -60.0, max_db: 0.0, skew: 0.0 };
        assert!(bad_skew.position_of_db(-30.0).is_finite());
        let nan_skew = MeterScale { min_db: -60.0, max_db: 0.0, skew: f32::NAN };
        assert!(nan_skew.position_of_db(-30.0).is_finite());
    }

    #[test]
    fn meter_ticks_stay_inside_the_configured_range() {
        let s = MeterScale::linear_db(-20.0, 0.0);
        let ticks: Vec<f32> = s.ticks().collect();
        assert!(ticks.iter().all(|db| *db >= -20.0 && *db <= 0.0), "{ticks:?}");
        assert!(ticks.contains(&0.0));
        assert!(!ticks.contains(&-60.0));
    }

    #[test]
    fn a_meter_rises_instantly_and_falls_gradually() {
        let b = MeterBallistics::default();
        let mut m = MeterState::silent();
        m.advance(-6.0, false, 1.0 / 60.0, &b);
        assert!((m.level_db + 6.0).abs() < 1e-4, "rise must be instant: {}", m.level_db);

        m.advance(MIN_DB, false, 1.0 / 60.0, &b);
        assert!(m.level_db < -6.0, "must have fallen");
        assert!(m.level_db > -10.0, "must not have fallen instantly: {}", m.level_db);
    }

    #[test]
    fn the_release_rate_is_what_it_says_it_is() {
        let b = MeterBallistics { release_db_per_second: 20.0, ..Default::default() };
        let mut m = MeterState::silent();
        m.advance(0.0, false, 0.0, &b);
        // One second of silence at 20 dB/s.
        for _ in 0..100 {
            m.advance(MIN_DB, false, 0.01, &b);
        }
        assert!((m.level_db + 20.0).abs() < 0.5, "expected about -20 dB, got {}", m.level_db);
    }

    #[test]
    fn the_peak_marker_holds_then_falls() {
        let b = MeterBallistics {
            peak_hold_seconds: 0.5,
            peak_fall_db_per_second: 10.0,
            ..Default::default()
        };
        let mut m = MeterState::silent();
        m.advance(0.0, false, 0.0, &b);
        assert!((m.peak_db).abs() < 1e-4);

        // Still inside the hold window.
        for _ in 0..40 {
            m.advance(-40.0, false, 0.01, &b);
        }
        assert!(m.peak_db > -1.0, "the peak fell during its hold: {}", m.peak_db);

        // Well past it.
        for _ in 0..100 {
            m.advance(-40.0, false, 0.01, &b);
        }
        assert!(m.peak_db < -1.0, "the peak never fell: {}", m.peak_db);
    }

    #[test]
    fn the_peak_marker_never_sinks_below_the_level() {
        let b = MeterBallistics::default();
        let mut m = MeterState::silent();
        for i in 0..500 {
            let target = if i % 50 == 0 { -3.0 } else { -30.0 };
            m.advance(target, false, 0.01, &b);
            assert!(
                m.peak_db >= m.level_db - 1e-3,
                "peak {} below level {}",
                m.peak_db,
                m.level_db
            );
        }
    }

    #[test]
    fn the_clip_indicator_holds_and_can_be_cleared() {
        let b = MeterBallistics { clip_hold_seconds: 0.2, ..Default::default() };
        let mut m = MeterState::silent();
        m.advance(0.0, true, 0.01, &b);
        assert!(m.clipping());
        for _ in 0..10 {
            m.advance(-40.0, false, 0.01, &b);
        }
        assert!(m.clipping(), "the clip light went out too soon");
        for _ in 0..20 {
            m.advance(-40.0, false, 0.01, &b);
        }
        assert!(!m.clipping());

        m.advance(0.0, true, 0.01, &b);
        assert!(m.clipping());
        m.clear_clip();
        assert!(!m.clipping());
    }

    #[test]
    fn a_meter_survives_a_garbage_delta_or_target() {
        let b = MeterBallistics::default();
        let mut m = MeterState::silent();
        m.advance(f32::NAN, false, 0.016, &b);
        assert!(m.level_db.is_finite());
        m.advance(-6.0, false, f32::NAN, &b);
        assert!(m.level_db.is_finite());
        // A huge delta after a stall must not teleport the meter past its target.
        m.advance(-60.0, false, 1000.0, &b);
        assert!(m.level_db >= -60.0 - 1e-3, "{}", m.level_db);
    }

    #[test]
    fn a_default_meter_state_advances_from_silence() {
        // `Default` gives zeroes, which would mean 0 dB — full scale. Advancing
        // must correct that rather than flashing a full meter on the first frame.
        let mut m = MeterState::default();
        m.advance(MIN_DB, false, 0.016, &MeterBallistics::default());
        assert!(m.level_db < -50.0, "an uninitialised meter showed {} dB", m.level_db);
    }

    #[test]
    fn log_frequency_spans_its_range_end_to_end() {
        let s = LogFrequencyScale::default();
        assert!(s.position_of(20.0).abs() < 1e-5);
        assert!((s.position_of(20_000.0) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn every_octave_gets_equal_width() {
        // The defining property of a log scale, and the reason it is used.
        let s = LogFrequencyScale::new(100.0, 1600.0); // four octaves
        let step = s.position_of(200.0) - s.position_of(100.0);
        for (a, b) in [(200.0, 400.0), (400.0, 800.0), (800.0, 1600.0)] {
            let d = s.position_of(b) - s.position_of(a);
            assert!((d - step).abs() < 1e-4, "{a}->{b} spans {d}, expected {step}");
        }
    }

    #[test]
    fn log_frequency_round_trips() {
        let s = LogFrequencyScale::default();
        for hz in [20.0, 100.0, 440.0, 1_000.0, 10_000.0, 20_000.0] {
            let back = s.hz_at_position(s.position_of(hz));
            assert!((back - hz).abs() / hz < 1e-3, "{hz} -> {back}");
        }
    }

    #[test]
    fn a_degenerate_frequency_range_is_repaired_not_propagated() {
        // log10(0) is -inf; one of those becomes a NaN vertex position.
        let s = LogFrequencyScale::new(0.0, 0.0);
        assert!(s.min_hz > 0.0 && s.max_hz > s.min_hz);
        assert!(s.position_of(1000.0).is_finite());

        let nan = LogFrequencyScale::new(f32::NAN, f32::NAN);
        assert!(nan.position_of(1000.0).is_finite());
        assert_eq!(LogFrequencyScale::default().position_of(0.0), 0.0);
        assert_eq!(LogFrequencyScale::default().position_of(f32::NAN), 0.0);
    }

    #[test]
    fn gridlines_stay_inside_the_range() {
        let s = LogFrequencyScale::new(100.0, 5_000.0);
        let lines: Vec<f32> = s.gridlines().collect();
        assert!(lines.iter().all(|hz| *hz >= 100.0 && *hz <= 5_000.0), "{lines:?}");
        assert!(lines.contains(&1_000.0));
        assert!(!lines.contains(&20.0));
    }

    #[test]
    fn the_envelope_brackets_every_sample() {
        // The correctness property that distinguishes min/max from decimation.
        let samples: Vec<f32> =
            (0..1000).map(|i| (i as f32 * 0.1).sin() * if i == 500 { 1.0 } else { 0.3 }).collect();
        let mut out = Vec::new();
        min_max_envelope(&samples, 50, &mut out);
        assert_eq!(out.len(), 100);

        for (column, pair) in out.chunks_exact(2).enumerate() {
            let start = column * samples.len() / 50;
            let end = (column + 1) * samples.len() / 50;
            for &s in &samples[start..end] {
                assert!(s >= pair[0] - 1e-6 && s <= pair[1] + 1e-6, "sample {s} outside {pair:?}");
            }
        }
    }

    #[test]
    fn the_envelope_preserves_a_lone_transient() {
        // Decimation would drop this entirely, which is why it is not used.
        let mut samples = vec![0.01f32; 1000];
        samples[499] = 0.95;
        let mut out = Vec::new();
        min_max_envelope(&samples, 20, &mut out);
        let loudest = out.chunks_exact(2).map(|p| p[1]).fold(f32::MIN, f32::max);
        assert!((loudest - 0.95).abs() < 1e-6, "the transient was lost: {loudest}");
    }

    #[test]
    fn the_envelope_covers_every_sample_with_no_gaps() {
        // If column boundaries do not tile, samples fall between columns and
        // the waveform silently loses detail.
        let samples: Vec<f32> = (0..997).map(|i| i as f32).collect();
        let columns = 33;
        let mut covered = vec![false; samples.len()];
        for column in 0..columns {
            let start = column * samples.len() / columns;
            let end = ((column + 1) * samples.len() / columns).max(start + 1).min(samples.len());
            for c in covered.iter_mut().take(end).skip(start) {
                *c = true;
            }
        }
        assert!(covered.iter().all(|c| *c), "some samples were never visited");
    }

    #[test]
    fn an_empty_buffer_produces_a_flat_envelope() {
        let mut out = Vec::new();
        min_max_envelope(&[], 10, &mut out);
        assert_eq!(out.len(), 20);
        assert!(out.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn a_single_sample_fills_every_column() {
        let mut out = Vec::new();
        min_max_envelope(&[0.5], 8, &mut out);
        assert_eq!(out.len(), 16);
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn zero_columns_produces_nothing_rather_than_dividing_by_zero() {
        let mut out = Vec::new();
        min_max_envelope(&[0.1, 0.2], 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn more_columns_than_samples_does_not_panic() {
        let mut out = Vec::new();
        min_max_envelope(&[0.1, 0.2, 0.3], 100, &mut out);
        assert_eq!(out.len(), 200);
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn non_finite_samples_never_reach_the_envelope() {
        let samples = [0.1, f32::NAN, 0.5, f32::INFINITY, -0.3];
        let mut out = Vec::new();
        min_max_envelope(&samples, 2, &mut out);
        assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
    }

    #[test]
    fn a_column_of_only_garbage_collapses_to_zero() {
        let samples = [f32::NAN, f32::NAN, 0.5, 0.5];
        let mut out = Vec::new();
        min_max_envelope(&samples, 2, &mut out);
        assert_eq!(&out[0..2], &[0.0, 0.0], "an all-NaN column must not stay at +/-inf");
        assert!(out.iter().all(|v| v.is_finite()));
    }
}
