//! 2D affine transforms.
//!
//! Sphere stores transforms as a 2x3 matrix rather than a full 3x3. UI never
//! needs projective transforms, and the compact form halves the bytes uploaded
//! per instanced primitive.

use crate::geometry::{Point, Rect, Size};
use crate::unit::{Px, Rad};
use core::ops::Mul;

/// A 2D affine transform.
///
/// The matrix maps `(x, y)` to `(a*x + c*y + tx, b*x + d*y + ty)`, matching the
/// column-vector convention used by SVG, Core Graphics and Direct2D:
///
/// ```text
/// | a  c  tx |
/// | b  d  ty |
/// | 0  0  1  |
/// ```
#[derive(Copy, Clone, PartialEq, Debug)]
#[repr(C)]
pub struct Affine {
    /// Row 0, column 0: x scale.
    pub a: f32,
    /// Row 1, column 0: y skew.
    pub b: f32,
    /// Row 0, column 1: x skew.
    pub c: f32,
    /// Row 1, column 1: y scale.
    pub d: f32,
    /// Row 0, column 2: x translation.
    pub tx: f32,
    /// Row 1, column 2: y translation.
    pub ty: f32,
}

impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Affine {
    /// The identity transform.
    pub const IDENTITY: Self = Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 };

    /// Pure translation.
    #[inline]
    pub fn translate(offset: Size<Px>) -> Self {
        Self { tx: offset.width.get(), ty: offset.height.get(), ..Self::IDENTITY }
    }

    /// Pure scale about the origin.
    #[inline]
    pub fn scale(sx: f32, sy: f32) -> Self {
        Self { a: sx, d: sy, ..Self::IDENTITY }
    }

    /// Uniform scale about the origin.
    #[inline]
    pub fn uniform_scale(s: f32) -> Self {
        Self::scale(s, s)
    }

    /// Rotation about the origin, clockwise in Sphere's y-down space.
    #[inline]
    pub fn rotate(angle: Rad) -> Self {
        let (s, c) = angle.sin_cos();
        Self { a: c, b: s, c: -s, d: c, tx: 0.0, ty: 0.0 }
    }

    /// Skew, with both angles measured from the respective axis.
    #[inline]
    pub fn skew(x: Rad, y: Rad) -> Self {
        Self { a: 1.0, b: y.0.tan(), c: x.0.tan(), d: 1.0, tx: 0.0, ty: 0.0 }
    }

    /// A transform that applies `inner` and then rotates about `pivot`.
    ///
    /// Rotating a widget about its own centre is by far the most common case,
    /// and doing the translate/rotate/untranslate dance by hand is a reliable
    /// source of off-by-a-half-pixel bugs.
    #[inline]
    pub fn rotate_about(pivot: Point<Px>, angle: Rad) -> Self {
        let to = Self::translate(Size::new(pivot.x, pivot.y));
        let back = Self::translate(Size::new(-pivot.x, -pivot.y));
        to.then(Self::rotate(angle)).then(back)
    }

    /// A transform that scales about `pivot`.
    #[inline]
    pub fn scale_about(pivot: Point<Px>, sx: f32, sy: f32) -> Self {
        let to = Self::translate(Size::new(pivot.x, pivot.y));
        let back = Self::translate(Size::new(-pivot.x, -pivot.y));
        to.then(Self::scale(sx, sy)).then(back)
    }

    /// Returns `self` followed by `next`.
    ///
    /// Reading left to right in application order is far less error-prone than
    /// remembering which side of a `*` means "first".
    #[inline]
    pub fn then(self, next: Self) -> Self {
        next.pre_concat(self)
    }

    /// Returns the transform that applies `inner` first, then `self`.
    #[inline]
    pub fn pre_concat(self, inner: Self) -> Self {
        Self {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            tx: self.a * inner.tx + self.c * inner.ty + self.tx,
            ty: self.b * inner.tx + self.d * inner.ty + self.ty,
        }
    }

    /// Transforms a point.
    #[inline]
    pub fn apply(self, p: Point<Px>) -> Point<Px> {
        let (x, y) = (p.x.get(), p.y.get());
        Point { x: Px(self.a * x + self.c * y + self.tx), y: Px(self.b * x + self.d * y + self.ty) }
    }

    /// Transforms an offset vector, ignoring translation.
    #[inline]
    pub fn apply_vector(self, v: Size<Px>) -> Size<Px> {
        let (x, y) = (v.width.get(), v.height.get());
        Size { width: Px(self.a * x + self.c * y), height: Px(self.b * x + self.d * y) }
    }

    /// The determinant. Zero means the transform collapses to a line or point.
    #[inline]
    pub fn determinant(self) -> f32 {
        self.a * self.d - self.b * self.c
    }

    /// The inverse, or `None` when the transform is singular.
    pub fn inverse(self) -> Option<Self> {
        let det = self.determinant();
        if det.abs() < 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        Some(Self {
            a: self.d * inv,
            b: -self.b * inv,
            c: -self.c * inv,
            d: self.a * inv,
            tx: (self.c * self.ty - self.d * self.tx) * inv,
            ty: (self.b * self.tx - self.a * self.ty) * inv,
        })
    }

    /// True when the transform is a translation only.
    ///
    /// The renderer uses this to keep the cheap instanced-rectangle path alive
    /// for scrolled content instead of falling back to general mesh output.
    #[inline]
    pub fn is_translation_only(self) -> bool {
        (self.a - 1.0).abs() < 1e-6
            && self.b.abs() < 1e-6
            && self.c.abs() < 1e-6
            && (self.d - 1.0).abs() < 1e-6
    }

    /// True when the transform maps axis-aligned rectangles to axis-aligned
    /// rectangles, which is what allows scissor-rect clipping instead of a mask.
    #[inline]
    pub fn is_axis_aligned(self) -> bool {
        (self.b.abs() < 1e-6 && self.c.abs() < 1e-6)
            || (self.a.abs() < 1e-6 && self.d.abs() < 1e-6)
    }

    /// The translation component.
    #[inline]
    pub fn translation(self) -> Size<Px> {
        Size { width: Px(self.tx), height: Px(self.ty) }
    }

    /// An approximate uniform scale factor, used to pick MTSDF smoothing and
    /// tessellation tolerance under zoom.
    ///
    /// The square root of `|det|` is the area-preserving mean of the two axis
    /// scales, which behaves better than either axis alone under skew.
    #[inline]
    pub fn approx_scale(self) -> f32 {
        self.determinant().abs().sqrt()
    }

    /// The axis-aligned bounding box of a transformed rectangle.
    pub fn transform_rect_bounds(self, r: Rect<Px>) -> Rect<Px> {
        if self.is_translation_only() {
            return r.translate(self.translation());
        }
        let p = [
            self.apply(r.origin),
            self.apply(Point::new(r.max_x(), r.min_y())),
            self.apply(r.max_point()),
            self.apply(Point::new(r.min_x(), r.max_y())),
        ];
        let mut min = p[0];
        let mut max = p[0];
        for q in &p[1..] {
            min = min.min(*q);
            max = max.max(*q);
        }
        Rect::from_corners(min, max)
    }

    /// Packs into the `[a, b, c, d, tx, ty]` layout used by GPU instance data.
    #[inline]
    pub fn to_array(self) -> [f32; 6] {
        [self.a, self.b, self.c, self.d, self.tx, self.ty]
    }

    /// Packs into two `vec4`s, the layout a WGSL uniform expects.
    #[inline]
    pub fn to_gpu(self) -> [[f32; 4]; 2] {
        [[self.a, self.b, self.c, self.d], [self.tx, self.ty, 0.0, 0.0]]
    }
}

/// `a * b` applies `b` first, then `a`, matching standard matrix convention.
impl Mul for Affine {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        self.pre_concat(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{point, rect, size};
    use crate::unit::{Deg, px};

    fn approx(a: Point<Px>, b: Point<Px>) {
        assert!(
            (a.x.get() - b.x.get()).abs() < 1e-3 && (a.y.get() - b.y.get()).abs() < 1e-3,
            "{a:?} != {b:?}"
        );
    }

    #[test]
    fn identity_is_a_no_op() {
        let p = point(px(3.0), px(4.0));
        assert_eq!(Affine::IDENTITY.apply(p), p);
    }

    #[test]
    fn then_reads_left_to_right() {
        // Translate by (10,0) and then scale 2x should land at 20, not 10 + 2*0.
        let t = Affine::translate(size(px(10.0), px(0.0))).then(Affine::uniform_scale(2.0));
        approx(t.apply(Point::ZERO), point(px(20.0), px(0.0)));
    }

    #[test]
    fn rotation_about_a_pivot_leaves_the_pivot_fixed() {
        let pivot = point(px(50.0), px(50.0));
        let t = Affine::rotate_about(pivot, Deg(37.0).to_rad());
        approx(t.apply(pivot), pivot);
    }

    #[test]
    fn quarter_turn_maps_x_to_y() {
        // y-down space: a positive rotation takes +x toward +y.
        let t = Affine::rotate(Deg(90.0).to_rad());
        approx(t.apply(point(px(1.0), px(0.0))), point(px(0.0), px(1.0)));
    }

    #[test]
    fn inverse_roundtrips() {
        let t = Affine::translate(size(px(12.0), px(-3.0)))
            .then(Affine::rotate(Deg(23.0).to_rad()))
            .then(Affine::scale(2.0, 3.0));
        let inv = t.inverse().expect("non-singular");
        let p = point(px(7.0), px(11.0));
        approx(inv.apply(t.apply(p)), p);
    }

    #[test]
    fn singular_transforms_have_no_inverse() {
        assert!(Affine::scale(0.0, 1.0).inverse().is_none());
    }

    #[test]
    fn translation_only_is_detected_for_the_fast_path() {
        assert!(Affine::translate(size(px(5.0), px(5.0))).is_translation_only());
        assert!(!Affine::uniform_scale(2.0).is_translation_only());
        assert!(!Affine::rotate(Deg(1.0).to_rad()).is_translation_only());
    }

    #[test]
    fn axis_alignment_survives_scale_but_not_arbitrary_rotation() {
        assert!(Affine::scale(2.0, 3.0).is_axis_aligned());
        assert!(Affine::rotate(Deg(90.0).to_rad()).is_axis_aligned());
        assert!(!Affine::rotate(Deg(45.0).to_rad()).is_axis_aligned());
    }

    #[test]
    fn rotated_bounds_enclose_the_source() {
        let r = rect(px(0.0), px(0.0), px(10.0), px(10.0));
        let b = Affine::rotate(Deg(45.0).to_rad()).transform_rect_bounds(r);
        // A 45-degree rotation of a 10x10 square spans 10*sqrt(2) on each axis.
        assert!((b.width().get() - 14.142).abs() < 0.01, "{b:?}");
        assert!((b.height().get() - 14.142).abs() < 0.01, "{b:?}");
    }

    #[test]
    fn approx_scale_tracks_uniform_zoom() {
        assert!((Affine::uniform_scale(3.0).approx_scale() - 3.0).abs() < 1e-5);
        assert!((Affine::rotate(Deg(30.0).to_rad()).approx_scale() - 1.0).abs() < 1e-5);
    }
}
