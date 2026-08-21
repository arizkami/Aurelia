//! Vertical grid-fitting for the small-size bitmap path.
//!
//! Snapping a glyph's quad to the pixel grid puts its *baseline* on a whole
//! pixel. It does nothing for the other horizontal edges in a letterform, and
//! those are most of what the eye reads: the x-height line across the top of
//! `n`, `o` and `x`, the cap line across `H`, the crossbar of `e`. At 13 device
//! pixels an x-height of 6.7 pixels puts that line two thirds of the way into a
//! pixel row, so the top of every lowercase letter is rendered as two rows of
//! grey instead of one row of black.
//!
//! This module builds a map that moves those lines onto whole pixels and
//! stretches the outline between them to follow. It is the vertical half of what
//! TrueType hinting does, and it is deliberately only the vertical half.
//!
//! ## Why not horizontal too
//!
//! Horizontal grid-fitting means moving stems onto pixel boundaries, which
//! changes the width of every glyph and therefore its advance. Doing it properly
//! requires either the font's own hinting bytecode or an autohinter that
//! understands stem detection, and doing it improperly quantises letter spacing
//! into visible clumps. Vertical fitting changes no advance at all, because it
//! only ever moves y.
//!
//! ## Why only below the bitmap threshold
//!
//! A distance field is size-independent by construction: one field serves every
//! size, so there is no single size to fit it to. Fitting is therefore a
//! property of the bitmap path, which rasterises at one exact device size, and
//! it is applied there and nowhere else.

use smallvec::SmallVec;

/// How far a face's round letters reach past each alignment line, in em.
///
/// Always positive, and always measured *outward*: past the x-height and cap
/// lines means further from the baseline, past the baseline means below it.
/// Zero means the face carries no usable round probe glyph — an icon font, a
/// Latin-less subset, a face whose `o` is not an `o` — and zero is what makes
/// suppression a no-op for such a face rather than a refusal.
///
/// Measured across ten faces installed here, relative to the `OS/2` lines these
/// zones use: 0.0088 em (Consolas) to 0.0166 em (Georgia), which is 0.11 to
/// 0.22 device pixels at 13 px and 0.21 to 0.40 at 24.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Overshoot {
    /// Past the x-height line.
    pub x_height: f32,
    /// Past the cap line.
    pub cap_height: f32,
    /// Below the baseline.
    pub baseline: f32,
}

/// The glyphs each line's overshoot is measured from.
///
/// All round, and all reaching past their own line by construction, so a
/// bounding box settles the question — no on-curve test and therefore no
/// outline trace.
///
/// The baseline set carries both cases because they disagree: measured, Arial's
/// `O` bottoms 0.0005 em lower than its `o`, Georgia's 0.0020 and Trebuchet's
/// 0.0024, and a lowercase-only set leaves `O` one row taller than `H` on all
/// three.
const X_ROUND: &[char] = &['o', 'e', 's', 'c'];
const CAP_ROUND: &[char] = &['O', 'C', 'G', 'S'];
const BASE_ROUND: &[char] = &['o', 'e', 's', 'c', 'O', 'C', 'G', 'S', 'U'];

/// Below this an overshoot is not worth a control point.
const MIN_OVERSHOOT_EM: f32 = 0.002;
/// Above this it is not an overshoot but a mis-measurement — a loose authored
/// bounding box, a units-per-em error, an icon font whose `o` is a picture.
///
/// Four times the largest honest value measured here, and deliberately the same
/// band [`tests::measure_overshoot_on_this_machine`] asserts.
const MAX_OVERSHOOT_EM: f32 = 0.04;

/// The horizontal alignment lines of a face, in em units, y-down.
///
/// Baseline is zero and is not stored. Everything above the baseline is
/// negative, matching SphereKit's y-down convention.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VerticalZones {
    /// Top of a lowercase `x`. Negative.
    pub x_height: f32,
    /// Top of a capital `H`. Negative.
    pub cap_height: f32,
    /// Highest ascender. Negative.
    pub ascender: f32,
    /// Lowest descender. Positive.
    pub descender: f32,
    /// How far this face's round letters reach past those lines.
    pub overshoot: Overshoot,
}

impl VerticalZones {
    /// Zones with no overshoot anywhere.
    ///
    /// What a face with no round probe glyphs measures to, and what the
    /// synthetic tests build. A [`GridFit`] over these is the five-knot map
    /// this module carried before suppression existed, control point for
    /// control point.
    pub fn flat(x_height: f32, cap_height: f32, ascender: f32, descender: f32) -> Self {
        Self { x_height, cap_height, ascender, descender, overshoot: Overshoot::default() }
    }

    /// Reads the zones from a face.
    ///
    /// Returns `None` when the face reports neither an x-height nor a cap
    /// height. Those two are what carry the benefit; fitting a glyph to the
    /// ascender and descender alone moves the parts of it nobody is reading.
    ///
    /// `OS/2` version 0 and 1 do not carry `sxHeight` or `sCapHeight` at all,
    /// which is why this is fallible rather than assumed.
    pub fn from_face(face: &ttf_parser::Face<'_>) -> Option<Self> {
        let upem = f32::from(face.units_per_em().max(1));
        let to_em = |v: i16| -(f32::from(v) / upem);

        let x_height = face.x_height().map(to_em);
        let cap_height = face.capital_height().map(to_em);
        if x_height.is_none() && cap_height.is_none() {
            return None;
        }
        // A face with one but not the other is common enough to be worth
        // handling: estimate the missing one from the other's usual ratio
        // rather than dropping the fit entirely.
        let x_height = x_height.unwrap_or_else(|| cap_height.unwrap_or(0.0) * 0.72);
        let cap_height = cap_height.unwrap_or(x_height / 0.72);

        // The reference lines stay the table's. Two variants of this were built
        // and compared over 3300 glyph heights — one taking the reference from
        // `OS/2` as here, one from the median ink of `x z u` and `H E Z T`. Not
        // one height differed, and it could not: the largest disagreement over
        // ten faces is 0.0024 em, a thirtieth of a pixel at 13, far too small to
        // move a snapped row. Only the *width* of the slab needs the ink.
        //
        // These are the default instance's metrics. `with_outline_face` parses a
        // bare face and never sets variation coordinates, so they match the
        // outlines they fit; the day variations reach this path, the cache key
        // has to become the one `GlyphKey` already uses.
        let zones = Self {
            x_height,
            cap_height,
            ascender: to_em(face.ascender()),
            descender: to_em(face.descender()),
            overshoot: Overshoot {
                x_height: furthest_past(face, upem, X_ROUND, x_height, true),
                cap_height: furthest_past(face, upem, CAP_ROUND, cap_height, true),
                baseline: furthest_past(face, upem, BASE_ROUND, 0.0, false),
            },
        };
        zones.is_sane().then_some(zones)
    }

    /// True when the zones are ordered the way a Latin face's are.
    ///
    /// A face whose metrics are nonsense — zero, inverted, absurdly large — must
    /// not be fitted, because a non-monotonic map would fold the outline over
    /// itself and produce a glyph that is not merely unhinted but wrong.
    fn is_sane(&self) -> bool {
        let finite = [self.x_height, self.cap_height, self.ascender, self.descender]
            .into_iter()
            .all(f32::is_finite);
        finite
            && self.x_height < 0.0
            && self.cap_height < 0.0
            && self.ascender <= self.cap_height
            && self.cap_height <= self.x_height
            && self.descender > 0.0
            && self.ascender > -4.0
            && self.descender < 4.0
    }
}

/// The furthest any probe glyph reaches past `reference`, in em, or zero.
///
/// `top` selects the glyph's `y_max`, which is the *smaller* number in y-down
/// coordinates.
///
/// The **furthest**, never the median. FreeType can take a median because its
/// capture radius is far wider than the probe set's spread; a slab exactly as
/// wide as the overshoot has no such slack and must span the whole set or it
/// misses a letter. Measured here: a median met the objective on 30 of 100
/// face/size combinations against 100 of 100 for the furthest. Tahoma's round
/// tops spread 0.0024 em, and a median slab left its `o` outside — nine rows at
/// 13 px against `x`'s seven.
fn furthest_past(
    face: &ttf_parser::Face<'_>,
    upem: f32,
    probes: &[char],
    reference: f32,
    top: bool,
) -> f32 {
    let mut worst = 0.0f32;
    for &c in probes {
        let Some(g) = face.glyph_index(c) else { continue };
        // Some symbol faces map a Latin codepoint onto `.notdef` rather than
        // onto nothing. `.notdef` is a box, and a box is not an overshoot.
        if g.0 == 0 {
            continue;
        }
        let Some(bb) = face.glyph_bounding_box(g) else { continue };
        let edge = -(f32::from(if top { bb.y_max } else { bb.y_min }) / upem);
        // Outward is up for a top line and down for the baseline.
        let over = if top { reference - edge } else { edge - reference };
        // Filtered per probe, not on the maximum. An icon font whose `c` is a
        // picture at 0.2 em must not cost the honest `o`, `e` and `s` their
        // slab; taking the max first and testing that would discard all four.
        if (MIN_OVERSHOOT_EM..=MAX_OVERSHOOT_EM).contains(&over) {
            worst = worst.max(over);
        }
    }
    worst
}

/// The extra control point that flattens one line's overshoot, if it earns one.
///
/// Its target is the line's *fitted* position, copied rather than computed.
/// That is the whole mechanism, and it is FreeType's `shoot.fit = ref.fit`.
/// Rounding the shoot on its own is what goes wrong: two positions a fifth of a
/// pixel apart can round onto different rows, or onto the same row from
/// opposite sides, and neither outcome is a suppression.
fn collapse_knot(
    reference: f32,
    fitted: f32,
    over_em: f32,
    outward: f32,
    neighbour: f32,
    size_px: f32,
) -> Option<(f32, f32)> {
    if outward == 0.0 || over_em <= 0.0 {
        return None;
    }
    let over_px = over_em * size_px;
    // Below a sixty-fourth of a pixel there is nothing to remove. At half a
    // pixel or more the overshoot is a row of ink the designer drew, and
    // hinting stops interfering — which is also what bounds this knot's
    // displacement at one whole device pixel, half from the line's own rounding
    // and half from the overshoot, rather than at some measured number.
    //
    // The upper gate is reachable and must stay. `TextRasterMode::Bitmap`
    // forces this path at any size, and `bitmap_size_px` clamps at `u16::MAX`,
    // not at `BITMAP_MAX_DEVICE_PX`; Georgia's 0.0166 em crosses half a pixel
    // at about 30 device px. It simply never fires under `Auto`, where 24 px is
    // the ceiling and the largest measured overshoot is 0.40 px there.
    if !(1.0 / 64.0..0.5).contains(&over_px) {
        return None;
    }
    // A knot that reached the next line would put two control points out of
    // order in `from` and make `apply` interpolate backwards. A quarter of the
    // gap is far more room than any real face needs: the tightest ratio over
    // ten faces is Courier New's x-height overshoot against its cap-to-x gap,
    // 0.0146 against 0.1484 em — 9.9 per cent, a factor of two and a half
    // inside the guard. Dropping the knot costs one line its suppression, which
    // is cosmetic; an inverted control point folds the outline, which is not.
    if over_em >= (reference - neighbour).abs() * 0.25 {
        return None;
    }
    Some((reference + outward * over_em, fitted))
}

/// A weakly monotonic, piecewise-linear map from unfitted em `y` to fitted em
/// `y`.
///
/// *Weakly*, since overshoot suppression: the segment between a line and its
/// shoot has zero slope on purpose, and that is precisely what puts the apex of
/// an `o` on the row the top of an `x` occupies.
///
/// Built for one exact device size. Applying it at any other size is wrong, not
/// merely suboptimal, which is why it carries no default and is constructed per
/// rasterisation.
#[derive(Clone, Debug, PartialEq)]
pub struct GridFit {
    /// `(from, to)` control points in em, strictly increasing in `from` and
    /// non-decreasing in `to`.
    ///
    /// Eight inline slots because eight is the maximum: five lines plus three
    /// collapse knots. At six it would spill to the heap on every construction
    /// of a real face's fit.
    points: SmallVec<[(f32, f32); 8]>,
}

impl GridFit {
    /// Builds the map for a face at one device size.
    ///
    /// Each zone is moved to the nearest whole pixel and the outline between
    /// zones is stretched linearly to follow. The baseline is a fixed point:
    /// zero pixels rounds to zero, so text on a fitted glyph sits on exactly the
    /// baseline it would have without one.
    ///
    /// Returns `None` when there is nothing useful to do — a size so small that
    /// zones collide, or so large that they are already within a fraction of a
    /// pixel of the grid.
    pub fn new(zones: &VerticalZones, size_px: f32) -> Option<Self> {
        if !size_px.is_finite() || size_px <= 0.0 {
            return None;
        }
        let snap = |y: f32| (y * size_px).round() / size_px;

        // The five reference lines, already ascending because `is_sane` requires
        // `ascender <= cap <= x < 0 < descender`. Zones that are *not* so
        // ordered are refused by the guard below rather than silently sorted
        // into a map, which is what `is_sane` exists to prevent.
        let refs: [(f32, f32); 5] = [
            (zones.ascender, snap(zones.ascender)),
            (zones.cap_height, snap(zones.cap_height)),
            (zones.x_height, snap(zones.x_height)),
            (0.0, 0.0),
            (zones.descender, snap(zones.descender)),
        ];

        // Run on the five lines *before* any collapse knot joins them. That
        // ordering is load-bearing: a collapsed pair is a segment of zero rise
        // by construction and is the entire point of the exercise, while two
        // lines that rounded onto the same row mean a size too small to fit at
        // all. This guard rejects zero rise, so on the finished eight-point list
        // it could not tell the two apart and would refuse every face on earth —
        // switching grid-fitting off silently rather than failing loudly.
        for pair in refs.windows(2) {
            if pair[1].0 - pair[0].0 <= f32::EPSILON || pair[1].1 - pair[0].1 <= f32::EPSILON {
                return None;
            }
        }

        let mut points: SmallVec<[(f32, f32); 8]> = SmallVec::new();
        let mut collapsed = false;
        for (i, &(from, to)) in refs.iter().enumerate() {
            // How far this line's round letters reach past it, which way, and
            // which line they head toward. y is down, so a top line is overshot
            // upward (negative) and the baseline downward (positive).
            let (over, outward, neighbour) = match i {
                1 => (zones.overshoot.cap_height, -1.0f32, refs[0].0),
                2 => (zones.overshoot.x_height, -1.0, refs[1].0),
                3 => (zones.overshoot.baseline, 1.0, refs[4].0),
                _ => (0.0, 0.0, from),
            };
            match collapse_knot(from, to, over, outward, neighbour, size_px) {
                // A top line's shoot is above it, so it comes first in `from`.
                Some(knot) if outward < 0.0 => {
                    collapsed = true;
                    points.extend([knot, (from, to)]);
                }
                Some(knot) => {
                    collapsed = true;
                    points.extend([(from, to), knot]);
                }
                None => points.push((from, to)),
            }
        }

        // Nothing to do only when every line already sits on the grid *and* no
        // slab was earned. Testing the lines alone would switch suppression off
        // for a face whose zones happen to land on whole pixels, and its `o`
        // would stay a row taller than its `x` for that reason alone.
        let moved = refs.iter().any(|&(from, to)| (to - from).abs() * size_px > 1.0e-3);
        if !moved && !collapsed {
            return None;
        }
        Some(Self { points })
    }

    /// Maps one em `y` through the fit.
    ///
    /// Interpolates between the two zones it falls between, and extrapolates
    /// with the slope of the nearest segment outside them — so a very tall
    /// accent or a very deep tail is displaced consistently with the part of
    /// the glyph it is attached to, rather than being clamped onto a zone.
    pub fn apply(&self, y: f32) -> f32 {
        let n = self.points.len();
        if n == 0 || !y.is_finite() {
            return y;
        }
        if y <= self.points[0].0 {
            return extrapolate(self.points[0], self.points[1.min(n - 1)], y);
        }
        if y >= self.points[n - 1].0 {
            return extrapolate(self.points[n.saturating_sub(2)], self.points[n - 1], y);
        }
        for pair in self.points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if y >= a.0 && y <= b.0 {
                let t = (y - a.0) / (b.0 - a.0);
                return a.1 + (b.1 - a.1) * t;
            }
        }
        y
    }

    /// How many control points the map carries.
    ///
    /// Five for a face with no overshoot, eight for an ordinary Latin one.
    #[inline]
    pub fn zone_count(&self) -> usize {
        self.points.len()
    }

    /// The steepest segment of the map.
    ///
    /// A y-only map is safe for a shallow diagonal exactly insofar as this is
    /// bounded, and it is the one quantity that separates this design from a
    /// point-wise capture band. A snap — `if |y - line| < band { fitted } else
    /// { map(y) }` — is a non-decreasing step function of y, so it passes a
    /// dense monotonicity sweep with a green light while tearing two adjacent
    /// flattened vertices a quarter of a pixel apart at the band edge. And
    /// `Rasterizer::coverage` takes `acc.abs()`, so a folded contour does not
    /// fail loudly; it quietly produces a wrong row.
    ///
    /// Measured worst over ten faces at ten sizes: 1.608, against 1.472 for the
    /// five-knot map on the same face and size.
    pub fn max_slope(&self) -> f32 {
        self.points.windows(2).map(|w| (w[1].1 - w[0].1) / (w[1].0 - w[0].0)).fold(0.0, f32::max)
    }
}

/// Linear extrapolation through two control points.
fn extrapolate(a: (f32, f32), b: (f32, f32), y: f32) -> f32 {
    let span = b.0 - a.0;
    if span.abs() <= f32::EPSILON {
        return y + (a.1 - a.0);
    }
    let slope = (b.1 - a.1) / span;
    a.1 + (y - a.0) * slope
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roughly Segoe UI: x-height 0.50 em, cap 0.70 em.
    fn zones() -> VerticalZones {
        VerticalZones::flat(-0.50, -0.70, -1.0, 0.25)
    }

    /// The same face with a typical Latin overshoot on every line.
    ///
    /// 0.012 em is the middle of the range measured across ten installed faces,
    /// which runs 0.0088 to 0.0166.
    fn zones_with_overshoot() -> VerticalZones {
        VerticalZones {
            overshoot: Overshoot { x_height: 0.012, cap_height: 0.012, baseline: 0.012 },
            ..zones()
        }
    }

    #[test]
    fn the_baseline_is_a_fixed_point() {
        // Text sits on the baseline the layout engine computed. A fit that
        // moved it would shift every line of a paragraph relative to the boxes
        // around it.
        let fit = GridFit::new(&zones(), 13.0).expect("13 px needs fitting");
        assert_eq!(fit.apply(0.0), 0.0);
    }

    #[test]
    fn the_x_height_lands_on_a_whole_pixel() {
        // The whole point: at 13 px an x-height of 0.5 em is 6.5 pixels, so the
        // top of every lowercase letter straddles two rows.
        let fit = GridFit::new(&zones(), 13.0).expect("13 px needs fitting");
        let fitted = fit.apply(-0.50) * 13.0;
        assert!((fitted - fitted.round()).abs() < 1.0e-4, "x-height landed at {fitted}");
        assert!((fitted - -7.0).abs() < 1.0e-4, "expected -7, got {fitted}");
    }

    #[test]
    fn every_zone_lands_on_a_whole_pixel() {
        let z = zones();
        let fit = GridFit::new(&z, 13.0).expect("13 px needs fitting");
        for zone in [z.ascender, z.cap_height, z.x_height, 0.0, z.descender] {
            let fitted = fit.apply(zone) * 13.0;
            assert!((fitted - fitted.round()).abs() < 1.0e-4, "zone {zone} landed at {fitted}");
        }
    }

    #[test]
    fn the_map_is_monotonic_so_the_outline_cannot_fold_over_itself() {
        // A non-monotonic map would put a lower point above a higher one and
        // turn the glyph inside out, which is worse than no fitting at all.
        let fit = GridFit::new(&zones(), 13.0).expect("13 px needs fitting");
        let mut previous = f32::NEG_INFINITY;
        for i in -200..=200 {
            let y = i as f32 / 100.0;
            let mapped = fit.apply(y);
            assert!(mapped >= previous - 1.0e-6, "went backwards at y = {y}");
            previous = mapped;
        }
    }

    #[test]
    fn nothing_moves_further_than_one_device_pixel() {
        // Fitting is a nudge, not a redesign. Half a pixel comes from the
        // line's own rounding and half from an overshoot slab, and
        // `collapse_knot` refuses any overshoot of half a pixel or more — so
        // one whole pixel is a proven bound, not a measured one.
        //
        // The sweep runs past the descender at 0.25 on purpose: `apply`
        // extrapolates outside the zones with the end segment's slope, so the
        // worst displacement is not necessarily attained at a control point,
        // and that extrapolation slope is exactly what a new first knot
        // changes.
        for z in [zones(), zones_with_overshoot()] {
            let Some(fit) = GridFit::new(&z, 13.0) else { continue };
            for i in -140..=40 {
                let y = i as f32 / 100.0;
                let moved = (fit.apply(y) - y).abs() * 13.0;
                assert!(moved <= 1.0 + 1.0e-3, "y = {y} moved {moved} px with {z:?}");
            }
        }
    }

    #[test]
    fn a_size_that_already_lands_on_the_grid_and_has_no_overshoot_needs_no_fit() {
        // 0.5 em at 16 px is exactly 8 pixels, and 0.25 em is exactly 4.
        let z = VerticalZones::flat(-0.5, -0.75, -1.0, 0.25);
        assert_eq!(GridFit::new(&z, 16.0), None, "a fit with nothing to move is wasted work");
    }

    #[test]
    fn a_face_already_on_the_grid_still_gets_a_fit_if_it_has_an_overshoot() {
        // The trap in the early return: testing only whether a line moved would
        // switch suppression off for a face whose zones happen to land on whole
        // pixels, and its `o` would stay a row taller than its `x` for that
        // reason alone.
        let z = VerticalZones {
            overshoot: Overshoot { x_height: 0.012, cap_height: 0.012, baseline: 0.012 },
            ..VerticalZones::flat(-0.5, -0.75, -1.0, 0.25)
        };
        let fit = GridFit::new(&z, 16.0).expect("an earned slab is reason enough to build a map");
        assert!(fit.zone_count() > 5);
    }

    #[test]
    fn a_size_too_small_to_separate_the_zones_is_refused() {
        // At 3 px the x-height and cap height round to the same row. Fitting
        // there would collapse a segment to zero width and divide by it.
        assert_eq!(GridFit::new(&zones(), 3.0), None);
        assert_eq!(GridFit::new(&zones(), 0.0), None);
        assert_eq!(GridFit::new(&zones(), f32::NAN), None);
    }

    #[test]
    fn points_beyond_the_zones_are_extrapolated_rather_than_clamped() {
        // A tall accent or a deep tail must move with the part of the glyph it
        // is attached to. Clamping would flatten it onto the zone.
        let fit = GridFit::new(&zones(), 13.0).expect("13 px needs fitting");
        let high = fit.apply(-1.4);
        let low = fit.apply(0.6);
        assert!(high < fit.apply(-1.0), "above the ascender must stay above it");
        assert!(low > fit.apply(0.25), "below the descender must stay below it");
        assert!(high.is_finite() && low.is_finite());
    }

    #[test]
    fn implausible_metrics_are_refused_rather_than_fitted() {
        // A non-monotonic or zeroed set of zones would build a map that folds
        // the outline over itself.
        let bad = [
            VerticalZones::flat(0.0, 0.0, 0.0, 0.0),
            // x-height above the cap height.
            VerticalZones::flat(-0.9, -0.5, -1.0, 0.2),
            // Descender on the wrong side of the baseline.
            VerticalZones::flat(-0.5, -0.7, -1.0, -0.2),
            // Absurd scale, which a corrupt `units_per_em` produces.
            VerticalZones::flat(-50.0, -70.0, -100.0, 25.0),
        ];
        for z in bad {
            assert!(!z.is_sane(), "{z:?} should have been refused");
        }
        assert!(zones().is_sane());
    }

    // ------------------------------------------------ overshoot suppression

    #[test]
    fn a_round_letters_apex_is_given_the_flat_letters_row() {
        // The whole feature. The shoot is not rounded on its own — it is handed
        // the position the line was rounded to, so the two cannot land on
        // different rows however the arithmetic falls.
        let z = zones_with_overshoot();
        let fit = GridFit::new(&z, 13.0).expect("13 px needs fitting");
        let flat_top = fit.apply(z.x_height);
        let round_top = fit.apply(z.x_height - z.overshoot.x_height);
        assert_eq!(round_top, flat_top, "the apex of `o` did not land on `x`'s row");
        // Within float noise, not exactly: `(y * size).round() / size` does not
        // round-trip back through `* size` onto the integer it came from. This
        // face lands at -7.0000005 rather than -7.0, which is exactly the
        // residue `raster::snap_out` exists to absorb before it reaches
        // `floor` and costs the glyph a blank row.
        let rows = flat_top * 13.0;
        assert!((rows - rows.round()).abs() < 1.0e-4, "the row was {rows}, not a whole pixel");
    }

    #[test]
    fn a_round_letters_foot_is_given_the_baseline() {
        let z = zones_with_overshoot();
        let fit = GridFit::new(&z, 13.0).expect("13 px needs fitting");
        assert_eq!(fit.apply(z.overshoot.baseline), 0.0, "the foot of `o` did not reach 0");
    }

    #[test]
    fn suppression_adds_one_control_point_per_line_that_earns_it() {
        assert_eq!(GridFit::new(&zones(), 13.0).unwrap().zone_count(), 5);
        assert_eq!(GridFit::new(&zones_with_overshoot(), 13.0).unwrap().zone_count(), 8);
    }

    #[test]
    fn the_map_stays_weakly_monotonic_with_slabs_in_it() {
        // A slab has zero slope by construction. What it must never have is
        // negative slope, which would fold the outline over itself.
        let fit = GridFit::new(&zones_with_overshoot(), 13.0).expect("13 px needs fitting");
        let mut previous = f32::NEG_INFINITY;
        for i in -140..=40 {
            let y = i as f32 / 100.0;
            let mapped = fit.apply(y);
            assert!(mapped >= previous - 1.0e-6, "went backwards at y = {y}");
            previous = mapped;
        }
    }

    #[test]
    fn the_slope_stays_bounded_so_a_shallow_diagonal_is_not_torn() {
        // The invariant that separates this from a point-wise capture band. A
        // snap is a non-decreasing step function of y, so it passes the
        // monotonicity sweep above while tearing two adjacent flattened
        // vertices apart at the band edge — and `Rasterizer::coverage` takes
        // `acc.abs()`, so a folded contour does not fail loudly.
        for size in [11.0f32, 13.0, 16.0, 20.0, 24.0] {
            let Some(fit) = GridFit::new(&zones_with_overshoot(), size) else { continue };
            let slope = fit.max_slope();
            assert!(slope < 2.0, "slope {slope} at {size} px would visibly stretch a diagonal");
            assert!(slope > 0.0);
        }
    }

    #[test]
    fn an_overshoot_of_half_a_pixel_or_more_is_left_alone() {
        // At that point it is a row of ink the designer drew, and hinting stops
        // interfering. This is also what bounds the displacement at one pixel.
        let z = VerticalZones {
            overshoot: Overshoot { x_height: 0.06, cap_height: 0.06, baseline: 0.06 },
            ..zones()
        };
        let fit = GridFit::new(&z, 13.0).expect("13 px needs fitting");
        assert_eq!(fit.zone_count(), 5, "a half-pixel overshoot must not be collapsed");
    }

    #[test]
    fn an_overshoot_that_would_reach_the_next_line_is_refused() {
        // An inverted control point makes `apply` interpolate backwards and
        // folds the outline. Losing one line's suppression is cosmetic; that is
        // not.
        let z = VerticalZones {
            // 0.1 em against a cap-to-x gap of 0.2 em: half the gap.
            overshoot: Overshoot { x_height: 0.1, cap_height: 0.0, baseline: 0.0 },
            ..zones()
        };
        let fit = GridFit::new(&z, 13.0).expect("13 px needs fitting");
        assert_eq!(fit.zone_count(), 5);
    }

    #[test]
    fn a_face_with_no_round_letters_is_unaffected() {
        // An icon font, a Latin-less subset. Zero overshoot is a no-op, not a
        // refusal: the face still gets the five-knot fit it had before.
        let fit = GridFit::new(&zones(), 13.0).expect("13 px needs fitting");
        assert_eq!(fit.zone_count(), 5);
        assert_eq!(fit.apply(0.0), 0.0);
    }

    #[test]
    fn the_baseline_is_still_a_fixed_point_with_a_slab_below_it() {
        // The baseline slab hangs *below* zero, so the knot before the baseline
        // is still the x-height and the interpolation still lands exactly.
        let fit = GridFit::new(&zones_with_overshoot(), 13.0).expect("13 px needs fitting");
        assert_eq!(fit.apply(0.0), 0.0);
    }

    #[test]
    fn the_map_carries_one_point_per_zone_including_the_baseline() {
        let fit = GridFit::new(&zones(), 13.0).expect("13 px needs fitting");
        assert_eq!(fit.zone_count(), 5, "ascender, cap, x-height, baseline, descender");
    }

    #[test]
    fn a_non_finite_input_passes_through_untouched() {
        let fit = GridFit::new(&zones(), 13.0).expect("13 px needs fitting");
        assert!(fit.apply(f32::NAN).is_nan());
    }
}

#[cfg(test)]
mod face_tests {
    use super::*;
    use crate::font::FontDatabase;
    use crate::raster::{rasterize_glyph, rasterize_shape};
    use crate::types::FontRequest;
    use spherekit_core::GlyphId;

    /// The top row of a glyph's coverage, as a fraction of full black.
    ///
    /// A fitted x-height puts the top of `x` on a pixel boundary, so its first
    /// row is nearly solid. An unfitted one straddles two rows and the first is
    /// partial — which is exactly what makes small text look grey.
    fn top_row_coverage(image: &crate::types::GlyphImage) -> f32 {
        if image.width == 0 || image.height == 0 {
            return 0.0;
        }
        let row = &image.data[..image.width as usize];
        let lit: Vec<f32> = row.iter().filter(|&&v| v > 0).map(|&v| f32::from(v) / 255.0).collect();
        if lit.is_empty() {
            return 0.0;
        }
        lit.iter().sum::<f32>() / lit.len() as f32
    }

    #[test]
    fn a_real_face_reports_usable_zones() {
        let mut db = FontDatabase::with_system_fonts();
        let Some(font) = db.resolve(&FontRequest::default()) else {
            eprintln!("no system font; skipping");
            return;
        };
        let Some(zones) = db.with_face(font, |face| VerticalZones::from_face(face)).flatten()
        else {
            eprintln!("face reports no x-height or cap height; skipping");
            return;
        };
        assert!(zones.x_height < 0.0 && zones.cap_height < zones.x_height);
        assert!(zones.ascender <= zones.cap_height);
        assert!(zones.descender > 0.0);
    }

    #[test]
    fn fitting_darkens_the_top_row_of_a_flat_topped_letter() {
        // The measurable form of "sharper": the x-height line lands on a pixel
        // boundary, so the row it lands in is solid rather than half-covered.
        let mut db = FontDatabase::with_system_fonts();
        let Some(font) = db.resolve(&FontRequest::default()) else {
            eprintln!("no system font; skipping");
            return;
        };
        let Some(glyph) = db.glyph_index(font, 'x') else { return };

        let mut best_gain = 0.0f32;
        let mut fitted_any = false;
        for size in [11.0f32, 12.0, 13.0, 14.0, 15.0, 16.0] {
            let Some((fitted, plain)) = db
                .with_face(font, |face| {
                    let shape = crate::mtsdf::extract_shape(face, glyph).ok()?;
                    let zones = VerticalZones::from_face(face)?;
                    GridFit::new(&zones, size)?;
                    let fitted = rasterize_glyph(face, glyph, size).ok()?;
                    let plain = rasterize_shape(&shape, size);
                    Some((fitted, plain))
                })
                .flatten()
            else {
                continue;
            };
            fitted_any = true;
            best_gain = best_gain.max(top_row_coverage(&fitted) - top_row_coverage(&plain));
        }
        if !fitted_any {
            eprintln!("no size needed fitting; skipping");
            return;
        }
        assert!(
            best_gain > 0.05,
            "fitting never darkened the x-height row; best gain was {best_gain}"
        );
    }

    #[test]
    fn a_round_letter_rasterises_to_the_same_height_as_a_flat_one() {
        // The objective, measured in rows of actual coverage rather than in em.
        // Without suppression `o` is one row taller than `x` because its apex
        // overshoots the x-height line by about a fifth of a pixel and lands in
        // the row above.
        let mut db = FontDatabase::with_system_fonts();
        let Some(font) = db.resolve(&FontRequest::default()) else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut checked = 0;
        for size in [11.0f32, 13.0, 16.0, 20.0] {
            let heights = db.with_face(font, |face| {
                let zones = VerticalZones::from_face(face)?;
                GridFit::new(&zones, size)?;
                let mut out = Vec::new();
                for c in ['x', 'z', 'o', 'e', 's', 'c'] {
                    let g = face.glyph_index(c)?;
                    out.push((c, rasterize_glyph(face, GlyphId(g.0), size).ok()?.height));
                }
                Some(out)
            });
            let Some(Some(heights)) = heights else { continue };
            let flat = heights
                .iter()
                .filter(|(c, _)| matches!(c, 'x' | 'z'))
                .map(|&(_, h)| h)
                .max()
                .unwrap_or(0);
            for &(c, h) in heights.iter().filter(|(c, _)| !matches!(c, 'x' | 'z')) {
                assert_eq!(h, flat, "`{c}` is {h} rows at {size} px against a flat {flat}");
            }
            checked += 1;
        }
        if checked == 0 {
            eprintln!("no size needed fitting; skipping");
        }
    }

    #[test]
    fn fitting_never_moves_the_baseline_of_a_real_glyph() {
        // The invariant the whole layer depends on: a fitted glyph sits on the
        // same baseline as an unfitted one, or every line shifts against the
        // boxes around it.
        let mut db = FontDatabase::with_system_fonts();
        let Some(font) = db.resolve(&FontRequest::default()) else {
            eprintln!("no system font; skipping");
            return;
        };
        let Some(glyph) = db.glyph_index(font, 'x') else { return };
        let Some(bottoms) = db
            .with_face(font, |face| {
                let zones = VerticalZones::from_face(face)?;
                let fit = GridFit::new(&zones, 13.0)?;
                let shape = crate::mtsdf::extract_shape(face, glyph).ok()?;
                let ink = shape.bounds()?;
                // `x` sits on the baseline, so its lowest point is at or very near
                // zero and the fit must leave it there.
                Some((ink.max_y(), fit.apply(ink.max_y())))
            })
            .flatten()
        else {
            eprintln!("no fit at 13 px; skipping");
            return;
        };
        let moved = (bottoms.1 - bottoms.0).abs() * 13.0;
        assert!(moved < 0.6, "the baseline edge moved {moved} px");
    }
}

#[cfg(test)]
mod overshoot_measurement {
    use crate::font::FontDatabase;
    use crate::mtsdf::extract_shape;
    use crate::types::FontRequest;

    /// Measures how far the round letters actually overshoot, per face.
    ///
    /// Prints the numbers so a threshold can be chosen against measured values
    /// rather than against one someone remembered, and asserts the property
    /// that makes suppression worth doing at all: the overshoot is real, and it
    /// is small. Measured here at 0.012 to 0.017 em across five faces, which is
    /// 0.15 to 0.22 device pixels at 13 px.
    ///
    /// The bound is deliberately loose. It is not a claim about any particular
    /// font; it catches the two ways the extraction could be wrong — a sign flip
    /// in the y-down conversion, which would report a negative overshoot, and a
    /// units-per-em error, which would report a nonsensically large one.
    #[test]
    fn measure_overshoot_on_this_machine() {
        let mut db = FontDatabase::with_system_fonts();
        let mut checked = 0;
        for family in ["Segoe UI", "Arial", "Tahoma", "Verdana", "Georgia"] {
            let Some(font) = db.resolve(&FontRequest::family(family)) else { continue };
            let Some(name) = db.family_name(font).map(str::to_string) else { continue };
            if name != family {
                continue;
            }
            let probe = |db: &mut FontDatabase, c: char| -> Option<(f32, f32)> {
                let glyph = db.glyph_index(font, c)?;
                db.with_face(font, |face| {
                    let shape = extract_shape(face, glyph).ok()?;
                    let b = shape.bounds()?;
                    Some((b.min_y(), b.max_y()))
                })
                .flatten()
            };
            let (Some(x), Some(o), Some(h), Some(cap_o)) = (
                probe(&mut db, 'x'),
                probe(&mut db, 'o'),
                probe(&mut db, 'H'),
                probe(&mut db, 'O'),
            ) else {
                continue;
            };
            // y is down, so a smaller min_y is higher on the page.
            let x_top = (x.0 - o.0).abs();
            let base = (o.1 - x.1).abs();
            let cap_top = (h.0 - cap_o.0).abs();
            println!(
                "{name:<10} x-height overshoot {:.4} em ({:.2} px at 13), baseline {:.4} em ({:.2} px), cap {:.4} em ({:.2} px)",
                x_top,
                x_top * 13.0,
                base,
                base * 13.0,
                cap_top,
                cap_top * 13.0
            );
            for (what, v) in [("x-height", x_top), ("baseline", base), ("cap", cap_top)] {
                assert!(
                    (0.002..0.04).contains(&v),
                    "{name} {what} overshoot of {v} em is not a plausible Latin overshoot"
                );
            }
            checked += 1;
        }
        if checked == 0 {
            eprintln!("no probed face was installed; skipping");
        }
    }
}

#[cfg(test)]
mod probe_cost {
    use super::*;
    use crate::font::FontDatabase;
    use crate::types::FontRequest;

    /// Times reading the zones, which is what decides whether it needs a cache.
    ///
    /// Prints rather than asserts a bound: a timing assertion on shared CI
    /// hardware is a flaky test, and the number here exists to justify a design
    /// decision that is written down in the module docs.
    #[test]
    fn how_long_does_reading_the_zones_take() {
        let mut db = FontDatabase::with_system_fonts();
        for family in ["Segoe UI", "Georgia", "Consolas"] {
            let Some(font) = db.resolve(&FontRequest::family(family)) else { continue };
            // The parse is hoisted because the real caller has one: `rasterize_glyph`
            // runs inside `with_outline_face`, so reading the zones there costs the
            // glyph lookups and nothing else. Timing the parse too would measure a
            // cost this code does not add.
            let n = 5000;
            let measured = db.with_face(font, |face| {
                let start = std::time::Instant::now();
                let mut sink = 0.0f32;
                for _ in 0..n {
                    if let Some(z) = VerticalZones::from_face(face) {
                        sink += z.overshoot.x_height;
                    }
                }
                (start.elapsed().as_secs_f64() * 1.0e6 / f64::from(n), sink)
            });
            let Some((per, sink)) = measured else { continue };
            println!("{family:<10} {per:.3} us per read (sink {sink})");
        }
    }
}
