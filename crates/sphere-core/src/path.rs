//! Paths: the outline representation shared by the canvas, SVG and text layers.
//!
//! Paths are stored as a flat verb list plus a flat point list rather than an
//! enum-per-segment vector. That keeps a path contiguous in memory, makes
//! `Clone` a pair of memcpys, and lets the tessellator walk it without chasing
//! per-element padding.

use crate::geometry::{Point, Rect, Size};
use crate::transform::Affine;
use crate::unit::Px;
use smallvec::SmallVec;

/// A single path command.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Verb {
    /// Starts a new subpath at one point.
    MoveTo,
    /// Straight segment to one point.
    LineTo,
    /// Quadratic bezier: one control point, then the endpoint.
    QuadTo,
    /// Cubic bezier: two control points, then the endpoint.
    CubicTo,
    /// Closes the current subpath back to its start.
    Close,
}

impl Verb {
    /// How many points this verb consumes from the point list.
    #[inline]
    pub const fn point_count(self) -> usize {
        match self {
            Verb::MoveTo | Verb::LineTo => 1,
            Verb::QuadTo => 2,
            Verb::CubicTo => 3,
            Verb::Close => 0,
        }
    }
}

/// How the interior of a path is determined.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Hash)]
pub enum FillRule {
    /// A point is inside when the winding number is nonzero. The default, and
    /// what fonts and most vector art assume.
    #[default]
    NonZero,
    /// A point is inside when the crossing count is odd.
    EvenOdd,
}

/// An expanded path segment, produced when iterating a [`Path`].
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum PathEvent {
    /// Begin a subpath.
    MoveTo(Point<Px>),
    /// Straight segment from the current point.
    LineTo(Point<Px>),
    /// Quadratic bezier from the current point.
    QuadTo(Point<Px>, Point<Px>),
    /// Cubic bezier from the current point.
    CubicTo(Point<Px>, Point<Px>, Point<Px>),
    /// Close the current subpath.
    Close,
}

/// A 2D outline made of one or more subpaths.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Path {
    verbs: SmallVec<[Verb; 8]>,
    points: SmallVec<[Point<Px>; 8]>,
    fill_rule: FillRule,
}

impl Path {
    /// An empty path.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts building a path.
    #[inline]
    pub fn builder() -> PathBuilder {
        PathBuilder::new()
    }

    /// True when the path contains no commands.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    /// Number of commands.
    #[inline]
    pub fn verb_count(&self) -> usize {
        self.verbs.len()
    }

    /// The fill rule to use when filling this path.
    #[inline]
    pub fn fill_rule(&self) -> FillRule {
        self.fill_rule
    }

    /// Sets the fill rule.
    #[inline]
    pub fn set_fill_rule(&mut self, rule: FillRule) {
        self.fill_rule = rule;
    }

    /// Iterates the path as expanded [`PathEvent`]s.
    pub fn iter(&self) -> impl Iterator<Item = PathEvent> + '_ {
        let mut i = 0usize;
        self.verbs.iter().map(move |&v| {
            let p = &self.points[i..];
            i += v.point_count();
            match v {
                Verb::MoveTo => PathEvent::MoveTo(p[0]),
                Verb::LineTo => PathEvent::LineTo(p[0]),
                Verb::QuadTo => PathEvent::QuadTo(p[0], p[1]),
                Verb::CubicTo => PathEvent::CubicTo(p[0], p[1], p[2]),
                Verb::Close => PathEvent::Close,
            }
        })
    }

    /// The bounding box of the path's control points.
    ///
    /// This is a conservative bound: a bezier never leaves the convex hull of
    /// its control points, so the box always encloses the true curve. It is
    /// what culling wants, since it is cheap and never under-covers.
    pub fn control_bounds(&self) -> Rect<Px> {
        if self.points.is_empty() {
            return Rect::ZERO;
        }
        let mut min = self.points[0];
        let mut max = self.points[0];
        for p in &self.points[1..] {
            min = min.min(*p);
            max = max.max(*p);
        }
        Rect::from_corners(min, max)
    }

    /// Applies a transform, producing a new path.
    pub fn transformed(&self, t: Affine) -> Path {
        Path {
            verbs: self.verbs.clone(),
            points: self.points.iter().map(|&p| t.apply(p)).collect(),
            fill_rule: self.fill_rule,
        }
    }

    /// True when `p` is inside the path under the current fill rule.
    ///
    /// Uses a crossing test against flattened segments; the tolerance controls
    /// how finely curves are subdivided.
    pub fn contains(&self, p: Point<Px>, tolerance: Px) -> bool {
        let mut winding = 0i32;
        let mut crossings = 0u32;
        self.flatten(tolerance, |a, b| {
            let (ax, ay) = (a.x.get(), a.y.get());
            let (bx, by) = (b.x.get(), b.y.get());
            let (px_, py) = (p.x.get(), p.y.get());
            if (ay <= py && by > py) || (by <= py && ay > py) {
                let t = (py - ay) / (by - ay);
                if px_ < ax + t * (bx - ax) {
                    crossings += 1;
                    winding += if by > ay { 1 } else { -1 };
                }
            }
        });
        match self.fill_rule {
            FillRule::NonZero => winding != 0,
            FillRule::EvenOdd => crossings % 2 == 1,
        }
    }

    /// Flattens the path into line segments, invoking `emit(from, to)` for each.
    ///
    /// Subpaths are implicitly closed for the purposes of flattening, which is
    /// what fill and containment tests need. Stroking handles open subpaths
    /// separately.
    pub fn flatten(&self, tolerance: Px, mut emit: impl FnMut(Point<Px>, Point<Px>)) {
        let tol = tolerance.get().max(1e-3);
        let mut cursor = Point::ZERO;
        let mut start = Point::ZERO;
        let mut open = false;

        for ev in self.iter() {
            match ev {
                PathEvent::MoveTo(p) => {
                    if open && cursor != start {
                        emit(cursor, start);
                    }
                    cursor = p;
                    start = p;
                    open = true;
                }
                PathEvent::LineTo(p) => {
                    emit(cursor, p);
                    cursor = p;
                }
                PathEvent::QuadTo(c, p) => {
                    let n = quad_subdivisions(cursor, c, p, tol);
                    let mut prev = cursor;
                    for i in 1..=n {
                        let t = i as f32 / n as f32;
                        let q = eval_quad(cursor, c, p, t);
                        emit(prev, q);
                        prev = q;
                    }
                    cursor = p;
                }
                PathEvent::CubicTo(c1, c2, p) => {
                    let n = cubic_subdivisions(cursor, c1, c2, p, tol);
                    let mut prev = cursor;
                    for i in 1..=n {
                        let t = i as f32 / n as f32;
                        let q = eval_cubic(cursor, c1, c2, p, t);
                        emit(prev, q);
                        prev = q;
                    }
                    cursor = p;
                }
                PathEvent::Close => {
                    if cursor != start {
                        emit(cursor, start);
                    }
                    cursor = start;
                    open = false;
                }
            }
        }
        if open && cursor != start {
            emit(cursor, start);
        }
    }

    /// Total length of the flattened outline, used to place dashes and to
    /// distribute automation-curve samples.
    pub fn length(&self, tolerance: Px) -> Px {
        let mut total = 0.0f32;
        self.flatten(tolerance, |a, b| total += a.distance_to(b).get());
        Px(total)
    }

    /// The raw verb list, for tessellator adapters.
    #[inline]
    pub fn verbs(&self) -> &[Verb] {
        &self.verbs
    }

    /// The raw point list, for tessellator adapters.
    #[inline]
    pub fn points(&self) -> &[Point<Px>] {
        &self.points
    }
}

/// Incrementally builds a [`Path`].
#[derive(Clone, Default, Debug)]
pub struct PathBuilder {
    verbs: SmallVec<[Verb; 8]>,
    points: SmallVec<[Point<Px>; 8]>,
    fill_rule: FillRule,
    cursor: Point<Px>,
    subpath_start: Point<Px>,
    has_current: bool,
}

impl PathBuilder {
    /// A new, empty builder.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the fill rule for the resulting path.
    #[inline]
    pub fn fill_rule(mut self, rule: FillRule) -> Self {
        self.fill_rule = rule;
        self
    }

    /// Begins a new subpath.
    pub fn move_to(&mut self, p: Point<Px>) -> &mut Self {
        self.verbs.push(Verb::MoveTo);
        self.points.push(p);
        self.cursor = p;
        self.subpath_start = p;
        self.has_current = true;
        self
    }

    /// Adds a straight segment.
    ///
    /// A `line_to` without a preceding `move_to` implicitly starts a subpath at
    /// the origin, matching SVG's behaviour, rather than panicking.
    pub fn line_to(&mut self, p: Point<Px>) -> &mut Self {
        if !self.has_current {
            self.move_to(Point::ZERO);
        }
        self.verbs.push(Verb::LineTo);
        self.points.push(p);
        self.cursor = p;
        self
    }

    /// Adds a quadratic bezier.
    pub fn quad_to(&mut self, c: Point<Px>, p: Point<Px>) -> &mut Self {
        if !self.has_current {
            self.move_to(Point::ZERO);
        }
        self.verbs.push(Verb::QuadTo);
        self.points.push(c);
        self.points.push(p);
        self.cursor = p;
        self
    }

    /// Adds a cubic bezier.
    pub fn cubic_to(&mut self, c1: Point<Px>, c2: Point<Px>, p: Point<Px>) -> &mut Self {
        if !self.has_current {
            self.move_to(Point::ZERO);
        }
        self.verbs.push(Verb::CubicTo);
        self.points.push(c1);
        self.points.push(c2);
        self.points.push(p);
        self.cursor = p;
        self
    }

    /// Closes the current subpath.
    pub fn close(&mut self) -> &mut Self {
        if self.has_current {
            self.verbs.push(Verb::Close);
            self.cursor = self.subpath_start;
        }
        self
    }

    /// Appends a rectangle as a closed subpath, wound clockwise.
    pub fn rect(&mut self, r: Rect<Px>) -> &mut Self {
        self.move_to(r.origin);
        self.line_to(Point::new(r.max_x(), r.min_y()));
        self.line_to(r.max_point());
        self.line_to(Point::new(r.min_x(), r.max_y()));
        self.close()
    }

    /// Appends an ellipse inscribed in `r`, as four cubic arcs.
    pub fn ellipse(&mut self, r: Rect<Px>) -> &mut Self {
        let c = r.center();
        let rx = r.width().get() * 0.5;
        let ry = r.height().get() * 0.5;
        // Magic constant for approximating a quarter circle with one cubic.
        const K: f32 = 0.552_284_75;
        let (kx, ky) = (rx * K, ry * K);
        let (cx, cy) = (c.x.get(), c.y.get());
        let p = |x: f32, y: f32| Point::new(Px(x), Px(y));

        self.move_to(p(cx, cy - ry));
        self.cubic_to(p(cx + kx, cy - ry), p(cx + rx, cy - ky), p(cx + rx, cy));
        self.cubic_to(p(cx + rx, cy + ky), p(cx + kx, cy + ry), p(cx, cy + ry));
        self.cubic_to(p(cx - kx, cy + ry), p(cx - rx, cy + ky), p(cx - rx, cy));
        self.cubic_to(p(cx - rx, cy - ky), p(cx - kx, cy - ry), p(cx, cy - ry));
        self.close()
    }

    /// Appends a circle.
    pub fn circle(&mut self, center: Point<Px>, radius: Px) -> &mut Self {
        let r = radius.get();
        self.ellipse(Rect::new(
            Point::new(Px(center.x.get() - r), Px(center.y.get() - r)),
            Size::new(Px(r * 2.0), Px(r * 2.0)),
        ))
    }

    /// Appends a rounded rectangle.
    pub fn rounded_rect(&mut self, rr: crate::geometry::RoundedRect) -> &mut Self {
        let r = rr.rect;
        let c = rr.clamped_radii();
        if c.is_zero() {
            return self.rect(r);
        }
        const K: f32 = 0.552_284_75;
        let (x0, y0) = (r.min_x().get(), r.min_y().get());
        let (x1, y1) = (r.max_x().get(), r.max_y().get());
        let (tl, tr, br, bl) =
            (c.top_left.get(), c.top_right.get(), c.bottom_right.get(), c.bottom_left.get());
        let p = |x: f32, y: f32| Point::new(Px(x), Px(y));

        self.move_to(p(x0 + tl, y0));
        self.line_to(p(x1 - tr, y0));
        if tr > 0.0 {
            self.cubic_to(p(x1 - tr + tr * K, y0), p(x1, y0 + tr - tr * K), p(x1, y0 + tr));
        }
        self.line_to(p(x1, y1 - br));
        if br > 0.0 {
            self.cubic_to(p(x1, y1 - br + br * K), p(x1 - br + br * K, y1), p(x1 - br, y1));
        }
        self.line_to(p(x0 + bl, y1));
        if bl > 0.0 {
            self.cubic_to(p(x0 + bl - bl * K, y1), p(x0, y1 - bl + bl * K), p(x0, y1 - bl));
        }
        self.line_to(p(x0, y0 + tl));
        if tl > 0.0 {
            self.cubic_to(p(x0, y0 + tl - tl * K), p(x0 + tl - tl * K, y0), p(x0 + tl, y0));
        }
        self.close()
    }

    /// Appends a polyline, optionally closing it.
    pub fn polyline(&mut self, pts: &[Point<Px>], closed: bool) -> &mut Self {
        let Some((first, rest)) = pts.split_first() else { return self };
        self.move_to(*first);
        for p in rest {
            self.line_to(*p);
        }
        if closed {
            self.close();
        }
        self
    }

    /// The current pen position.
    #[inline]
    pub fn current_point(&self) -> Point<Px> {
        self.cursor
    }

    /// Finishes and yields the path.
    pub fn build(self) -> Path {
        Path { verbs: self.verbs, points: self.points, fill_rule: self.fill_rule }
    }
}

#[inline]
fn eval_quad(a: Point<Px>, c: Point<Px>, b: Point<Px>, t: f32) -> Point<Px> {
    let mt = 1.0 - t;
    let w = [mt * mt, 2.0 * mt * t, t * t];
    Point::new(
        Px(w[0] * a.x.get() + w[1] * c.x.get() + w[2] * b.x.get()),
        Px(w[0] * a.y.get() + w[1] * c.y.get() + w[2] * b.y.get()),
    )
}

#[inline]
fn eval_cubic(a: Point<Px>, c1: Point<Px>, c2: Point<Px>, b: Point<Px>, t: f32) -> Point<Px> {
    let mt = 1.0 - t;
    let w = [mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t];
    Point::new(
        Px(w[0] * a.x.get() + w[1] * c1.x.get() + w[2] * c2.x.get() + w[3] * b.x.get()),
        Px(w[0] * a.y.get() + w[1] * c1.y.get() + w[2] * c2.y.get() + w[3] * b.y.get()),
    )
}

/// Subdivision count for a quadratic, from the classic flatness bound.
///
/// The maximum deviation of a quadratic from its chord is bounded by
/// `|a - 2c + b| / 8`, and halving `t` divides the error by four, so `n` scales
/// with the square root of the deviation over tolerance.
fn quad_subdivisions(a: Point<Px>, c: Point<Px>, b: Point<Px>, tol: f32) -> u32 {
    let dx = a.x.get() - 2.0 * c.x.get() + b.x.get();
    let dy = a.y.get() - 2.0 * c.y.get() + b.y.get();
    let dev = (dx * dx + dy * dy).sqrt() * 0.25;
    if !dev.is_finite() || dev <= tol {
        return 1;
    }
    ((dev / tol).sqrt().ceil() as u32).clamp(1, 256)
}

/// Subdivision count for a cubic, using the larger of the two control-point
/// deviations.
fn cubic_subdivisions(a: Point<Px>, c1: Point<Px>, c2: Point<Px>, b: Point<Px>, tol: f32) -> u32 {
    let d1x = a.x.get() - 2.0 * c1.x.get() + c2.x.get();
    let d1y = a.y.get() - 2.0 * c1.y.get() + c2.y.get();
    let d2x = c1.x.get() - 2.0 * c2.x.get() + b.x.get();
    let d2y = c1.y.get() - 2.0 * c2.y.get() + b.y.get();
    let dev = ((d1x * d1x + d1y * d1y).max(d2x * d2x + d2y * d2y)).sqrt() * 0.75;
    if !dev.is_finite() || dev <= tol {
        return 1;
    }
    ((dev / tol).sqrt().ceil() as u32).clamp(1, 256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{RoundedRect, point, rect};
    use crate::unit::px;

    #[test]
    fn builder_records_verbs_and_points_in_step() {
        let mut b = PathBuilder::new();
        b.move_to(point(px(0.0), px(0.0)));
        b.line_to(point(px(10.0), px(0.0)));
        b.quad_to(point(px(10.0), px(5.0)), point(px(5.0), px(10.0)));
        b.cubic_to(point(px(0.0), px(10.0)), point(px(0.0), px(5.0)), point(px(0.0), px(0.0)));
        b.close();
        let p = b.build();
        assert_eq!(p.verb_count(), 5);
        assert_eq!(p.points().len(), 1 + 1 + 2 + 3);
        assert_eq!(p.iter().count(), 5);
    }

    #[test]
    fn line_to_without_move_to_starts_at_origin() {
        let mut b = PathBuilder::new();
        b.line_to(point(px(5.0), px(5.0)));
        let p = b.build();
        assert!(matches!(p.iter().next(), Some(PathEvent::MoveTo(_))));
    }

    #[test]
    fn rect_path_contains_its_own_centre() {
        let mut b = PathBuilder::new();
        b.rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)));
        let p = b.build();
        assert!(p.contains(point(px(5.0), px(5.0)), px(0.1)));
        assert!(!p.contains(point(px(-1.0), px(5.0)), px(0.1)));
        assert!(!p.contains(point(px(20.0), px(5.0)), px(0.1)));
    }

    #[test]
    fn circle_flattens_to_roughly_the_right_circumference() {
        let mut b = PathBuilder::new();
        b.circle(point(px(0.0), px(0.0)), px(100.0));
        let len = b.build().length(px(0.01)).get();
        let expected = core::f32::consts::TAU * 100.0;
        assert!((len - expected).abs() / expected < 0.005, "{len} vs {expected}");
    }

    #[test]
    fn circle_containment() {
        let mut b = PathBuilder::new();
        b.circle(point(px(50.0), px(50.0)), px(20.0));
        let p = b.build();
        assert!(p.contains(point(px(50.0), px(50.0)), px(0.05)));
        assert!(p.contains(point(px(65.0), px(50.0)), px(0.05)));
        assert!(!p.contains(point(px(75.0), px(50.0)), px(0.05)));
    }

    #[test]
    fn even_odd_and_nonzero_differ_on_a_nested_ring() {
        // Two concentric circles wound the same way: NonZero fills the middle,
        // EvenOdd punches it out.
        let build = |rule| {
            let mut b = PathBuilder::new().fill_rule(rule);
            b.circle(point(px(0.0), px(0.0)), px(50.0));
            b.circle(point(px(0.0), px(0.0)), px(25.0));
            b.build()
        };
        let inner = point(px(0.0), px(0.0));
        assert!(build(FillRule::NonZero).contains(inner, px(0.05)));
        assert!(!build(FillRule::EvenOdd).contains(inner, px(0.05)));
    }

    #[test]
    fn control_bounds_enclose_the_flattened_curve() {
        let mut b = PathBuilder::new();
        b.move_to(point(px(0.0), px(0.0)));
        b.cubic_to(point(px(0.0), px(100.0)), point(px(100.0), px(100.0)), point(px(100.0), px(0.0)));
        let p = b.build();
        let bounds = p.control_bounds();
        p.flatten(px(0.05), |a, _| {
            assert!(a.x >= bounds.min_x() && a.x <= bounds.max_x());
            assert!(a.y >= bounds.min_y() && a.y <= bounds.max_y());
        });
    }

    #[test]
    fn tighter_tolerance_produces_more_segments() {
        let mut b = PathBuilder::new();
        b.circle(point(px(0.0), px(0.0)), px(100.0));
        let p = b.build();
        let count = |tol: f32| {
            let mut n = 0;
            p.flatten(px(tol), |_, _| n += 1);
            n
        };
        assert!(count(0.01) > count(1.0));
    }

    #[test]
    fn transform_moves_every_point() {
        let mut b = PathBuilder::new();
        b.rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)));
        let p = b.build().transformed(Affine::translate(Size::new(px(5.0), px(5.0))));
        assert_eq!(p.control_bounds().origin, point(px(5.0), px(5.0)));
    }

    #[test]
    fn rounded_rect_with_zero_radii_degrades_to_a_plain_rect() {
        let mut b = PathBuilder::new();
        b.rounded_rect(RoundedRect::uniform(rect(px(0.0), px(0.0), px(10.0), px(10.0)), px(0.0)));
        let p = b.build();
        assert_eq!(p.verb_count(), 5, "move + 3 lines + close");
    }

    #[test]
    fn rounded_rect_stays_inside_its_box() {
        let r = rect(px(0.0), px(0.0), px(40.0), px(20.0));
        let mut b = PathBuilder::new();
        b.rounded_rect(RoundedRect::uniform(r, px(8.0)));
        let p = b.build();
        let bounds = p.control_bounds();
        assert!(bounds.min_x() >= r.min_x() - px(0.001));
        assert!(bounds.max_x() <= r.max_x() + px(0.001));
        assert!(bounds.min_y() >= r.min_y() - px(0.001));
        assert!(bounds.max_y() <= r.max_y() + px(0.001));
    }

    #[test]
    fn unclosed_subpath_is_implicitly_closed_for_fill() {
        let mut b = PathBuilder::new();
        b.move_to(point(px(0.0), px(0.0)));
        b.line_to(point(px(10.0), px(0.0)));
        b.line_to(point(px(10.0), px(10.0)));
        b.line_to(point(px(0.0), px(10.0)));
        // deliberately not closed
        let p = b.build();
        assert!(p.contains(point(px(5.0), px(5.0)), px(0.1)));
    }

    #[test]
    fn degenerate_curves_do_not_hang_the_flattener() {
        let mut b = PathBuilder::new();
        let z = point(px(0.0), px(0.0));
        b.move_to(z);
        b.cubic_to(z, z, z);
        b.quad_to(z, z);
        let mut n = 0;
        b.build().flatten(px(0.01), |_, _| n += 1);
        assert!(n < 10);
    }
}
