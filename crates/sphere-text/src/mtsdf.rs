//! Multi-channel plus true signed distance field ("MTSDF") glyph generation.
//!
//! This is a from-scratch Rust implementation of Viktor Chlumský's multi-channel
//! signed distance field algorithm. The reference implementation (`msdfgen`) is
//! C++ and Sphere is pure Rust, so the algorithm is reproduced here rather than
//! bound to.
//!
//! ## What the four channels mean
//!
//! * `R`, `G`, `B` each hold a *signed pseudo-distance* to the nearest outline
//!   edge that was assigned that channel. Reconstruction takes the
//!   [`median`] of the three, which is what makes a sharp corner survive: two
//!   edges meeting at a corner share exactly one channel, so the shared channel
//!   carries the corner while the two private channels carry the two half-planes,
//!   and the median of the three is the distance to their intersection instead of
//!   the rounded distance to the corner point.
//! * `A` holds the plain *true* signed distance. It is what outlines, glows and
//!   drop shadows sample, and it is also the ground truth the error-correction
//!   pass measures the median against.
//!
//! ## Pseudo-distance
//!
//! For the channel distances the winning edge's distance is converted to a
//! *pseudo*-distance: past an edge's endpoint the edge's tangent is extended to
//! infinity and the perpendicular distance to that line is used instead. Without
//! it every corner would be clipped back to the rounded true distance and the
//! whole point of the multi-channel encoding would be lost.
//!
//! ## Coordinate conventions
//!
//! TrueType and CFF outlines are y-**up** and measured in font units. Sphere is
//! y-**down** and measures glyph geometry in em units so one field can serve
//! every size. [`extract_shape`] performs both conversions in one step: `x_em =
//! x / units_per_em`, `y_em = -y / units_per_em`. Flipping y reverses contour
//! orientation, which is why orientation is re-derived from the signed area
//! rather than assumed (see [`Shape::orient`]).
//!
//! ## Resolution and memory
//!
//! [`GlyphRasterConfig::em_size_px`] chooses how many texels one em occupies and
//! [`GlyphRasterConfig::range_px`] how many of those texels the field spans
//! either side of the outline. 32 px/em is enough for UI text and costs about
//! 6 KiB per glyph; 64 px/em roughly quadruples that for detail that only shows
//! up in display sizes. The range is the real quality knob: it has to be wide
//! enough to smooth the glyph at the largest size it will be drawn *and* to hold
//! whatever outline width the shader synthesises, but every extra texel of range
//! is padding added around every glyph in the atlas. 4 px is the usual
//! compromise and is the default here.

use crate::types::{FontMetrics, GlyphFormat, GlyphImage, GlyphMetrics};
use sphere_core::{Edges, FontError, GlyphId, Point, Rect, Size};

// ---------------------------------------------------------------------------
// Small vector helpers.
//
// `Point<f32>` doubles as a vector inside this module: the algorithm constantly
// mixes positions and offsets and forcing every step through `Point`/`Size`
// conversions would obscure it. These helpers are the only place that liberty is
// taken.
// ---------------------------------------------------------------------------

#[inline]
fn v(x: f32, y: f32) -> Point<f32> {
    Point::new(x, y)
}

#[inline]
fn vsub(a: Point<f32>, b: Point<f32>) -> Point<f32> {
    v(a.x - b.x, a.y - b.y)
}

#[inline]
fn vadd(a: Point<f32>, b: Point<f32>) -> Point<f32> {
    v(a.x + b.x, a.y + b.y)
}

#[inline]
fn vscale(a: Point<f32>, s: f32) -> Point<f32> {
    v(a.x * s, a.y * s)
}

#[inline]
fn dot(a: Point<f32>, b: Point<f32>) -> f32 {
    a.x * b.x + a.y * b.y
}

/// The z component of the 3D cross product of two 2D vectors.
#[inline]
fn cross(a: Point<f32>, b: Point<f32>) -> f32 {
    a.x * b.y - a.y * b.x
}

#[inline]
fn length(a: Point<f32>) -> f32 {
    dot(a, a).sqrt()
}

/// Unit vector, or the zero vector when the input has no direction.
#[inline]
fn normalize(a: Point<f32>) -> Point<f32> {
    let len = length(a);
    if len > 0.0 { vscale(a, 1.0 / len) } else { v(0.0, 0.0) }
}

/// The unit vector 90 degrees clockwise from `a` in y-down space.
///
/// Matches `msdfgen`'s `getOrthonormal(false)`, which fixes the sign convention
/// for every distance in this module: a point on this side of an edge is at a
/// negative distance.
#[inline]
fn orthonormal(a: Point<f32>) -> Point<f32> {
    let len = length(a);
    if len > 0.0 { v(a.y / len, -a.x / len) } else { v(0.0, 0.0) }
}

#[inline]
fn lerp(a: Point<f32>, b: Point<f32>, t: f32) -> Point<f32> {
    v(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}

/// `+1` for a positive value, `-1` for zero or negative.
///
/// Zero must not map to zero: it is used to give an exactly-on-the-outline point
/// a definite side, and a zero sign would erase the distance's magnitude.
#[inline]
fn non_zero_sign(x: f32) -> f32 {
    if x > 0.0 { 1.0 } else { -1.0 }
}

// ---------------------------------------------------------------------------
// Polynomial roots
// ---------------------------------------------------------------------------

/// Up to three real roots of a polynomial.
///
/// A fixed-size buffer rather than a `Vec`: root finding sits in the innermost
/// loop of field generation, one call per curve per texel, and must not allocate.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Roots {
    values: [f32; 3],
    count: u8,
}

impl Roots {
    /// No real roots.
    pub const NONE: Self = Self { values: [0.0; 3], count: 0 };

    /// The roots found, in solver order (not sorted).
    #[inline]
    pub fn as_slice(&self) -> &[f32] {
        &self.values[..self.count as usize]
    }

    /// How many real roots were found.
    #[inline]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// True when the polynomial had no real roots the solver could report.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    fn push(&mut self, x: f64) {
        if self.count < 3 {
            self.values[self.count as usize] = x as f32;
            self.count += 1;
        }
    }
}

/// Real roots of `a*x^2 + b*x + c`.
///
/// A degenerate polynomial (`a` and `b` both vanishing) reports no roots even
/// when every `x` is a solution: callers use roots to enumerate distance
/// candidates, and "every point is a candidate" is not actionable.
pub fn solve_quadratic(a: f32, b: f32, c: f32) -> Roots {
    solve_quadratic_f64(a as f64, b as f64, c as f64)
}

fn solve_quadratic_f64(a: f64, b: f64, c: f64) -> Roots {
    let mut out = Roots::NONE;
    // The threshold is absolute rather than relative because the caller feeds in
    // em-scale coefficients, where 1e-14 is far below any meaningful curvature.
    if a.abs() < 1e-14 {
        if b.abs() < 1e-14 {
            return out;
        }
        out.push(-c / b);
        return out;
    }
    let discriminant = b * b - 4.0 * a * c;
    if discriminant > 0.0 {
        let d = discriminant.sqrt();
        out.push((-b + d) / (2.0 * a));
        out.push((-b - d) / (2.0 * a));
    } else if discriminant == 0.0 {
        out.push(-b / (2.0 * a));
    }
    out
}

/// Real roots of `a*x^3 + b*x^2 + c*x + d`.
///
/// Uses the trigonometric solution in the three-real-roots case and Cardano's
/// formula otherwise, in `f64` regardless of the `f32` interface: the
/// intermediate `r^2 - q^3` loses most of its significant digits near a double
/// root, and doing that arithmetic in `f32` produces visibly wrong closest points
/// on nearly-degenerate curves.
pub fn solve_cubic(a: f32, b: f32, c: f32, d: f32) -> Roots {
    let (a, b, c, d) = (a as f64, b as f64, c as f64, d as f64);
    if a.abs() < 1e-14 {
        return solve_quadratic_f64(b, c, d);
    }
    solve_cubic_normed(b / a, c / a, d / a)
}

fn solve_cubic_normed(a: f64, b: f64, c: f64) -> Roots {
    let mut out = Roots::NONE;
    let a2 = a * a;
    let mut q = (a2 - 3.0 * b) / 9.0;
    let r = (a * (2.0 * a2 - 9.0 * b) + 27.0 * c) / 54.0;
    let r2 = r * r;
    let q3 = q * q * q;
    let a = a / 3.0;
    if r2 < q3 {
        // Three distinct real roots: the trigonometric form avoids the complex
        // arithmetic Cardano's formula would otherwise need here.
        let t = (r / q3.sqrt()).clamp(-1.0, 1.0).acos();
        q = -2.0 * q.sqrt();
        out.push(q * (t / 3.0).cos() - a);
        out.push(q * ((t + 2.0 * core::f64::consts::PI) / 3.0).cos() - a);
        out.push(q * ((t - 2.0 * core::f64::consts::PI) / 3.0).cos() - a);
    } else {
        let u = if r < 0.0 { 1.0 } else { -1.0 } * (r.abs() + (r2 - q3).sqrt()).cbrt();
        let w = if u == 0.0 { 0.0 } else { q / u };
        out.push((u + w) - a);
        // u == w is the double-root case; the tolerance catches the far more
        // common "almost a double root", where reporting one root would leave a
        // real closest point undiscovered.
        if u == w || (u - w).abs() < 1e-12 * (u + w).abs() {
            out.push(-0.5 * (u + w) - a);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Edge colors
// ---------------------------------------------------------------------------

/// Which of the three distance channels an edge contributes to.
///
/// Only the two-channel values ([`EdgeColor::YELLOW`], [`EdgeColor::MAGENTA`],
/// [`EdgeColor::CYAN`]) and [`EdgeColor::WHITE`] are ever assigned: the coloring
/// invariant is that two edges meeting at a corner share *exactly one* channel,
/// which two-channel masks make possible and one-channel masks do not.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct EdgeColor(u8);

impl EdgeColor {
    /// No channels. Only ever used as "no ban" when picking the next color.
    pub const BLACK: Self = Self(0);
    /// Red only.
    pub const RED: Self = Self(1);
    /// Green only.
    pub const GREEN: Self = Self(2);
    /// Red and green.
    pub const YELLOW: Self = Self(3);
    /// Blue only.
    pub const BLUE: Self = Self(4);
    /// Red and blue.
    pub const MAGENTA: Self = Self(5);
    /// Green and blue.
    pub const CYAN: Self = Self(6);
    /// All three channels, used for contours with no corners at all.
    pub const WHITE: Self = Self(7);

    /// The raw channel mask, bit 0 red, bit 1 green, bit 2 blue.
    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// True when this edge contributes to the red channel.
    #[inline]
    pub const fn has_red(self) -> bool {
        self.0 & 1 != 0
    }

    /// True when this edge contributes to the green channel.
    #[inline]
    pub const fn has_green(self) -> bool {
        self.0 & 2 != 0
    }

    /// True when this edge contributes to the blue channel.
    #[inline]
    pub const fn has_blue(self) -> bool {
        self.0 & 4 != 0
    }

    /// How many channels the two colors have in common.
    ///
    /// The coloring pass guarantees this is exactly one across every corner,
    /// which is the property the median reconstruction depends on.
    #[inline]
    pub const fn shared_channels(self, other: Self) -> u32 {
        (self.0 & other.0).count_ones()
    }
}

/// Picks the next edge color, avoiding `banned`.
///
/// A direct port of `msdfgen`'s `switchColor`. The rotation is deterministic
/// given `seed`, which matters because glyph fields are cached: the same glyph
/// must produce the same field on every run or the atlas would churn.
fn switch_color(color: &mut EdgeColor, seed: &mut u64, banned: EdgeColor) {
    let combined = color.0 & banned.0;
    if combined == EdgeColor::RED.0
        || combined == EdgeColor::GREEN.0
        || combined == EdgeColor::BLUE.0
    {
        *color = EdgeColor(combined ^ EdgeColor::WHITE.0);
        return;
    }
    if *color == EdgeColor::BLACK || *color == EdgeColor::WHITE {
        const START: [EdgeColor; 3] = [EdgeColor::CYAN, EdgeColor::MAGENTA, EdgeColor::YELLOW];
        *color = START[(*seed % 3) as usize];
        *seed /= 3;
        return;
    }
    // Rotating the two-bit mask by one or two positions always lands on another
    // two-bit mask that shares exactly one bit with the old one.
    let shifted = (color.0 as u32) << (1 + (*seed & 1));
    *color = EdgeColor(((shifted | (shifted >> 3)) & EdgeColor::WHITE.0 as u32) as u8);
    *seed >>= 1;
}

// ---------------------------------------------------------------------------
// Signed distance
// ---------------------------------------------------------------------------

/// A signed distance plus the tiebreaker used when two edges are equidistant.
#[derive(Copy, Clone, Debug, PartialEq)]
struct SignedDistance {
    /// Signed distance; positive is inside for a correctly oriented shape.
    distance: f32,
    /// `|cos|` of the angle between the edge's endpoint tangent and the
    /// direction to the query point, and zero when the closest point is in the
    /// segment's interior.
    ///
    /// Two edges meeting at a corner are exactly equidistant from any point in
    /// the corner's outer wedge. Preferring the smaller dot picks the edge more
    /// nearly perpendicular to the query direction, which is the one whose
    /// half-plane actually bounds the shape there. Without this tiebreaker the
    /// shared channel of a corner picks arbitrarily and the median collapses to
    /// the *nearer* half-plane instead of the farther one, turning every corner
    /// into a cross-shaped artifact.
    dot: f32,
}

impl SignedDistance {
    const INFINITE: Self = Self { distance: f32::NEG_INFINITY, dot: 1.0 };

    #[inline]
    fn closer_than(self, other: Self) -> bool {
        let (a, b) = (self.distance.abs(), other.distance.abs());
        a < b || (a == b && self.dot < other.dot)
    }
}

// ---------------------------------------------------------------------------
// Edge segments
// ---------------------------------------------------------------------------

/// One outline segment in em units, y-down.
///
/// Fonts only ever produce lines, quadratics (TrueType) and cubics (CFF), so
/// these three cases cover every outline without a general path representation.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum EdgeSegment {
    /// A straight line.
    Linear {
        /// Start point.
        p0: Point<f32>,
        /// End point.
        p1: Point<f32>,
    },
    /// A quadratic Bézier, as produced by TrueType `glyf` outlines.
    Quadratic {
        /// Start point.
        p0: Point<f32>,
        /// The single off-curve control point.
        control: Point<f32>,
        /// End point.
        p1: Point<f32>,
    },
    /// A cubic Bézier, as produced by CFF/Type 2 charstrings.
    Cubic {
        /// Start point.
        p0: Point<f32>,
        /// First control point.
        control0: Point<f32>,
        /// Second control point.
        control1: Point<f32>,
        /// End point.
        p1: Point<f32>,
    },
}

impl EdgeSegment {
    /// The segment's first point.
    #[inline]
    pub fn start(&self) -> Point<f32> {
        match *self {
            EdgeSegment::Linear { p0, .. }
            | EdgeSegment::Quadratic { p0, .. }
            | EdgeSegment::Cubic { p0, .. } => p0,
        }
    }

    /// The segment's last point.
    #[inline]
    pub fn end(&self) -> Point<f32> {
        match *self {
            EdgeSegment::Linear { p1, .. }
            | EdgeSegment::Quadratic { p1, .. }
            | EdgeSegment::Cubic { p1, .. } => p1,
        }
    }

    /// Evaluates the segment at `t` in `[0, 1]`.
    pub fn point(&self, t: f32) -> Point<f32> {
        match *self {
            EdgeSegment::Linear { p0, p1 } => lerp(p0, p1, t),
            EdgeSegment::Quadratic { p0, control, p1 } => {
                lerp(lerp(p0, control, t), lerp(control, p1, t), t)
            }
            EdgeSegment::Cubic { p0, control0, control1, p1 } => {
                let a = lerp(p0, control0, t);
                let b = lerp(control0, control1, t);
                let c = lerp(control1, p1, t);
                lerp(lerp(a, b, t), lerp(b, c, t), t)
            }
        }
    }

    /// The (unnormalised) tangent at `t`.
    ///
    /// Falls back to a chord when the derivative vanishes, which happens when a
    /// control point coincides with an endpoint — common in real fonts and fatal
    /// for corner detection if it returned a zero vector.
    pub fn direction(&self, t: f32) -> Point<f32> {
        match *self {
            EdgeSegment::Linear { p0, p1 } => vsub(p1, p0),
            EdgeSegment::Quadratic { p0, control, p1 } => {
                let tangent = lerp(vsub(control, p0), vsub(p1, control), t);
                if tangent.x == 0.0 && tangent.y == 0.0 { vsub(p1, p0) } else { tangent }
            }
            EdgeSegment::Cubic { p0, control0, control1, p1 } => {
                let a = vsub(control0, p0);
                let b = vsub(control1, control0);
                let c = vsub(p1, control1);
                let tangent = lerp(lerp(a, b, t), lerp(b, c, t), t);
                if tangent.x == 0.0 && tangent.y == 0.0 {
                    if t == 0.0 {
                        return vsub(control1, p0);
                    }
                    if t == 1.0 {
                        return vsub(p1, control0);
                    }
                }
                tangent
            }
        }
    }

    /// The portion of this segment between `t0` and `t1`, as a segment of the
    /// same degree.
    ///
    /// Uses the polar form (blossom) of the Bézier, which gives the exact
    /// control points of the sub-curve in closed form; de Casteljau splitting
    /// twice would accumulate more error and is no simpler here.
    pub fn sub_segment(&self, t0: f32, t1: f32) -> Self {
        match *self {
            EdgeSegment::Linear { .. } => {
                EdgeSegment::Linear { p0: self.point(t0), p1: self.point(t1) }
            }
            EdgeSegment::Quadratic { p0, control, p1 } => EdgeSegment::Quadratic {
                p0: self.point(t0),
                control: blossom2(p0, control, p1, t0, t1),
                p1: self.point(t1),
            },
            EdgeSegment::Cubic { p0, control0, control1, p1 } => EdgeSegment::Cubic {
                p0: self.point(t0),
                control0: blossom3(p0, control0, control1, p1, t0, t0, t1),
                control1: blossom3(p0, control0, control1, p1, t0, t1, t1),
                p1: self.point(t1),
            },
        }
    }

    /// Splits into three equal-parameter pieces.
    ///
    /// Used by the coloring pass: a contour with fewer than three edges cannot
    /// carry three colors, so its edges are subdivided until it can.
    pub fn split_in_thirds(&self) -> [Self; 3] {
        [
            self.sub_segment(0.0, 1.0 / 3.0),
            self.sub_segment(1.0 / 3.0, 2.0 / 3.0),
            self.sub_segment(2.0 / 3.0, 1.0),
        ]
    }

    /// The segment traversed backwards.
    pub fn reversed(&self) -> Self {
        match *self {
            EdgeSegment::Linear { p0, p1 } => EdgeSegment::Linear { p0: p1, p1: p0 },
            EdgeSegment::Quadratic { p0, control, p1 } => {
                EdgeSegment::Quadratic { p0: p1, control, p1: p0 }
            }
            EdgeSegment::Cubic { p0, control0, control1, p1 } => {
                EdgeSegment::Cubic { p0: p1, control0: control1, control1: control0, p1: p0 }
            }
        }
    }

    /// The exact bounding box of the segment.
    ///
    /// Exact rather than the control-point hull: the hull of a cubic can be far
    /// larger than the curve, and the box decides how much padding the glyph's
    /// field carries, i.e. atlas space.
    pub fn bounds(&self) -> Rect<f32> {
        let (mut min, mut max) = (self.start(), self.start());
        let mut include = |p: Point<f32>| {
            min = v(min.x.min(p.x), min.y.min(p.y));
            max = v(max.x.max(p.x), max.y.max(p.y));
        };
        include(self.end());
        match *self {
            EdgeSegment::Linear { .. } => {}
            EdgeSegment::Quadratic { p0, control, p1 } => {
                // The derivative is linear in each axis:
                // 2[(c - p0) + t((p1 - c) - (c - p0))].
                for (a, c, e) in [(p0.x, control.x, p1.x), (p0.y, control.y, p1.y)] {
                    let denom = a - 2.0 * c + e;
                    if denom.abs() > f32::EPSILON {
                        let t = (a - c) / denom;
                        if t > 0.0 && t < 1.0 {
                            include(self.point(t));
                        }
                    }
                }
            }
            EdgeSegment::Cubic { p0, control0, control1, p1 } => {
                // The derivative is quadratic in each axis, with coefficients
                // built from the first differences of the control points.
                for (q0, q1, q2, q3) in
                    [(p0.x, control0.x, control1.x, p1.x), (p0.y, control0.y, control1.y, p1.y)]
                {
                    let a0 = q1 - q0;
                    let a1 = q2 - q1;
                    let a2 = q3 - q2;
                    let roots = solve_quadratic(a0 - 2.0 * a1 + a2, 2.0 * (a1 - a0), a0);
                    for &t in roots.as_slice() {
                        if t > 0.0 && t < 1.0 {
                            include(self.point(t));
                        }
                    }
                }
            }
        }
        Rect::new(min, Size::new(max.x - min.x, max.y - min.y))
    }

    /// Appends a polyline approximation to `out`, excluding the start point.
    ///
    /// The segment count comes from the curve's second difference, which bounds
    /// the chord error and shrinks with `1/n^2`, so a tolerance in em units maps
    /// directly onto a segment count without any adaptive recursion.
    pub fn flatten_into(&self, tolerance: f32, out: &mut Vec<Point<f32>>) {
        let tolerance = tolerance.max(1e-7);
        let n = match *self {
            EdgeSegment::Linear { .. } => 1,
            EdgeSegment::Quadratic { p0, control, p1 } => {
                let d = length(vsub(vadd(p0, p1), vscale(control, 2.0)));
                segment_count(d / 4.0, tolerance)
            }
            EdgeSegment::Cubic { p0, control0, control1, p1 } => {
                let d0 = length(vsub(vadd(p0, control1), vscale(control0, 2.0)));
                let d1 = length(vsub(vadd(control0, p1), vscale(control1, 2.0)));
                segment_count(d0.max(d1), tolerance)
            }
        };
        for i in 1..=n {
            out.push(self.point(i as f32 / n as f32));
        }
    }

    /// The true signed distance from `origin`, plus the parameter of the closest
    /// point (which may fall outside `[0, 1]` when the closest point is an
    /// endpoint).
    fn signed_distance(&self, origin: Point<f32>) -> (SignedDistance, f32) {
        match *self {
            EdgeSegment::Linear { p0, p1 } => linear_signed_distance(p0, p1, origin),
            EdgeSegment::Quadratic { p0, control, p1 } => {
                quadratic_signed_distance(p0, control, p1, origin)
            }
            EdgeSegment::Cubic { p0, control0, control1, p1 } => {
                cubic_signed_distance(p0, control0, control1, p1, origin)
            }
        }
    }

    /// Converts a true distance into a pseudo-distance in place.
    ///
    /// Only does anything when the closest point was an endpoint: inside the
    /// segment the two agree. Past an endpoint the tangent line is extended and
    /// its perpendicular distance used, but only when it is *smaller* in
    /// magnitude, so the field never grows discontinuously.
    fn apply_pseudo_distance(&self, distance: &mut SignedDistance, origin: Point<f32>, param: f32) {
        if param < 0.0 {
            let dir = normalize(self.direction(0.0));
            let aq = vsub(origin, self.point(0.0));
            if dot(aq, dir) < 0.0 {
                let pseudo = cross(aq, dir);
                if pseudo.abs() <= distance.distance.abs() {
                    distance.distance = pseudo;
                    distance.dot = 0.0;
                }
            }
        } else if param > 1.0 {
            let dir = normalize(self.direction(1.0));
            let bq = vsub(origin, self.point(1.0));
            if dot(bq, dir) > 0.0 {
                let pseudo = cross(bq, dir);
                if pseudo.abs() <= distance.distance.abs() {
                    distance.distance = pseudo;
                    distance.dot = 0.0;
                }
            }
        }
    }
}

fn segment_count(deviation: f32, tolerance: f32) -> u32 {
    if !deviation.is_finite() || deviation <= 0.0 {
        return 1;
    }
    ((deviation / tolerance).sqrt().ceil() as u32).clamp(1, 128)
}

/// Polar form of a quadratic Bézier.
fn blossom2(p0: Point<f32>, c: Point<f32>, p1: Point<f32>, u: f32, w: f32) -> Point<f32> {
    let a = (1.0 - u) * (1.0 - w);
    let b = (1.0 - u) * w + (1.0 - w) * u;
    let d = u * w;
    v(a * p0.x + b * c.x + d * p1.x, a * p0.y + b * c.y + d * p1.y)
}

/// Polar form of a cubic Bézier.
fn blossom3(
    p0: Point<f32>,
    c0: Point<f32>,
    c1: Point<f32>,
    p1: Point<f32>,
    u: f32,
    w: f32,
    z: f32,
) -> Point<f32> {
    let (iu, iw, iz) = (1.0 - u, 1.0 - w, 1.0 - z);
    let a = iu * iw * iz;
    let b = iu * iw * z + iu * w * iz + u * iw * iz;
    let c = iu * w * z + u * iw * z + u * w * iz;
    let d = u * w * z;
    v(a * p0.x + b * c0.x + c * c1.x + d * p1.x, a * p0.y + b * c0.y + c * c1.y + d * p1.y)
}

fn linear_signed_distance(
    p0: Point<f32>,
    p1: Point<f32>,
    origin: Point<f32>,
) -> (SignedDistance, f32) {
    let aq = vsub(origin, p0);
    let ab = vsub(p1, p0);
    let ab_len2 = dot(ab, ab);
    if ab_len2 <= 0.0 {
        // A zero-length line has no direction and would produce NaN parameters.
        // Degenerate edges are filtered out on extraction; this guard exists so a
        // hand-built shape cannot poison the whole field with NaNs.
        return (SignedDistance { distance: -length(aq), dot: 1.0 }, 0.0);
    }
    let param = dot(aq, ab) / ab_len2;
    let eq = vsub(if param > 0.5 { p1 } else { p0 }, origin);
    let endpoint_distance = length(eq);
    if param > 0.0 && param < 1.0 {
        let ortho_distance = dot(orthonormal(ab), aq);
        if ortho_distance.abs() < endpoint_distance {
            return (SignedDistance { distance: ortho_distance, dot: 0.0 }, param);
        }
    }
    let distance = non_zero_sign(cross(aq, ab)) * endpoint_distance;
    let d = dot(normalize(ab), normalize(eq)).abs();
    (SignedDistance { distance, dot: d }, param)
}

fn quadratic_signed_distance(
    p0: Point<f32>,
    control: Point<f32>,
    p1: Point<f32>,
    origin: Point<f32>,
) -> (SignedDistance, f32) {
    let qa = vsub(p0, origin);
    let ab = vsub(control, p0);
    let br = vsub(vsub(p1, control), ab);

    // |Q(t) - origin|^2 is a quartic, so its derivative is the cubic below.
    let a = dot(br, br);
    let b = 3.0 * dot(ab, br);
    let c = 2.0 * dot(ab, ab) + dot(qa, br);
    let d = dot(qa, ab);
    let roots = solve_cubic(a, b, c, d);

    let seg = EdgeSegment::Quadratic { p0, control, p1 };
    let ep_dir0 = seg.direction(0.0);
    let mut min_distance = non_zero_sign(cross(ep_dir0, qa)) * length(qa);
    let mut param = -dot(qa, ep_dir0) / dot(ep_dir0, ep_dir0);
    {
        let ep_dir1 = seg.direction(1.0);
        let bq = vsub(p1, origin);
        let distance = length(bq);
        if distance < min_distance.abs() {
            min_distance = non_zero_sign(cross(ep_dir1, bq)) * distance;
            param = dot(vsub(origin, control), ep_dir1) / dot(ep_dir1, ep_dir1);
        }
    }
    for &t in roots.as_slice() {
        if t > 0.0 && t < 1.0 {
            let qe = vadd(vadd(qa, vscale(ab, 2.0 * t)), vscale(br, t * t));
            let distance = length(qe);
            if distance <= min_distance.abs() {
                min_distance = non_zero_sign(cross(vadd(ab, vscale(br, t)), qe)) * distance;
                param = t;
            }
        }
    }

    let dotv = if (0.0..=1.0).contains(&param) {
        0.0
    } else if param < 0.5 {
        dot(normalize(seg.direction(0.0)), normalize(qa)).abs()
    } else {
        dot(normalize(seg.direction(1.0)), normalize(vsub(p1, origin))).abs()
    };
    (SignedDistance { distance: min_distance, dot: dotv }, param)
}

/// Number of evenly spaced starting parameters for the cubic search.
const CUBIC_SEARCH_STARTS: usize = 4;
/// Newton refinement steps taken from each start.
const CUBIC_SEARCH_STEPS: usize = 4;

/// Closest point on a cubic, by Newton refinement from several seeds.
///
/// The exact answer needs the roots of a quintic, which has no closed form. Five
/// evenly spaced seeds with four Newton steps each is the same budget `msdfgen`
/// uses and converges to well under a thousandth of an em on font-shaped curves;
/// the failure mode is an S-curve whose two local minima are nearly equal, where
/// the search can settle on the wrong one and shift that texel's distance by a
/// fraction of a texel. Raising the seed count trades generation time for that,
/// and generation happens once per glyph, not per frame.
fn cubic_signed_distance(
    p0: Point<f32>,
    control0: Point<f32>,
    control1: Point<f32>,
    p1: Point<f32>,
    origin: Point<f32>,
) -> (SignedDistance, f32) {
    let qa = vsub(p0, origin);
    let ab = vsub(control0, p0);
    let br = vsub(vsub(control1, control0), ab);
    let as_ = vsub(vsub(vsub(p1, control1), vsub(control1, control0)), br);

    let seg = EdgeSegment::Cubic { p0, control0, control1, p1 };
    let ep_dir0 = seg.direction(0.0);
    let mut min_distance = non_zero_sign(cross(ep_dir0, qa)) * length(qa);
    let mut param = -dot(qa, ep_dir0) / dot(ep_dir0, ep_dir0);
    {
        let ep_dir1 = seg.direction(1.0);
        let bq = vsub(p1, origin);
        let distance = length(bq);
        if distance < min_distance.abs() {
            min_distance = non_zero_sign(cross(ep_dir1, bq)) * distance;
            param = dot(vsub(ep_dir1, bq), ep_dir1) / dot(ep_dir1, ep_dir1);
        }
    }

    for i in 0..=CUBIC_SEARCH_STARTS {
        let mut t = i as f32 / CUBIC_SEARCH_STARTS as f32;
        let mut qe = cubic_offset(qa, ab, br, as_, t);
        for _ in 0..CUBIC_SEARCH_STEPS {
            // d1 and d2 are the first and second derivatives of the curve; the
            // update is one Newton step on d/dt |Q(t) - origin|^2.
            let d1 = vadd(vadd(vscale(ab, 3.0), vscale(br, 6.0 * t)), vscale(as_, 3.0 * t * t));
            let d2 = vadd(vscale(br, 6.0), vscale(as_, 6.0 * t));
            let denom = dot(d1, d1) + dot(qe, d2);
            if denom == 0.0 {
                break;
            }
            t -= dot(qe, d1) / denom;
            if !(t > 0.0 && t < 1.0) {
                break;
            }
            qe = cubic_offset(qa, ab, br, as_, t);
            let distance = length(qe);
            if distance < min_distance.abs() {
                min_distance = non_zero_sign(cross(d1, qe)) * distance;
                param = t;
            }
        }
    }

    let dotv = if (0.0..=1.0).contains(&param) {
        0.0
    } else if param < 0.5 {
        dot(normalize(seg.direction(0.0)), normalize(qa)).abs()
    } else {
        dot(normalize(seg.direction(1.0)), normalize(vsub(p1, origin))).abs()
    };
    (SignedDistance { distance: min_distance, dot: dotv }, param)
}

#[inline]
fn cubic_offset(
    qa: Point<f32>,
    ab: Point<f32>,
    br: Point<f32>,
    as_: Point<f32>,
    t: f32,
) -> Point<f32> {
    vadd(vadd(qa, vscale(ab, 3.0 * t)), vadd(vscale(br, 3.0 * t * t), vscale(as_, t * t * t)))
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

/// An outline segment together with the channels it contributes to.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ColoredEdge {
    /// The geometry.
    pub segment: EdgeSegment,
    /// Which distance channels this edge participates in.
    pub color: EdgeColor,
}

impl ColoredEdge {
    /// A white (all-channel) edge, the state before coloring runs.
    #[inline]
    pub fn new(segment: EdgeSegment) -> Self {
        Self { segment, color: EdgeColor::WHITE }
    }
}

/// A closed sequence of edges.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contour {
    /// Edges in traversal order; the last edge ends where the first begins.
    pub edges: Vec<ColoredEdge>,
}

impl Contour {
    /// An empty contour.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an edge, skipping degenerate ones.
    ///
    /// Zero-length edges carry no direction, which would make corner detection
    /// and every distance sign meaningless, and real fonts do contain them.
    pub fn push(&mut self, segment: EdgeSegment) {
        if is_degenerate(&segment) {
            return;
        }
        self.edges.push(ColoredEdge::new(segment));
    }

    /// Reverses traversal order and every edge with it.
    pub fn reverse(&mut self) {
        self.edges.reverse();
        for e in &mut self.edges {
            e.segment = e.segment.reversed();
        }
    }

    /// A closed polyline approximation, first point repeated at the end.
    pub fn flatten(&self, tolerance: f32) -> Vec<Point<f32>> {
        let mut out = Vec::new();
        let Some(first) = self.edges.first() else { return out };
        out.push(first.segment.start());
        for e in &self.edges {
            e.segment.flatten_into(tolerance, &mut out);
        }
        match (out.first().copied(), out.last().copied()) {
            (Some(f), Some(l)) if f.x != l.x || f.y != l.y => out.push(f),
            _ => {}
        }
        out
    }

    /// Assigns edge colors for this contour alone.
    ///
    /// `cross_threshold` is the sine of the corner angle threshold.
    fn color(&mut self, cross_threshold: f32, seed: &mut u64) {
        if self.edges.is_empty() {
            return;
        }
        // A one-edge contour cannot express three colors. Splitting first means
        // the teardrop and smooth cases below always have something to work with.
        if self.edges.len() == 1 {
            let parts = self.edges[0].segment.split_in_thirds();
            self.edges = parts.iter().copied().map(ColoredEdge::new).collect();
        }

        let mut corners: Vec<usize> = Vec::new();
        let mut prev_dir = normalize(self.edges[self.edges.len() - 1].segment.direction(1.0));
        for (i, e) in self.edges.iter().enumerate() {
            let dir = normalize(e.segment.direction(0.0));
            if is_corner(prev_dir, dir, cross_threshold) {
                corners.push(i);
            }
            prev_dir = normalize(e.segment.direction(1.0));
        }

        match corners.len() {
            // Fully smooth: nothing to protect, so one color for the whole
            // contour. All three channels then agree and the median degenerates
            // to a plain signed distance, which is exactly right for a shape with
            // no corners.
            0 => {
                for e in &mut self.edges {
                    e.color = EdgeColor::WHITE;
                }
            }
            1 => self.color_teardrop(corners[0], seed),
            _ => self.color_multi(&corners, seed),
        }
    }

    /// A contour with exactly one corner — a comma, a lowercase `e` terminal, a
    /// teardrop counter.
    ///
    /// The single corner still has to be protected, so the contour is banded into
    /// three colors centred on the corner, subdividing edges when there are fewer
    /// than three of them.
    fn color_teardrop(&mut self, corner: usize, seed: &mut u64) {
        let mut colors = [EdgeColor::WHITE, EdgeColor::WHITE, EdgeColor::BLACK];
        switch_color(&mut colors[0], seed, EdgeColor::BLACK);
        colors[2] = colors[0];
        switch_color(&mut colors[2], seed, EdgeColor::BLACK);

        let m = self.edges.len();
        if m >= 3 {
            for i in 0..m {
                let index = (corner + i) % m;
                let band = 1 + symmetrical_trichotomy(i, m);
                self.edges[index].color = colors[band as usize];
            }
        } else if m == 2 {
            // Six thirds, rotated so the corner sits between parts[2] and
            // parts[3]; pairs of thirds share a color so the corner is flanked by
            // two different colors.
            let first = self.edges[0].segment.split_in_thirds();
            let second = self.edges[1].segment.split_in_thirds();
            let mut parts = [first[0]; 6];
            if corner == 0 {
                parts[..3].copy_from_slice(&first);
                parts[3..].copy_from_slice(&second);
            } else {
                parts[..3].copy_from_slice(&second);
                parts[3..].copy_from_slice(&first);
            }
            let order = [colors[0], colors[0], colors[1], colors[1], colors[2], colors[2]];
            self.edges = parts
                .iter()
                .zip(order)
                .map(|(&segment, color)| ColoredEdge { segment, color })
                .collect();
        } else {
            let parts = self.edges[0].segment.split_in_thirds();
            self.edges = parts
                .iter()
                .zip(colors)
                .map(|(&segment, color)| ColoredEdge { segment, color })
                .collect();
        }
    }

    /// Two or more corners: walk from the first corner and switch color at every
    /// subsequent one, banning the initial color on the last run so the wrap-around
    /// corner is protected too.
    fn color_multi(&mut self, corners: &[usize], seed: &mut u64) {
        let corner_count = corners.len();
        let m = self.edges.len();
        let start = corners[0];
        let mut spline = 0usize;
        let mut color = EdgeColor::WHITE;
        switch_color(&mut color, seed, EdgeColor::BLACK);
        let initial = color;
        for i in 0..m {
            let index = (start + i) % m;
            if spline + 1 < corner_count && corners[spline + 1] == index {
                spline += 1;
                let banned = if spline == corner_count - 1 { initial } else { EdgeColor::BLACK };
                switch_color(&mut color, seed, banned);
            }
            self.edges[index].color = color;
        }
    }
}

/// Maps a position within a teardrop contour onto one of three bands.
///
/// Reproduces `msdfgen`'s `symmetricalTrichotomy`: the first and last edges get
/// the two flanking colors and everything in between the middle one, with the
/// split biased so short contours still get all three bands.
fn symmetrical_trichotomy(position: usize, n: usize) -> i32 {
    debug_assert!(n >= 2);
    ((3.0 + 2.875 * position as f32 / (n - 1) as f32 - 1.4375 + 0.5) as i32) - 3
}

fn is_corner(a_dir: Point<f32>, b_dir: Point<f32>, cross_threshold: f32) -> bool {
    dot(a_dir, b_dir) <= 0.0 || cross(a_dir, b_dir).abs() > cross_threshold
}

fn is_degenerate(segment: &EdgeSegment) -> bool {
    const EPS: f32 = 1e-9;
    let close = |a: Point<f32>, b: Point<f32>| (a.x - b.x).abs() < EPS && (a.y - b.y).abs() < EPS;
    match *segment {
        EdgeSegment::Linear { p0, p1 } => close(p0, p1),
        EdgeSegment::Quadratic { p0, control, p1 } => close(p0, control) && close(control, p1),
        EdgeSegment::Cubic { p0, control0, control1, p1 } => {
            close(p0, control0) && close(control0, control1) && close(control1, p1)
        }
    }
}

/// A glyph outline as closed contours in em units, y-down.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shape {
    /// The contours. Outer contours and counters are not distinguished; fill is
    /// decided by the nonzero winding rule, exactly as in the font format.
    pub contours: Vec<Contour>,
}

impl Shape {
    /// An empty shape.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the shape has no edges at all, as for a space glyph.
    pub fn is_empty(&self) -> bool {
        self.contours.iter().all(|c| c.edges.is_empty())
    }

    /// Total edge count across every contour.
    pub fn edge_count(&self) -> usize {
        self.contours.iter().map(|c| c.edges.len()).sum()
    }

    /// Builds a shape from one closed polygon, for tests and synthetic glyphs.
    ///
    /// The polygon is closed automatically; fewer than two distinct points
    /// produces an empty shape rather than a degenerate contour.
    pub fn from_polygon(points: &[Point<f32>]) -> Self {
        let mut contour = Contour::new();
        for i in 0..points.len() {
            let a = points[i];
            let b = points[(i + 1) % points.len()];
            contour.push(EdgeSegment::Linear { p0: a, p1: b });
        }
        if contour.edges.len() < 2 {
            return Self::new();
        }
        Self { contours: vec![contour] }
    }

    /// The exact ink bounding box, or `None` when the shape has no edges.
    pub fn bounds(&self) -> Option<Rect<f32>> {
        let mut acc: Option<(Point<f32>, Point<f32>)> = None;
        for contour in &self.contours {
            for edge in &contour.edges {
                let b = edge.segment.bounds();
                let (lo, hi) = (b.origin, b.max_point());
                acc = Some(match acc {
                    None => (lo, hi),
                    Some((min, max)) => {
                        (v(min.x.min(lo.x), min.y.min(lo.y)), v(max.x.max(hi.x), max.y.max(hi.y)))
                    }
                });
            }
        }
        acc.map(|(min, max)| Rect::new(min, Size::new(max.x - min.x, max.y - min.y)))
    }

    /// Reverses every contour.
    pub fn reverse(&mut self) {
        for c in &mut self.contours {
            c.reverse();
        }
    }

    /// Closed polyline approximations of every contour.
    pub fn flatten(&self, tolerance: f32) -> Vec<Vec<Point<f32>>> {
        self.contours.iter().filter(|c| !c.edges.is_empty()).map(|c| c.flatten(tolerance)).collect()
    }

    /// Rewinds the shape so that the filled side is the positive side.
    ///
    /// Every signed distance in this module takes its sign from edge direction,
    /// so the convention has to be pinned down once. TrueType winds outer
    /// contours clockwise in its own y-up space and CFF winds them
    /// counter-clockwise; after the y flip that is two opposite conventions
    /// reaching the same code. Deciding from the shape's own total signed area
    /// makes the module independent of which format the glyph came from, and lets
    /// a hand-built test shape be wound either way.
    ///
    /// Returns `true` if the shape was reversed.
    pub fn orient(&mut self, tolerance: f32) -> bool {
        let area: f32 = self.flatten(tolerance).iter().map(|p| polygon_area(p)).sum();
        if area > 0.0 {
            self.reverse();
            true
        } else {
            false
        }
    }

    /// Assigns a channel mask to every edge.
    ///
    /// `angle_threshold` is in radians; a vertex whose incoming and outgoing
    /// tangents differ by more than that counts as a corner.
    pub fn color_edges(&mut self, angle_threshold: f32) {
        self.color_edges_seeded(angle_threshold, 0);
    }

    /// [`Shape::color_edges`] with an explicit seed for the color rotation.
    pub fn color_edges_seeded(&mut self, angle_threshold: f32, seed: u64) {
        let cross_threshold = angle_threshold.abs().sin();
        let mut seed = seed;
        for contour in &mut self.contours {
            contour.color(cross_threshold, &mut seed);
        }
    }
}

/// Twice-signed polygon area halved; negative for a counter-clockwise polygon in
/// y-down space, which is the orientation this module treats as "filled".
fn polygon_area(points: &[Point<f32>]) -> f32 {
    let mut sum = 0.0;
    for i in 0..points.len() {
        let a = points[i];
        let b = points[(i + 1) % points.len()];
        sum += cross(a, b);
    }
    0.5 * sum
}

// ---------------------------------------------------------------------------
// Outline extraction
// ---------------------------------------------------------------------------

/// Collects a `ttf-parser` outline into em-space, y-down contours.
struct OutlineCollector {
    scale: f32,
    shape: Shape,
    current: Contour,
    start: Point<f32>,
    pen: Point<f32>,
}

impl OutlineCollector {
    fn new(units_per_em: f32) -> Self {
        Self {
            scale: 1.0 / units_per_em,
            shape: Shape::new(),
            current: Contour::new(),
            start: v(0.0, 0.0),
            pen: v(0.0, 0.0),
        }
    }

    /// Font units to em units, flipping y: font space is y-up, Sphere is y-down.
    #[inline]
    fn map(&self, x: f32, y: f32) -> Point<f32> {
        v(x * self.scale, -y * self.scale)
    }

    fn finish_contour(&mut self) {
        if self.pen != self.start {
            self.current.push(EdgeSegment::Linear { p0: self.pen, p1: self.start });
            self.pen = self.start;
        }
        if !self.current.edges.is_empty() {
            self.shape.contours.push(core::mem::take(&mut self.current));
        } else {
            self.current.edges.clear();
        }
    }

    fn finish(mut self) -> Shape {
        self.finish_contour();
        self.shape
    }
}

impl ttf_parser::OutlineBuilder for OutlineCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.finish_contour();
        self.start = self.map(x, y);
        self.pen = self.start;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p1 = self.map(x, y);
        self.current.push(EdgeSegment::Linear { p0: self.pen, p1 });
        self.pen = p1;
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let control = self.map(x1, y1);
        let p1 = self.map(x, y);
        self.current.push(EdgeSegment::Quadratic { p0: self.pen, control, p1 });
        self.pen = p1;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let control0 = self.map(x1, y1);
        let control1 = self.map(x2, y2);
        let p1 = self.map(x, y);
        self.current.push(EdgeSegment::Cubic { p0: self.pen, control0, control1, p1 });
        self.pen = p1;
    }

    fn close(&mut self) {
        self.finish_contour();
    }
}

/// Extracts a glyph outline as a [`Shape`] in em units, y-down.
///
/// A glyph with no outline (a space, or a glyph defined only by a bitmap) yields
/// an empty shape rather than an error: it is a perfectly normal thing to shape
/// and lay out, it simply draws nothing.
pub fn extract_shape(face: &ttf_parser::Face<'_>, glyph: GlyphId) -> Result<Shape, FontError> {
    if glyph.0 >= face.number_of_glyphs() {
        return Err(FontError::MissingGlyph(glyph));
    }
    let upem = face.units_per_em().max(1) as f32;
    let mut collector = OutlineCollector::new(upem);
    if face.outline_glyph(ttf_parser::GlyphId(glyph.0), &mut collector).is_none() {
        return Ok(Shape::new());
    }
    Ok(collector.finish())
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default texels per em.
pub const DEFAULT_EM_SIZE_PX: f32 = 48.0;
/// Default distance range in texels.
pub const DEFAULT_RANGE_PX: f32 = 4.0;
/// Default corner threshold: three degrees, in radians.
///
/// Small enough that a genuine corner in a light weight is never missed, large
/// enough that the many near-collinear joins in a curve approximation are not
/// each treated as a corner (which would use up all three colors within one
/// smooth arc and produce banding).
pub const DEFAULT_ANGLE_THRESHOLD: f32 = 3.0 * core::f32::consts::PI / 180.0;

/// How a glyph's distance field is sampled.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GlyphRasterConfig {
    /// Texels per em.
    ///
    /// 32 is adequate for UI text, 64 preserves detail into display sizes at four
    /// times the atlas footprint. See the module docs for the tradeoff.
    pub em_size_px: f32,
    /// How far the field extends either side of the outline, in texels.
    ///
    /// This is the smoothing budget the shader has, and also the widest outline
    /// or glow it can synthesise, paid for with padding around every glyph.
    pub range_px: f32,
    /// Corner detection threshold in radians.
    pub angle_threshold: f32,
    /// Seed for the deterministic color rotation.
    pub seed: u64,
    /// Whether to run the median-versus-true-distance repair pass.
    ///
    /// Only ever turned off to inspect the raw field while debugging; leaving it
    /// off puts visible notches on diagonal stems.
    pub error_correction: bool,
    /// Hard cap on either texel dimension.
    ///
    /// A malformed font can report an outline spanning thousands of ems; without
    /// a cap that becomes a multi-gigabyte allocation driven by file contents.
    pub max_texels: u32,
}

impl Default for GlyphRasterConfig {
    fn default() -> Self {
        Self {
            em_size_px: DEFAULT_EM_SIZE_PX,
            range_px: DEFAULT_RANGE_PX,
            angle_threshold: DEFAULT_ANGLE_THRESHOLD,
            seed: 0,
            error_correction: true,
            max_texels: 512,
        }
    }
}

impl GlyphRasterConfig {
    /// A configuration with the given resolution and range, everything else
    /// left at its default.
    pub fn new(em_size_px: f32, range_px: f32) -> Self {
        Self { em_size_px, range_px, ..Default::default() }
    }

    /// The distance range in em units, which is what travels with the glyph.
    #[inline]
    pub fn range_em(&self) -> f32 {
        self.range_px.max(0.0) / self.em_size_px_clamped()
    }

    #[inline]
    fn em_size_px_clamped(&self) -> f32 {
        if self.em_size_px.is_finite() && self.em_size_px >= 1.0 {
            self.em_size_px
        } else {
            DEFAULT_EM_SIZE_PX
        }
    }

    /// The texel cap, guaranteed to be at least one.
    ///
    /// `max_texels` is a public field, so a caller can set it to zero. Every
    /// glyph occupies at least one texel, so a zero cap has no coherent
    /// meaning; treating it as one keeps the dimension clamp well-formed
    /// instead of panicking on `clamp(1, 0)`.
    #[inline]
    fn max_texels_clamped(&self) -> u32 {
        self.max_texels.max(1)
    }
}

// ---------------------------------------------------------------------------
// Field generation
// ---------------------------------------------------------------------------

/// The median of three values, matching the `median()` every MSDF shader
/// defines.
///
/// Reconstruction has to use exactly this function or the CPU-side field and the
/// GPU-side sampling disagree about where the outline is.
#[inline]
pub fn median(a: f32, b: f32, c: f32) -> f32 {
    a.min(b).max(a.max(b).min(c))
}

/// Encodes a signed distance in em units into a texel value.
///
/// The encoding is `d / range + 0.5`, so 128 is exactly on the outline and the
/// representable band is `±range/2`; anything beyond saturates, which is why the
/// range has to be chosen for the largest size the glyph will be drawn at.
#[inline]
pub fn encode_distance(distance: f32, range: f32) -> u8 {
    let normalized = if range > 0.0 {
        distance / range + 0.5
    } else if distance > 0.0 {
        1.0
    } else {
        0.0
    };
    encode_normalized(normalized)
}

#[inline]
fn encode_normalized(normalized: f32) -> u8 {
    if normalized.is_nan() {
        return 0;
    }
    (normalized.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Recovers a signed distance in em units from a texel value.
#[inline]
pub fn decode_distance(texel: u8, range: f32) -> f32 {
    (texel as f32 / 255.0 - 0.5) * range
}

/// Per-channel state while searching for the nearest edge.
#[derive(Copy, Clone)]
struct Pick {
    distance: SignedDistance,
    edge: usize,
    param: f32,
}

impl Pick {
    const NONE: Self = Self { distance: SignedDistance::INFINITE, edge: usize::MAX, param: 0.0 };

    #[inline]
    fn consider(&mut self, distance: SignedDistance, edge: usize, param: f32) {
        if distance.closer_than(self.distance) {
            *self = Self { distance, edge, param };
        }
    }
}

/// An edge plus the box used to skip it cheaply.
#[derive(Copy, Clone)]
struct EdgeEntry {
    edge: ColoredEdge,
    bounds: Rect<f32>,
}

/// Distance from a point to a rectangle, zero inside it.
#[inline]
fn point_rect_distance(p: Point<f32>, r: Rect<f32>) -> f32 {
    let dx = (r.origin.x - p.x).max(p.x - (r.origin.x + r.size.width)).max(0.0);
    let dy = (r.origin.y - p.y).max(p.y - (r.origin.y + r.size.height)).max(0.0);
    (dx * dx + dy * dy).sqrt()
}

/// Generates the MTSDF for one glyph of a face.
///
/// The image covers the glyph's ink box outset by `range_em` on every side, and
/// that is the quad the caller must draw: `image.bounds_em.outset(range_em)`
/// scaled by the font size, positioned at the pen. Drawing only `bounds_em`
/// would cut off the half of the field that lies outside the outline, taking the
/// antialiasing and any synthesised outline with it.
///
/// Returns an empty [`GlyphImage`] for glyphs with no outline, and
/// [`FontError::MissingGlyph`] only when the glyph index is not in the face at
/// all.
pub fn generate_mtsdf(
    face: &ttf_parser::Face<'_>,
    glyph: GlyphId,
    config: &GlyphRasterConfig,
) -> Result<GlyphImage, FontError> {
    let shape = extract_shape(face, glyph)?;
    Ok(generate_mtsdf_from_shape(&shape, config))
}

/// Generates an MTSDF from an outline already in em units, y-down.
///
/// Separate from [`generate_mtsdf`] so synthetic shapes can be fed straight in,
/// which is how the corner-preservation behaviour is tested without a font file.
/// See [`generate_mtsdf`] for how `bounds_em` and `range_em` relate to the quad
/// the caller has to draw.
pub fn generate_mtsdf_from_shape(shape: &Shape, config: &GlyphRasterConfig) -> GlyphImage {
    if shape.is_empty() {
        return GlyphImage::empty(GlyphFormat::Mtsdf);
    }
    let em_px = config.em_size_px_clamped();
    let range_em = config.range_em();
    let Some(ink) = shape.bounds() else {
        return GlyphImage::empty(GlyphFormat::Mtsdf);
    };

    // Padding by the full range on every side keeps the field from being clipped
    // where it is still carrying useful values.
    let padded = ink.outset(Edges::all(range_em));
    if !padded.size.width.is_finite() || !padded.size.height.is_finite() {
        return GlyphImage::empty(GlyphFormat::Mtsdf);
    }
    // The texel grid covers `padded` exactly rather than snapping to a whole
    // number of texels per em, so the caller can reconstruct the quad as
    // `bounds_em` outset by `range_em` with no rounding slack to communicate.
    // The cost is that rounding each axis independently scales the two axes by up
    // to one texel differently — a few per cent on a typical glyph. The stored
    // values are true em distances either way; only the sample positions shift.
    let width =
        ((padded.size.width * em_px).ceil().max(1.0) as u32).clamp(1, config.max_texels_clamped());
    let height =
        ((padded.size.height * em_px).ceil().max(1.0) as u32).clamp(1, config.max_texels_clamped());

    let flatten_tol = 0.25 / em_px;
    let mut shape = shape.clone();
    let mut polylines = shape.flatten(flatten_tol);
    // Orientation is decided once from the total signed area; see `Shape::orient`.
    // The polylines are reversed alongside the shape so the winding test below
    // and the edge-derived signs agree on which side is filled.
    if polylines.iter().map(|p| polygon_area(p)).sum::<f32>() > 0.0 {
        shape.reverse();
        for poly in &mut polylines {
            poly.reverse();
        }
    }
    shape.color_edges_seeded(config.angle_threshold, config.seed);

    let entries: Vec<EdgeEntry> = shape
        .contours
        .iter()
        .flat_map(|c| c.edges.iter())
        .map(|&edge| EdgeEntry { edge, bounds: edge.segment.bounds() })
        .collect();

    let (w, h) = (width as usize, height as usize);
    let sx = padded.size.width / width as f32;
    let sy = padded.size.height / height as f32;
    let mut field = vec![[0.0f32; 4]; w * h];
    let mut crossings: Vec<(f32, i32)> = Vec::new();
    let mut inside_row = vec![false; w];

    for y in 0..h {
        let py = padded.origin.y + (y as f32 + 0.5) * sy;
        row_winding(&polylines, py, padded.origin.x, sx, &mut crossings, &mut inside_row);
        for x in 0..w {
            let px = padded.origin.x + (x as f32 + 0.5) * sx;
            let p = v(px, py);
            let mut distances = texel_distances(&entries, p);

            // The winding rule is ground truth for which side of the outline a
            // texel is on, and it is what makes the alpha channel dependable
            // enough for the repair pass below to trust. Edge-derived signs can
            // still disagree with it at a reflex vertex or in a font with one
            // mis-wound contour. Within half a texel of the outline the
            // disagreement is more likely to be the flattening error in the
            // winding test than a real fault, and the analytic sign is the
            // accurate one there, so the guard leaves that band alone.
            let inside = inside_row[x];
            if distances[3].abs() > 2.0 * flatten_tol && (distances[3] > 0.0) != inside {
                for d in &mut distances {
                    *d = -*d;
                }
            }

            field[y * w + x] = [
                distances[0] / range_em + 0.5,
                distances[1] / range_em + 0.5,
                distances[2] / range_em + 0.5,
                distances[3] / range_em + 0.5,
            ];
        }
    }

    if config.error_correction {
        correct_errors(&mut field);
    }

    let mut data = Vec::with_capacity(w * h * 4);
    for texel in &field {
        for c in texel {
            data.push(encode_normalized(*c));
        }
    }

    GlyphImage { width, height, format: GlyphFormat::Mtsdf, data, bounds_em: ink, range_em }
}

/// Signed distances `[r, g, b, true]` in em units for one texel.
fn texel_distances(entries: &[EdgeEntry], p: Point<f32>) -> [f32; 4] {
    let (mut r, mut g, mut b) = (Pick::NONE, Pick::NONE, Pick::NONE);
    let mut true_distance = SignedDistance::INFINITE;

    for (i, entry) in entries.iter().enumerate() {
        // An edge whose bounding box is already farther away than every current
        // best cannot win any channel: the box distance is a lower bound on the
        // true distance, and channel selection is by true distance. Pseudo
        // conversion happens only to the winner, so skipping here is exact.
        let worst = r
            .distance
            .distance
            .abs()
            .max(g.distance.distance.abs())
            .max(b.distance.distance.abs())
            .max(true_distance.distance.abs());
        if point_rect_distance(p, entry.bounds) > worst {
            continue;
        }

        let (distance, param) = entry.edge.segment.signed_distance(p);
        if entry.edge.color.has_red() {
            r.consider(distance, i, param);
        }
        if entry.edge.color.has_green() {
            g.consider(distance, i, param);
        }
        if entry.edge.color.has_blue() {
            b.consider(distance, i, param);
        }
        if distance.closer_than(true_distance) {
            true_distance = distance;
        }
    }

    let mut out = [0.0f32; 4];
    for (slot, pick) in [(0, r), (1, g), (2, b)] {
        out[slot] = if pick.edge == usize::MAX {
            // No edge carries this channel at all, which happens only for a shape
            // whose every contour is smooth-and-white in one channel's absence.
            // Falling back to the true distance keeps the median meaningful.
            true_distance.distance
        } else {
            let mut d = pick.distance;
            entries[pick.edge].edge.segment.apply_pseudo_distance(&mut d, p, pick.param);
            d.distance
        };
    }
    out[3] = true_distance.distance;
    out
}

/// Fills `out` with the nonzero-winding inside/outside state of one texel row.
fn row_winding(
    polylines: &[Vec<Point<f32>>],
    y: f32,
    x_origin: f32,
    x_step: f32,
    crossings: &mut Vec<(f32, i32)>,
    out: &mut [bool],
) {
    crossings.clear();
    for poly in polylines {
        for pair in poly.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            // Half-open in y so a vertex sitting exactly on the scanline is
            // counted once, not zero or two times.
            if (a.y <= y) != (b.y <= y) {
                let t = (y - a.y) / (b.y - a.y);
                crossings.push((a.x + t * (b.x - a.x), if b.y > a.y { 1 } else { -1 }));
            }
        }
    }
    crossings.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));

    let mut winding = 0;
    let mut next = 0usize;
    for (i, slot) in out.iter_mut().enumerate() {
        let x = x_origin + (i as f32 + 0.5) * x_step;
        while next < crossings.len() && crossings[next].0 <= x {
            winding += crossings[next].1;
            next += 1;
        }
        *slot = winding != 0;
    }
}

/// One texel quantum in normalised units: a median wrong by less than this
/// encodes to the same byte either way, so repairing it would change nothing.
const REPAIR_EPSILON: f32 = 1.0 / 255.0;

/// Repairs texels where the median contradicts the true distance.
///
/// Where three channels disagree the median can land on the wrong side of the
/// outline — the classic MSDF failure, seen as isolated dots on a diagonal stem
/// or a notch bitten out of a join. The shader thresholds the median at 0.5, so
/// such a texel renders as solid where the glyph is empty, and the repair is to
/// clamp all three channels to the true distance already sitting in alpha.
///
/// The reference (three-channel) algorithm has to *infer* those texels by
/// comparing each against its neighbours, because it has no true distance to
/// check against. MTSDF does, and it is exact, so the neighbour scan is
/// subsumed: a median on the wrong side of the threshold is a spurious contour
/// crossing against every correctly reconstructed neighbour, by definition. The
/// sign test additionally catches clusters where *both* texels either side of a
/// boundary are wrong, which a pairwise neighbour test misses.
///
/// Note what is deliberately *not* repaired: a median that differs from the true
/// distance in magnitude but not in sign. That difference is the whole point of
/// the multi-channel encoding — it is what keeps a corner square instead of
/// rounded — and flattening it would undo the algorithm.
fn correct_errors(field: &mut [[f32; 4]]) {
    for t in field.iter_mut() {
        let m = median(t[0], t[1], t[2]) - 0.5;
        let a = t[3] - 0.5;
        if (m > 0.0) != (a > 0.0) && m.abs() > REPAIR_EPSILON {
            t[0] = t[3];
            t[1] = t[3];
            t[2] = t[3];
        }
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Per-glyph metrics in em units, y-down.
///
/// The bounding box comes from the actual outline rather than the `glyf` header,
/// which is allowed to be wrong and in practice sometimes is.
pub fn glyph_metrics(
    face: &ttf_parser::Face<'_>,
    glyph: GlyphId,
) -> Result<GlyphMetrics, FontError> {
    if glyph.0 >= face.number_of_glyphs() {
        return Err(FontError::MissingGlyph(glyph));
    }
    let gid = ttf_parser::GlyphId(glyph.0);
    let upem = face.units_per_em().max(1) as f32;
    let advance = face.glyph_hor_advance(gid).unwrap_or(0) as f32 / upem;
    let left_bearing = face.glyph_hor_side_bearing(gid).unwrap_or(0) as f32 / upem;
    let bounds = match face.glyph_bounding_box(gid) {
        Some(b) => Rect::from_corners(
            v(b.x_min as f32 / upem, -(b.y_max as f32) / upem),
            v(b.x_max as f32 / upem, -(b.y_min as f32) / upem),
        ),
        None => Rect::ZERO,
    };
    Ok(GlyphMetrics { advance, left_bearing, bounds })
}

/// Vertical metrics for a face, in em units.
///
/// `x_height` and `cap_height` fall back to measuring `x` and `H` when the
/// `OS/2` table omits them, which older and hand-built fonts routinely do, and
/// only then to a proportion of the ascent.
pub fn face_metrics(face: &ttf_parser::Face<'_>) -> FontMetrics {
    let units_per_em = face.units_per_em().max(1);
    let upem = units_per_em as f32;
    let ascent = face.ascender() as f32 / upem;
    let descent = -(face.descender() as f32) / upem;

    let measured = |ch: char| -> Option<f32> {
        let gid = face.glyph_index(ch)?;
        let bbox = face.glyph_bounding_box(gid)?;
        Some(bbox.y_max as f32 / upem)
    };
    let x_height = face
        .x_height()
        .map(|v| v as f32 / upem)
        .filter(|v| *v > 0.0)
        .or_else(|| measured('x'))
        .unwrap_or(0.5 * ascent);
    let cap_height = face
        .capital_height()
        .map(|v| v as f32 / upem)
        .filter(|v| *v > 0.0)
        .or_else(|| measured('H'))
        .unwrap_or(0.7 * ascent);

    let underline = face.underline_metrics();
    FontMetrics {
        ascent,
        descent,
        line_gap: face.line_gap() as f32 / upem,
        x_height,
        cap_height,
        // Font units put the underline below the baseline as a negative value;
        // Sphere measures it positive-down.
        underline_offset: underline.map(|m| -(m.position as f32) / upem).unwrap_or(0.1),
        underline_thickness: underline.map(|m| m.thickness as f32 / upem).unwrap_or(0.05),
        units_per_em,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f32, y: f32) -> Point<f32> {
        Point::new(x, y)
    }

    fn unit_square() -> Shape {
        Shape::from_polygon(&[p(0.0, 0.0), p(0.0, 1.0), p(1.0, 1.0), p(1.0, 0.0)])
    }

    // -- median and encoding -------------------------------------------------

    #[test]
    fn median_picks_the_middle_of_every_permutation() {
        for perm in [
            (1.0, 2.0, 3.0),
            (1.0, 3.0, 2.0),
            (2.0, 1.0, 3.0),
            (2.0, 3.0, 1.0),
            (3.0, 1.0, 2.0),
            (3.0, 2.0, 1.0),
        ] {
            assert_eq!(median(perm.0, perm.1, perm.2), 2.0, "{perm:?}");
        }
        assert_eq!(median(5.0, 5.0, -1.0), 5.0);
        assert_eq!(median(-1.0, 5.0, 5.0), 5.0);
        assert_eq!(median(0.0, 0.0, 0.0), 0.0);
        assert_eq!(median(-3.0, -1.0, -2.0), -2.0);
    }

    #[test]
    fn distance_encoding_round_trips_within_half_a_step() {
        let range = 0.125_f32;
        let step = range / 255.0;
        for i in -60..=60 {
            let d = i as f32 * range / 120.0;
            let back = decode_distance(encode_distance(d, range), range);
            assert!((back - d).abs() <= step * 0.5 + 1e-6, "d={d} back={back}");
        }
    }

    #[test]
    fn distance_encoding_saturates_outside_the_range() {
        let range = 0.1;
        assert_eq!(encode_distance(range, range), 255);
        assert_eq!(encode_distance(-range, range), 0);
        assert_eq!(encode_distance(f32::INFINITY, range), 255);
        assert_eq!(encode_distance(f32::NEG_INFINITY, range), 0);
        // Exactly on the outline must land on the midpoint, or every glyph is
        // half a step too fat or too thin.
        assert_eq!(encode_distance(0.0, range), 128);
    }

    #[test]
    fn degenerate_range_still_produces_a_definite_side() {
        assert_eq!(encode_distance(0.5, 0.0), 255);
        assert_eq!(encode_distance(-0.5, 0.0), 0);
    }

    // -- root solving --------------------------------------------------------

    fn sorted(r: Roots) -> Vec<f32> {
        let mut v: Vec<f32> = r.as_slice().to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    }

    #[test]
    fn cubic_solver_finds_three_distinct_roots() {
        // (x-1)(x-2)(x-3)
        let roots = sorted(solve_cubic(1.0, -6.0, 11.0, -6.0));
        assert_eq!(roots.len(), 3);
        for (got, want) in roots.iter().zip([1.0, 2.0, 3.0]) {
            assert!((got - want).abs() < 1e-4, "{roots:?}");
        }
    }

    #[test]
    fn cubic_solver_reports_both_branches_of_a_double_root() {
        // (x-1)^2 (x+2) = x^3 - 3x + 2
        let roots = sorted(solve_cubic(1.0, 0.0, -3.0, 2.0));
        assert!(roots.len() >= 2, "{roots:?}");
        assert!(roots.iter().any(|r| (r - 1.0).abs() < 1e-3), "{roots:?}");
        assert!(roots.iter().any(|r| (r + 2.0).abs() < 1e-3), "{roots:?}");
    }

    #[test]
    fn cubic_solver_finds_the_single_real_root() {
        // x^3 + x + 1 has one real root near -0.6823.
        let roots = sorted(solve_cubic(1.0, 0.0, 1.0, 1.0));
        assert_eq!(roots.len(), 1);
        assert!((roots[0] + 0.682_327_8).abs() < 1e-4, "{roots:?}");
    }

    #[test]
    fn cubic_solver_falls_back_through_quadratic_and_linear() {
        // x^2 - 4 -> +-2
        let roots = sorted(solve_cubic(0.0, 1.0, 0.0, -4.0));
        assert_eq!(roots.len(), 2);
        assert!((roots[0] + 2.0).abs() < 1e-5 && (roots[1] - 2.0).abs() < 1e-5);
        // 2x + 6 -> -3
        let roots = sorted(solve_cubic(0.0, 0.0, 2.0, 6.0));
        assert_eq!(roots, vec![-3.0]);
        // Everything zero: no actionable root.
        assert!(solve_cubic(0.0, 0.0, 0.0, 0.0).is_empty());
        // No real roots.
        assert!(solve_quadratic(1.0, 0.0, 1.0).is_empty());
        // Touching root.
        assert_eq!(sorted(solve_quadratic(1.0, -2.0, 1.0)), vec![1.0]);
    }

    // -- segment geometry ----------------------------------------------------

    #[test]
    fn point_to_line_distance_matches_hand_computation() {
        let seg = EdgeSegment::Linear { p0: p(0.0, 0.0), p1: p(1.0, 0.0) };
        // Perpendicular foot inside the segment.
        let (d, t) = seg.signed_distance(p(0.5, -0.5));
        assert!((d.distance.abs() - 0.5).abs() < 1e-6, "{d:?}");
        assert!((t - 0.5).abs() < 1e-6);
        // Beyond the end: the endpoint distance, and a parameter outside [0, 1]
        // so the caller knows to extend the tangent.
        let (d, t) = seg.signed_distance(p(2.0, 0.0));
        assert!((d.distance.abs() - 1.0).abs() < 1e-6);
        assert!(t > 1.0, "t={t}");
        // Before the start.
        let (d, t) = seg.signed_distance(p(-3.0, 0.0));
        assert!((d.distance.abs() - 3.0).abs() < 1e-6);
        assert!(t < 0.0, "t={t}");
    }

    #[test]
    fn line_distance_signs_are_opposite_on_the_two_sides() {
        let seg = EdgeSegment::Linear { p0: p(0.0, 0.0), p1: p(1.0, 0.0) };
        let above = seg.signed_distance(p(0.5, -0.25)).0.distance;
        let below = seg.signed_distance(p(0.5, 0.25)).0.distance;
        assert!(above * below < 0.0, "above={above} below={below}");
    }

    #[test]
    fn zero_length_line_does_not_produce_nan() {
        let seg = EdgeSegment::Linear { p0: p(1.0, 1.0), p1: p(1.0, 1.0) };
        let (d, t) = seg.signed_distance(p(1.0, 2.0));
        assert!(d.distance.is_finite() && t.is_finite());
        assert!((d.distance.abs() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn point_to_quadratic_distance_matches_hand_computation() {
        // p0=(-1,1), c=(0,-1), p1=(1,1) traces exactly y = x^2 on x in [-1, 1].
        let seg =
            EdgeSegment::Quadratic { p0: p(-1.0, 1.0), control: p(0.0, -1.0), p1: p(1.0, 1.0) };
        assert!((seg.point(0.5).x).abs() < 1e-6 && (seg.point(0.5).y).abs() < 1e-6);

        // Below the vertex: the closest point is the vertex itself.
        let (d, t) = seg.signed_distance(p(0.0, -1.0));
        assert!((d.distance.abs() - 1.0).abs() < 1e-4, "{d:?}");
        assert!((t - 0.5).abs() < 1e-4, "t={t}");

        // Above the vertex the distance derivative has a triple root at t=1/2;
        // this is the case a naive cubic solver gets wrong.
        let (d, _) = seg.signed_distance(p(0.0, 0.5));
        assert!((d.distance.abs() - 0.5).abs() < 1e-4, "{d:?}");

        // Far above: both endpoints beat the vertex, distance sqrt(2).
        let (d, _) = seg.signed_distance(p(0.0, 2.0));
        assert!((d.distance.abs() - 2.0f32.sqrt()).abs() < 1e-4, "{d:?}");
    }

    #[test]
    fn degenerate_quadratic_behaves_like_a_line() {
        // Control at the midpoint: the curve is the straight segment (0,0)-(2,0).
        let seg = EdgeSegment::Quadratic { p0: p(0.0, 0.0), control: p(1.0, 0.0), p1: p(2.0, 0.0) };
        let (d, _) = seg.signed_distance(p(1.0, -0.5));
        assert!((d.distance.abs() - 0.5).abs() < 1e-5, "{d:?}");
        let (d, _) = seg.signed_distance(p(3.0, 0.0));
        assert!((d.distance.abs() - 1.0).abs() < 1e-5, "{d:?}");
    }

    #[test]
    fn point_to_cubic_distance_matches_hand_computation() {
        // Evenly spaced collinear controls: the curve is the segment (0,0)-(3,0).
        let seg = EdgeSegment::Cubic {
            p0: p(0.0, 0.0),
            control0: p(1.0, 0.0),
            control1: p(2.0, 0.0),
            p1: p(3.0, 0.0),
        };
        let (d, _) = seg.signed_distance(p(1.5, -0.5));
        assert!((d.distance.abs() - 0.5).abs() < 1e-4, "{d:?}");

        // The standard quarter-circle approximation: every point on it is within
        // 3e-4 of unit distance from the centre, so the search must land there.
        let k = 0.552_285_f32;
        let arc = EdgeSegment::Cubic {
            p0: p(1.0, 0.0),
            control0: p(1.0, k),
            control1: p(k, 1.0),
            p1: p(0.0, 1.0),
        };
        for q in [p(0.0, 0.0), p(0.3, 0.2), p(2.0, 2.0)] {
            let (d, _) = arc.signed_distance(q);
            let expected = (1.0 - length(q)).abs();
            assert!((d.distance.abs() - expected).abs() < 2e-3, "q={q:?} d={d:?}");
        }
    }

    #[test]
    fn sub_segment_reproduces_the_original_curve() {
        let seg = EdgeSegment::Cubic {
            p0: p(0.0, 0.0),
            control0: p(0.0, 1.0),
            control1: p(1.0, 1.0),
            p1: p(1.0, 0.0),
        };
        let mid = seg.sub_segment(0.25, 0.75);
        for i in 0..=8 {
            let s = i as f32 / 8.0;
            let want = seg.point(0.25 + 0.5 * s);
            let got = mid.point(s);
            assert!(length(vsub(got, want)) < 1e-5, "s={s} {got:?} vs {want:?}");
        }
        let thirds = seg.split_in_thirds();
        assert!(length(vsub(thirds[0].start(), seg.start())) < 1e-6);
        assert!(length(vsub(thirds[2].end(), seg.end())) < 1e-6);
        assert!(length(vsub(thirds[0].end(), thirds[1].start())) < 1e-6);
    }

    #[test]
    fn bounds_use_curve_extrema_not_the_control_hull() {
        // The control point is at y = -1 but the curve never goes above y = 0.
        let seg =
            EdgeSegment::Quadratic { p0: p(-1.0, 1.0), control: p(0.0, -1.0), p1: p(1.0, 1.0) };
        let b = seg.bounds();
        assert!((b.origin.y - 0.0).abs() < 1e-5, "{b:?}");
        assert!((b.max_point().y - 1.0).abs() < 1e-5, "{b:?}");
        assert!((b.origin.x + 1.0).abs() < 1e-5 && (b.max_point().x - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cubic_bounds_are_tight() {
        let seg = EdgeSegment::Cubic {
            p0: p(0.0, 0.0),
            control0: p(0.0, 3.0),
            control1: p(1.0, 3.0),
            p1: p(1.0, 0.0),
        };
        let b = seg.bounds();
        // The curve peaks at 3/4 of the control height, not at 3.
        assert!((b.max_point().y - 2.25).abs() < 1e-4, "{b:?}");
        assert!(b.origin.y.abs() < 1e-6);
    }

    #[test]
    fn flattening_stays_within_tolerance() {
        let seg =
            EdgeSegment::Quadratic { p0: p(-1.0, 1.0), control: p(0.0, -1.0), p1: p(1.0, 1.0) };
        let mut pts = vec![seg.start()];
        seg.flatten_into(0.001, &mut pts);
        assert!(pts.len() > 4, "{} points", pts.len());
        // Every emitted point is on the curve y = x^2.
        for q in &pts {
            assert!((q.y - q.x * q.x).abs() < 1e-4, "{q:?}");
        }
        // And the chords track it closely.
        for pair in pts.windows(2) {
            let mid = lerp(pair[0], pair[1], 0.5);
            assert!((mid.y - mid.x * mid.x).abs() < 0.002, "{mid:?}");
        }
    }

    // -- edge coloring -------------------------------------------------------

    fn assert_valid_coloring(shape: &Shape) {
        for contour in &shape.contours {
            for e in &contour.edges {
                assert_ne!(e.color, EdgeColor::BLACK, "an edge was left with no channels");
            }
        }
    }

    #[test]
    fn adjacent_edges_across_a_corner_share_exactly_one_channel() {
        let mut shape = unit_square();
        shape.color_edges(DEFAULT_ANGLE_THRESHOLD);
        assert_valid_coloring(&shape);
        let edges = &shape.contours[0].edges;
        assert_eq!(edges.len(), 4);
        for i in 0..edges.len() {
            let a = edges[i].color;
            let b = edges[(i + 1) % edges.len()].color;
            assert_eq!(
                a.shared_channels(b),
                1,
                "edges {i} and {} share {} channels ({a:?} / {b:?})",
                (i + 1) % edges.len(),
                a.shared_channels(b)
            );
        }
    }

    #[test]
    fn a_fully_smooth_contour_gets_one_valid_color() {
        // Four cubic quarter-arcs: a circle, no corners anywhere.
        let k = 0.552_285_f32;
        let mut contour = Contour::new();
        let quads = [
            (p(1.0, 0.0), p(1.0, k), p(k, 1.0), p(0.0, 1.0)),
            (p(0.0, 1.0), p(-k, 1.0), p(-1.0, k), p(-1.0, 0.0)),
            (p(-1.0, 0.0), p(-1.0, -k), p(-k, -1.0), p(0.0, -1.0)),
            (p(0.0, -1.0), p(k, -1.0), p(1.0, -k), p(1.0, 0.0)),
        ];
        for (a, b, c, d) in quads {
            contour.push(EdgeSegment::Cubic { p0: a, control0: b, control1: c, p1: d });
        }
        let mut shape = Shape { contours: vec![contour] };
        shape.color_edges(DEFAULT_ANGLE_THRESHOLD);
        assert_valid_coloring(&shape);
        for e in &shape.contours[0].edges {
            assert_eq!(e.color, EdgeColor::WHITE, "a smooth contour needs no corner protection");
        }
    }

    #[test]
    fn a_teardrop_contour_is_split_so_three_colors_fit() {
        // A comma: one straight edge, one curve back to the start arriving
        // tangentially (control1 left of the origin, so the returning tangent is
        // +x, exactly the line's direction). That leaves a single corner, at
        // (2,0), and only two edges to carry three colors.
        let mut contour = Contour::new();
        contour.push(EdgeSegment::Linear { p0: p(0.0, 0.0), p1: p(2.0, 0.0) });
        contour.push(EdgeSegment::Cubic {
            p0: p(2.0, 0.0),
            control0: p(2.0, 1.6),
            control1: p(-0.5, 0.0),
            p1: p(0.0, 0.0),
        });
        let mut shape = Shape { contours: vec![contour] };
        shape.color_edges(DEFAULT_ANGLE_THRESHOLD);
        let edges = &shape.contours[0].edges;
        assert_eq!(edges.len(), 6, "a 2-edge teardrop must be split into thirds");
        assert_valid_coloring(&shape);

        // The split rotates the contour to start at the corner, so the corner is
        // the wrap-around join: the first and last edges flank it and must share
        // exactly one channel, exactly as at any other corner.
        let first = edges[0].color;
        let last = edges[edges.len() - 1].color;
        assert_ne!(first, last, "the corner is not protected");
        assert_eq!(first.shared_channels(last), 1, "{first:?} / {last:?}");
        // The smooth middle of the contour needs no protection and stays white.
        assert_eq!(edges[2].color, EdgeColor::WHITE);
        assert_eq!(edges[3].color, EdgeColor::WHITE);
    }

    #[test]
    fn a_single_edge_contour_is_split_into_three() {
        let mut contour = Contour::new();
        // A closed loop expressed as one cubic.
        contour.push(EdgeSegment::Cubic {
            p0: p(0.0, 0.0),
            control0: p(2.0, 2.0),
            control1: p(-2.0, 2.0),
            p1: p(0.0, 0.0),
        });
        let mut shape = Shape { contours: vec![contour] };
        shape.color_edges(DEFAULT_ANGLE_THRESHOLD);
        assert_eq!(shape.contours[0].edges.len(), 3);
        assert_valid_coloring(&shape);
    }

    #[test]
    fn switch_color_never_repeats_the_banned_color() {
        for seed in 0..12u64 {
            let mut s = seed;
            let mut color = EdgeColor::WHITE;
            switch_color(&mut color, &mut s, EdgeColor::BLACK);
            let first = color;
            for _ in 0..6 {
                let previous = color;
                switch_color(&mut color, &mut s, first);
                assert_ne!(color, previous);
                assert_eq!(previous.shared_channels(color), 1);
                assert_ne!(color, EdgeColor::BLACK);
                assert_ne!(color, EdgeColor::WHITE);
            }
        }
    }

    // -- shape plumbing ------------------------------------------------------

    #[test]
    fn polygon_orientation_is_normalised() {
        let mut cw = unit_square();
        let mut ccw = unit_square();
        ccw.reverse();
        // Whichever way they were wound, both end up with the same orientation.
        cw.orient(0.01);
        ccw.orient(0.01);
        let area_cw: f32 = cw.flatten(0.01).iter().map(|p| polygon_area(p)).sum();
        let area_ccw: f32 = ccw.flatten(0.01).iter().map(|p| polygon_area(p)).sum();
        assert!(area_cw <= 0.0 && area_ccw <= 0.0, "{area_cw} {area_ccw}");
        assert!((area_cw - area_ccw).abs() < 1e-5);
    }

    #[test]
    fn shape_bounds_cover_every_contour() {
        let shape = Shape {
            contours: vec![
                Shape::from_polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0)]).contours.remove(0),
                Shape::from_polygon(&[p(2.0, -1.0), p(3.0, -1.0), p(3.0, 0.5)]).contours.remove(0),
            ],
        };
        let b = shape.bounds().unwrap();
        assert!((b.origin.x).abs() < 1e-6 && (b.origin.y + 1.0).abs() < 1e-6, "{b:?}");
        assert!((b.max_point().x - 3.0).abs() < 1e-6 && (b.max_point().y - 1.0).abs() < 1e-6);
        assert!(Shape::new().bounds().is_none());
    }

    #[test]
    fn degenerate_polygons_do_not_become_contours() {
        assert!(Shape::from_polygon(&[]).is_empty());
        assert!(Shape::from_polygon(&[p(1.0, 1.0)]).is_empty());
        // Two coincident points collapse to nothing.
        assert!(Shape::from_polygon(&[p(1.0, 1.0), p(1.0, 1.0)]).is_empty());
    }

    // -- field generation ----------------------------------------------------

    /// Decoded `[r, g, b, a]` in em units at a texel.
    fn sample(image: &GlyphImage, x: u32, y: u32) -> [f32; 4] {
        let i = ((y * image.width + x) * 4) as usize;
        [
            decode_distance(image.data[i], image.range_em),
            decode_distance(image.data[i + 1], image.range_em),
            decode_distance(image.data[i + 2], image.range_em),
            decode_distance(image.data[i + 3], image.range_em),
        ]
    }

    /// Texel coordinate of an em-space point within an image.
    fn texel_of(image: &GlyphImage, config: &GlyphRasterConfig, q: Point<f32>) -> (u32, u32) {
        let padded = image.bounds_em.outset(Edges::all(config.range_em()));
        let fx = (q.x - padded.origin.x) / padded.size.width * image.width as f32;
        let fy = (q.y - padded.origin.y) / padded.size.height * image.height as f32;
        (
            (fx.floor().max(0.0) as u32).min(image.width - 1),
            (fy.floor().max(0.0) as u32).min(image.height - 1),
        )
    }

    #[test]
    fn empty_shapes_produce_an_empty_image() {
        let img = generate_mtsdf_from_shape(&Shape::new(), &GlyphRasterConfig::default());
        assert!(img.is_empty());
        assert_eq!(img.format, GlyphFormat::Mtsdf);
        assert!(img.data.is_empty());
    }

    #[test]
    fn image_extent_matches_the_padded_ink_box() {
        let config = GlyphRasterConfig::new(32.0, 4.0);
        let img = generate_mtsdf_from_shape(&unit_square(), &config);
        assert_eq!(img.format, GlyphFormat::Mtsdf);
        assert!((img.range_em - 4.0 / 32.0).abs() < 1e-6);
        // The ink box is reported untouched so the caller can place the quad.
        assert!((img.bounds_em.size.width - 1.0).abs() < 1e-6);
        let padded = 1.0 + 2.0 * img.range_em;
        assert_eq!(img.width, (padded * 32.0).ceil() as u32);
        assert_eq!(img.height, img.width);
        assert_eq!(img.data.len(), (img.width * img.height * 4) as usize);
    }

    #[test]
    fn the_median_separates_inside_from_outside() {
        let config = GlyphRasterConfig::new(32.0, 4.0);
        let img = generate_mtsdf_from_shape(&unit_square(), &config);

        let (cx, cy) = texel_of(&img, &config, p(0.5, 0.5));
        let c = sample(&img, cx, cy);
        assert!(median(c[0], c[1], c[2]) > 0.0, "centre must read as inside: {c:?}");
        assert!(c[3] > 0.0, "true distance must agree at the centre");

        // A texel in the padding, well outside the square.
        let outside = sample(&img, 0, 0);
        assert!(median(outside[0], outside[1], outside[2]) < 0.0, "{outside:?}");
        assert!(outside[3] < 0.0);
    }

    #[test]
    fn winding_direction_does_not_change_the_field() {
        let config = GlyphRasterConfig::new(24.0, 4.0);
        let mut reversed = unit_square();
        reversed.reverse();
        let a = generate_mtsdf_from_shape(&unit_square(), &config);
        let b = generate_mtsdf_from_shape(&reversed, &config);
        assert_eq!(a.width, b.width);
        // Orientation is re-derived, so a CW and a CCW square must agree texel
        // for texel; a font mixing the two conventions would otherwise render
        // inside-out.
        let differing = a.data.iter().zip(&b.data).filter(|(x, y)| x.abs_diff(**y) > 1).count();
        assert_eq!(differing, 0, "{differing} texels differ between windings");
    }

    #[test]
    fn a_right_angle_corner_stays_sharp() {
        // The corner at (0,0) of the unit square, sampled diagonally outside it.
        let config = GlyphRasterConfig { em_size_px: 64.0, range_px: 6.0, ..Default::default() };
        let img = generate_mtsdf_from_shape(&unit_square(), &config);

        let q = p(-0.03, -0.03);
        let (x, y) = texel_of(&img, &config, q);
        let t = sample(&img, x, y);
        let m = median(t[0], t[1], t[2]);
        let true_distance = t[3];

        // Both agree the texel is outside.
        assert!(m < 0.0 && true_distance < 0.0, "{t:?}");
        // A single-channel field can only store the true distance, which is the
        // distance to the corner *point* and therefore rounds the corner off at
        // every threshold other than zero. The median instead reconstructs the
        // intersection of the two edges' half-planes, which is strictly nearer
        // than the corner point.
        assert!(
            m > true_distance + 0.002,
            "median {m} should be closer to the outline than the true distance {true_distance}"
        );
        // That intersection distance is max(dx, dy) for an axis-aligned corner.
        let padded = img.bounds_em.outset(Edges::all(img.range_em));
        let sx = padded.size.width / img.width as f32;
        let sy = padded.size.height / img.height as f32;
        let centre =
            p(padded.origin.x + (x as f32 + 0.5) * sx, padded.origin.y + (y as f32 + 0.5) * sy);
        let expected = -(-centre.x).max(-centre.y);
        assert!((m - expected).abs() < 0.01, "median {m}, expected about {expected}");
    }

    #[test]
    fn a_hole_reads_as_outside() {
        // Outer square wound one way, inner square the other: nonzero winding
        // makes the inner region a counter.
        let mut outer = Shape::from_polygon(&[p(0.0, 0.0), p(0.0, 1.0), p(1.0, 1.0), p(1.0, 0.0)]);
        let mut inner = Shape::from_polygon(&[p(0.3, 0.3), p(0.7, 0.3), p(0.7, 0.7), p(0.3, 0.7)]);
        outer.contours.append(&mut inner.contours);

        let config = GlyphRasterConfig::new(48.0, 4.0);
        let img = generate_mtsdf_from_shape(&outer, &config);
        let (x, y) = texel_of(&img, &config, p(0.5, 0.5));
        let t = sample(&img, x, y);
        assert!(t[3] < 0.0, "the counter must read as outside: {t:?}");
        assert!(median(t[0], t[1], t[2]) < 0.0, "{t:?}");

        // ...while the ring between the two squares is inside.
        let (x, y) = texel_of(&img, &config, p(0.15, 0.5));
        let t = sample(&img, x, y);
        assert!(t[3] > 0.0, "the ring must read as inside: {t:?}");
    }

    #[test]
    fn the_median_never_contradicts_the_true_distance_after_correction() {
        // An L shape has a reflex corner, which is where median/true conflicts
        // actually appear.
        let shape = Shape::from_polygon(&[
            p(0.0, 0.0),
            p(0.0, 1.0),
            p(0.4, 1.0),
            p(0.4, 0.4),
            p(1.0, 0.4),
            p(1.0, 0.0),
        ]);
        let config = GlyphRasterConfig::new(48.0, 4.0);
        let img = generate_mtsdf_from_shape(&shape, &config);
        let mut conflicts = 0;
        for y in 0..img.height {
            for x in 0..img.width {
                let t = sample(&img, x, y);
                let m = median(t[0], t[1], t[2]);
                // Ignore the quantisation band around zero, where a sign flip is
                // below one texel value.
                let step = img.range_em / 255.0;
                if m.abs() > step && t[3].abs() > step && (m > 0.0) != (t[3] > 0.0) {
                    conflicts += 1;
                }
            }
        }
        assert_eq!(conflicts, 0, "{conflicts} texels disagree with the true distance");
    }

    #[test]
    fn error_correction_clamps_a_wrong_sided_median_and_leaves_the_rest() {
        // Texel 0: the median says inside, the true distance says outside. This
        // is the artifact: it renders as a solid dot in empty space.
        // Texel 1: same sign, different magnitude — the corner sharpening the
        // multi-channel encoding exists to provide, which must survive.
        // Texel 2: wrong side but by less than one quantisation step, so
        // repairing it would not change a single output byte.
        let barely = 0.5 + 0.5 / 255.0;
        let mut field = [[0.9, 0.9, 0.2, 0.1], [0.7, 0.9, 0.8, 0.95], [0.1, barely, 0.9, 0.49]];
        correct_errors(&mut field);
        assert_eq!(field[0], [0.1, 0.1, 0.1, 0.1], "the artifact was not clamped");
        assert_eq!(field[1], [0.7, 0.9, 0.8, 0.95], "corner sharpening was flattened");
        assert_eq!(field[2], [0.1, barely, 0.9, 0.49], "sub-quantum churn");
    }

    #[test]
    fn error_correction_never_touches_the_true_distance() {
        let shape = Shape::from_polygon(&[
            p(0.0, 0.0),
            p(0.0, 1.0),
            p(0.4, 1.0),
            p(0.4, 0.4),
            p(1.0, 0.4),
            p(1.0, 0.0),
        ]);
        let on = GlyphRasterConfig::new(48.0, 4.0);
        let off = GlyphRasterConfig { error_correction: false, ..on };
        let a = generate_mtsdf_from_shape(&shape, &on);
        let b = generate_mtsdf_from_shape(&shape, &off);
        assert_eq!(a.data.len(), b.data.len());
        for i in (0..a.data.len()).step_by(4) {
            assert_eq!(a.data[i + 3], b.data[i + 3], "alpha must be left alone");
            if a.data[i..i + 3] != b.data[i..i + 3] {
                // A repaired texel is set to the true distance exactly.
                assert_eq!(a.data[i], a.data[i + 3]);
                assert_eq!(a.data[i + 1], a.data[i + 3]);
                assert_eq!(a.data[i + 2], a.data[i + 3]);
            }
        }
    }

    #[test]
    fn a_hairline_outline_still_produces_a_field() {
        // A zero-height ink box: only the padding gives the image any rows.
        let mut contour = Contour::new();
        contour.push(EdgeSegment::Linear { p0: p(0.0, 0.0), p1: p(1.0, 0.0) });
        contour.push(EdgeSegment::Linear { p0: p(1.0, 0.0), p1: p(0.0, 0.0) });
        let shape = Shape { contours: vec![contour] };
        let config = GlyphRasterConfig::new(32.0, 4.0);
        let img = generate_mtsdf_from_shape(&shape, &config);
        assert!(!img.is_empty());
        assert_eq!(img.height, (2.0 * img.range_em * 32.0).ceil() as u32);
        assert_eq!(img.data.len(), (img.width * img.height * 4) as usize);

        // A zero-area outline encloses nothing. The winding rule says so, and
        // every texel further from the line than the sign guard band is forced
        // to agree with it; only texels inside that band keep the (here
        // meaningless) analytic sign, and they are bounded by construction.
        let guard = (2.0 * (0.25 / 32.0) / img.range_em * 255.0).ceil() as u8;
        for texel in img.data.chunks_exact(4) {
            assert!(
                texel[3] <= 128 + guard,
                "a degenerate outline filled a texel: {} > {}",
                texel[3],
                128 + guard
            );
        }
    }

    #[test]
    fn enormous_outlines_are_capped() {
        let shape =
            Shape::from_polygon(&[p(0.0, 0.0), p(0.0, 5000.0), p(5000.0, 5000.0), p(5000.0, 0.0)]);
        let config = GlyphRasterConfig { max_texels: 64, ..GlyphRasterConfig::new(48.0, 4.0) };
        let img = generate_mtsdf_from_shape(&shape, &config);
        assert_eq!(img.width, 64);
        assert_eq!(img.height, 64);
    }

    #[test]
    fn a_zero_texel_cap_is_treated_as_one_rather_than_panicking() {
        // `max_texels` is a public field, so nothing stops a caller writing 0.
        // `clamp(1, 0)` panics, which would turn a configuration mistake into a
        // crash inside glyph rasterisation.
        let shape = unit_square();
        let config = GlyphRasterConfig { max_texels: 0, ..GlyphRasterConfig::new(32.0, 4.0) };
        let img = generate_mtsdf_from_shape(&shape, &config);
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.data.len(), 4);
    }

    // -- integration with a real face ---------------------------------------

    /// Loads a system font if one is present. Never fails the suite: CI images
    /// and developer machines do not agree on which fonts exist.
    fn system_font() -> Option<Vec<u8>> {
        for path in [
            "C:/Windows/Fonts/segoeui.ttf",
            "C:/Windows/Fonts/arial.ttf",
            "C:/Windows/Fonts/tahoma.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
        ] {
            if let Ok(data) = std::fs::read(path) {
                return Some(data);
            }
        }
        None
    }

    #[test]
    fn a_real_glyph_generates_a_plausible_field() {
        let Some(data) = system_font() else {
            eprintln!("skipping: no system font found");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).expect("system font should parse");
        let gid = face.glyph_index('B').expect("a text font has a 'B'");
        let glyph = GlyphId(gid.0);

        let metrics = glyph_metrics(&face, glyph).unwrap();
        assert!(metrics.advance > 0.0);
        assert!(metrics.bounds.size.height > 0.2, "{:?}", metrics.bounds);
        // y-down: the cap sits above the baseline, so min_y is negative.
        assert!(metrics.bounds.origin.y < 0.0, "{:?}", metrics.bounds);

        let config = GlyphRasterConfig::new(32.0, 4.0);
        let img = generate_mtsdf(&face, glyph, &config).unwrap();
        assert!(!img.is_empty());
        assert_eq!(img.data.len(), (img.width * img.height * 4) as usize);
        // The ink box is derived from the curves themselves, so it can only be
        // tighter than the one the font declares, never wider.
        assert!(img.bounds_em.size.width > 0.1, "{:?}", img.bounds_em);
        assert!(
            img.bounds_em.size.width <= metrics.bounds.size.width + 1e-3,
            "outline bounds {:?} exceed the declared {:?}",
            img.bounds_em,
            metrics.bounds
        );

        // The stem of a 'B' is solid: a texel a little inside its left edge must
        // read as inside on both the median and the true distance.
        let q = p(
            metrics.bounds.origin.x + 0.02,
            metrics.bounds.origin.y + metrics.bounds.size.height * 0.5,
        );
        let (x, y) = texel_of(&img, &config, q);
        let t = sample(&img, x, y);
        assert!(t[3] > 0.0, "expected inside the stem: {t:?}");
        assert!(median(t[0], t[1], t[2]) > 0.0, "{t:?}");
    }

    #[test]
    fn real_glyphs_of_every_shape_class_produce_a_consistent_field() {
        let Some(data) = system_font() else {
            eprintln!("skipping: no system font found");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        let config = GlyphRasterConfig::new(32.0, 4.0);
        // A counter ('O'), a comma (the teardrop case), a many-cornered 'W', a
        // multi-contour 'i', a diagonal 'x' and a curve-heavy 'e'.
        for ch in ['O', ',', 'W', 'i', 'x', 'e', '8', '@'] {
            let Some(gid) = face.glyph_index(ch) else { continue };
            let img = generate_mtsdf(&face, GlyphId(gid.0), &config).unwrap();
            assert!(!img.is_empty(), "'{ch}' produced no field");
            assert_eq!(img.data.len(), (img.width * img.height * 4) as usize);

            let step = img.range_em / 255.0;
            let mut conflicts = 0;
            for y in 0..img.height {
                for x in 0..img.width {
                    let t = sample(&img, x, y);
                    let m = median(t[0], t[1], t[2]);
                    if m.abs() > step && t[3].abs() > step && (m > 0.0) != (t[3] > 0.0) {
                        conflicts += 1;
                    }
                }
            }
            assert_eq!(conflicts, 0, "'{ch}' has {conflicts} texels the median gets wrong");

            // The border of the padded field is entirely outside the glyph: if it
            // were not, the range would be too small for the padding to contain
            // the outline and the glyph would be clipped in the atlas.
            for x in 0..img.width {
                assert!(sample(&img, x, 0)[3] < 0.0, "'{ch}' touches the top padding");
                assert!(
                    sample(&img, x, img.height - 1)[3] < 0.0,
                    "'{ch}' touches the bottom padding"
                );
            }
        }
    }

    #[test]
    fn a_space_glyph_has_no_field_but_is_not_an_error() {
        let Some(data) = system_font() else {
            eprintln!("skipping: no system font found");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        let gid = face.glyph_index(' ').expect("a text font has a space");
        let img = generate_mtsdf(&face, GlyphId(gid.0), &GlyphRasterConfig::default()).unwrap();
        assert!(img.is_empty());
        let m = glyph_metrics(&face, GlyphId(gid.0)).unwrap();
        assert!(m.advance > 0.0, "a space still advances the pen");
    }

    #[test]
    fn an_out_of_range_glyph_is_an_error() {
        let Some(data) = system_font() else {
            eprintln!("skipping: no system font found");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        let bogus = GlyphId(face.number_of_glyphs());
        assert!(matches!(
            generate_mtsdf(&face, bogus, &GlyphRasterConfig::default()),
            Err(FontError::MissingGlyph(_))
        ));
        assert!(matches!(glyph_metrics(&face, bogus), Err(FontError::MissingGlyph(_))));
    }

    #[test]
    fn face_metrics_are_self_consistent() {
        let Some(data) = system_font() else {
            eprintln!("skipping: no system font found");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        let m = face_metrics(&face);
        assert!(m.units_per_em >= 16);
        assert!(m.ascent > 0.0 && m.descent > 0.0, "{m:?}");
        assert!(m.line_height() >= m.ascent + m.descent);
        assert!(m.x_height > 0.0 && m.cap_height >= m.x_height, "{m:?}");
        assert!(m.underline_thickness > 0.0);
    }
}
