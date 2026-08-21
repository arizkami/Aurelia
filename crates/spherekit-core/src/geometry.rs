//! Unit-generic geometry primitives.
//!
//! Every type here is generic over a [`Scalar`], which is how SphereKit keeps
//! logical and device pixels from mixing: a `Rect<Px>` and a `Rect<DevicePx>`
//! are different types and cannot be added together by accident.

use crate::unit::{DevicePx, Px, Scalar, ScaleFactor};
use core::fmt;
use core::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};

/// A point in 2D space.
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Point<T> {
    /// Horizontal coordinate, increasing rightwards.
    pub x: T,
    /// Vertical coordinate, increasing downwards.
    pub y: T,
}

/// Shorthand constructor for [`Point`].
#[inline]
pub const fn point<T>(x: T, y: T) -> Point<T> {
    Point { x, y }
}

impl<T: fmt::Debug> fmt::Debug for Point<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({:?}, {:?})", self.x, self.y)
    }
}

impl<T: Scalar> Point<T> {
    /// The origin.
    pub const ZERO: Self = Self { x: T::ZERO, y: T::ZERO };

    /// Builds a point.
    #[inline]
    pub const fn new(x: T, y: T) -> Self {
        Self { x, y }
    }

    /// Applies `f` to both components.
    #[inline]
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Point<U> {
        Point { x: f(self.x), y: f(self.y) }
    }

    /// Component-wise minimum.
    #[inline]
    pub fn min(self, o: Self) -> Self {
        Self { x: self.x.min_of(o.x), y: self.y.min_of(o.y) }
    }

    /// Component-wise maximum.
    #[inline]
    pub fn max(self, o: Self) -> Self {
        Self { x: self.x.max_of(o.x), y: self.y.max_of(o.y) }
    }

    /// Reinterprets the point as an offset from the origin.
    #[inline]
    pub fn to_vector(self) -> Size<T> {
        Size { width: self.x, height: self.y }
    }
}

impl<T: Scalar> Add<Size<T>> for Point<T> {
    type Output = Self;
    #[inline]
    fn add(self, o: Size<T>) -> Self {
        Self { x: self.x + o.width, y: self.y + o.height }
    }
}
impl<T: Scalar> Sub<Size<T>> for Point<T> {
    type Output = Self;
    #[inline]
    fn sub(self, o: Size<T>) -> Self {
        Self { x: self.x - o.width, y: self.y - o.height }
    }
}
/// Subtracting two points gives the offset between them.
impl<T: Scalar> Sub for Point<T> {
    type Output = Size<T>;
    #[inline]
    fn sub(self, o: Self) -> Size<T> {
        Size { width: self.x - o.x, height: self.y - o.y }
    }
}
impl<T: Scalar> AddAssign<Size<T>> for Point<T> {
    #[inline]
    fn add_assign(&mut self, o: Size<T>) {
        self.x = self.x + o.width;
        self.y = self.y + o.height;
    }
}
impl<T: Scalar + Mul<f32, Output = T>> Mul<f32> for Point<T> {
    type Output = Self;
    #[inline]
    fn mul(self, s: f32) -> Self {
        Self { x: self.x * s, y: self.y * s }
    }
}

impl Point<Px> {
    /// Euclidean distance to another point.
    #[inline]
    pub fn distance_to(self, o: Self) -> Px {
        let d = self - o;
        Px((d.width.get() * d.width.get() + d.height.get() * d.height.get()).sqrt())
    }

    /// Linear interpolation, where `t == 0` yields `self`.
    #[inline]
    pub fn lerp(self, o: Self, t: f32) -> Self {
        Self {
            x: Px(self.x.get() + (o.x.get() - self.x.get()) * t),
            y: Px(self.y.get() + (o.y.get() - self.y.get()) * t),
        }
    }

    /// Converts to device space for GPU submission. Not rounded: see
    /// [`ScaleFactor::to_device_f32`].
    #[inline]
    pub fn to_device_f32(self, sf: ScaleFactor) -> [f32; 2] {
        [sf.to_device_f32(self.x), sf.to_device_f32(self.y)]
    }
}

/// A 2D extent, also used as an offset vector.
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Size<T> {
    /// Extent along x.
    pub width: T,
    /// Extent along y.
    pub height: T,
}

/// Shorthand constructor for [`Size`].
#[inline]
pub const fn size<T>(width: T, height: T) -> Size<T> {
    Size { width, height }
}

impl<T: fmt::Debug> fmt::Debug for Size<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}x{:?}", self.width, self.height)
    }
}

impl<T: Scalar> Size<T> {
    /// A zero-area size.
    pub const ZERO: Self = Self { width: T::ZERO, height: T::ZERO };

    /// Builds a size.
    #[inline]
    pub const fn new(width: T, height: T) -> Self {
        Self { width, height }
    }

    /// The same extent on both axes.
    #[inline]
    pub fn splat(v: T) -> Self {
        Self { width: v, height: v }
    }

    /// Applies `f` to both components.
    #[inline]
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Size<U> {
        Size { width: f(self.width), height: f(self.height) }
    }

    /// Component-wise minimum.
    #[inline]
    pub fn min(self, o: Self) -> Self {
        Self { width: self.width.min_of(o.width), height: self.height.min_of(o.height) }
    }

    /// Component-wise maximum.
    #[inline]
    pub fn max(self, o: Self) -> Self {
        Self { width: self.width.max_of(o.width), height: self.height.max_of(o.height) }
    }

    /// True when either axis is zero or negative, so nothing can be drawn.
    #[inline]
    pub fn is_empty(self) -> bool {
        self.width <= T::ZERO || self.height <= T::ZERO
    }

    /// Reinterprets the extent as a point offset from the origin.
    #[inline]
    pub fn to_point(self) -> Point<T> {
        Point { x: self.width, y: self.height }
    }
}

impl<T: Scalar> Add for Size<T> {
    type Output = Self;
    #[inline]
    fn add(self, o: Self) -> Self {
        Self { width: self.width + o.width, height: self.height + o.height }
    }
}
impl<T: Scalar> Sub for Size<T> {
    type Output = Self;
    #[inline]
    fn sub(self, o: Self) -> Self {
        Self { width: self.width - o.width, height: self.height - o.height }
    }
}
impl<T: Scalar> AddAssign for Size<T> {
    #[inline]
    fn add_assign(&mut self, o: Self) {
        self.width = self.width + o.width;
        self.height = self.height + o.height;
    }
}
impl<T: Scalar> SubAssign for Size<T> {
    #[inline]
    fn sub_assign(&mut self, o: Self) {
        self.width = self.width - o.width;
        self.height = self.height - o.height;
    }
}
impl<T: Scalar> Neg for Size<T> {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self { width: -self.width, height: -self.height }
    }
}
impl<T: Scalar + Mul<f32, Output = T>> Mul<f32> for Size<T> {
    type Output = Self;
    #[inline]
    fn mul(self, s: f32) -> Self {
        Self { width: self.width * s, height: self.height * s }
    }
}

impl Size<Px> {
    /// Length of the vector.
    #[inline]
    pub fn length(self) -> Px {
        Px((self.width.get() * self.width.get() + self.height.get() * self.height.get()).sqrt())
    }

    /// Converts to a physical surface extent, clamped to at least 1x1.
    ///
    /// Surfaces of zero extent are invalid on every backend, and a minimised
    /// window legitimately reports zero, so clamping here is what keeps a
    /// minimise from tearing down the swapchain.
    #[inline]
    pub fn to_surface_extent(self, sf: ScaleFactor) -> (u32, u32) {
        (sf.to_device(self.width).as_u32().max(1), sf.to_device(self.height).as_u32().max(1))
    }
}

impl Size<DevicePx> {
    /// Converts to a `(width, height)` pair for GPU APIs, clamped to at least 1x1.
    #[inline]
    pub fn to_extent(self) -> (u32, u32) {
        (self.width.as_u32().max(1), self.height.as_u32().max(1))
    }
}

/// An axis-aligned rectangle, stored as origin plus extent.
///
/// Origin-plus-extent (rather than min/max) is the representation that makes
/// layout output, hit testing and instanced GPU rectangles all cheap.
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Rect<T> {
    /// Top-left corner.
    pub origin: Point<T>,
    /// Extent from the origin.
    pub size: Size<T>,
}

/// Shorthand constructor for [`Rect`] from `x, y, width, height`.
#[inline]
pub fn rect<T>(x: T, y: T, width: T, height: T) -> Rect<T> {
    Rect { origin: Point { x, y }, size: Size { width, height } }
}

impl<T: fmt::Debug> fmt::Debug for Rect<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rect({:?} {:?})", self.origin, self.size)
    }
}

impl<T: Scalar> Rect<T> {
    /// An empty rectangle at the origin.
    pub const ZERO: Self = Self { origin: Point::ZERO, size: Size::ZERO };

    /// Builds a rectangle.
    #[inline]
    pub const fn new(origin: Point<T>, size: Size<T>) -> Self {
        Self { origin, size }
    }

    /// Builds a rectangle from two opposite corners, in either order.
    #[inline]
    pub fn from_corners(a: Point<T>, b: Point<T>) -> Self {
        let min = a.min(b);
        let max = a.max(b);
        Self { origin: min, size: max - min }
    }

    /// Left edge.
    #[inline]
    pub fn min_x(self) -> T {
        self.origin.x
    }
    /// Top edge.
    #[inline]
    pub fn min_y(self) -> T {
        self.origin.y
    }
    /// Right edge.
    #[inline]
    pub fn max_x(self) -> T {
        self.origin.x + self.size.width
    }
    /// Bottom edge.
    #[inline]
    pub fn max_y(self) -> T {
        self.origin.y + self.size.height
    }
    /// Width.
    #[inline]
    pub fn width(self) -> T {
        self.size.width
    }
    /// Height.
    #[inline]
    pub fn height(self) -> T {
        self.size.height
    }

    /// Bottom-right corner.
    #[inline]
    pub fn max_point(self) -> Point<T> {
        Point { x: self.max_x(), y: self.max_y() }
    }

    /// Geometric centre.
    #[inline]
    pub fn center(self) -> Point<T> {
        Point {
            x: self.origin.x + self.size.width.half(),
            y: self.origin.y + self.size.height.half(),
        }
    }

    /// True when the rectangle encloses no area.
    #[inline]
    pub fn is_empty(self) -> bool {
        self.size.is_empty()
    }

    /// True when `p` is inside, treating the min edges as inclusive and the max
    /// edges as exclusive. Half-open containment is what makes adjacent
    /// rectangles tile without double-hits during hit testing.
    #[inline]
    pub fn contains(self, p: Point<T>) -> bool {
        p.x >= self.min_x() && p.x < self.max_x() && p.y >= self.min_y() && p.y < self.max_y()
    }

    /// True when `other` lies entirely within `self`.
    #[inline]
    pub fn contains_rect(self, other: Self) -> bool {
        other.min_x() >= self.min_x()
            && other.min_y() >= self.min_y()
            && other.max_x() <= self.max_x()
            && other.max_y() <= self.max_y()
    }

    /// True when the two rectangles overlap in a region of nonzero area.
    #[inline]
    pub fn intersects(self, o: Self) -> bool {
        self.min_x() < o.max_x()
            && o.min_x() < self.max_x()
            && self.min_y() < o.max_y()
            && o.min_y() < self.max_y()
    }

    /// The overlapping region, or an empty rectangle when they are disjoint.
    ///
    /// The result is always clamped to non-negative extents so that a chain of
    /// intersections, which is exactly what a clip stack is, can never produce
    /// an inside-out rectangle.
    #[inline]
    pub fn intersection(self, o: Self) -> Self {
        let min = self.origin.max(o.origin);
        let max = self.max_point().min(o.max_point());
        Self {
            origin: min,
            size: Size {
                width: (max.x - min.x).max_of(T::ZERO),
                height: (max.y - min.y).max_of(T::ZERO),
            },
        }
    }

    /// The smallest rectangle containing both.
    #[inline]
    pub fn union(self, o: Self) -> Self {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        Self::from_corners(self.origin.min(o.origin), self.max_point().max(o.max_point()))
    }

    /// Moves the rectangle by an offset.
    #[inline]
    pub fn translate(self, by: Size<T>) -> Self {
        Self { origin: self.origin + by, size: self.size }
    }

    /// Shrinks by `insets` on each side, clamping to a non-negative extent.
    #[inline]
    pub fn inset(self, insets: Edges<T>) -> Self {
        let origin = Point { x: self.origin.x + insets.left, y: self.origin.y + insets.top };
        let w = self.size.width - insets.left - insets.right;
        let h = self.size.height - insets.top - insets.bottom;
        Self { origin, size: Size { width: w.max_of(T::ZERO), height: h.max_of(T::ZERO) } }
    }

    /// Grows by `outsets` on each side.
    #[inline]
    pub fn outset(self, outsets: Edges<T>) -> Self {
        self.inset(-outsets)
    }

    /// Applies `f` to every component.
    #[inline]
    pub fn map<U: Scalar>(self, mut f: impl FnMut(T) -> U) -> Rect<U> {
        Rect {
            origin: Point { x: f(self.origin.x), y: f(self.origin.y) },
            size: Size { width: f(self.size.width), height: f(self.size.height) },
        }
    }
}

impl Rect<Px> {
    /// Expands to the nearest enclosing whole-device-pixel rectangle.
    ///
    /// Used for invalidation and scissor rectangles, where covering slightly too
    /// much is correct and covering slightly too little is a visible artifact.
    #[inline]
    pub fn round_out(self, sf: ScaleFactor) -> Rect<DevicePx> {
        let x0 = (self.min_x().get() * sf.get()).floor() as i32;
        let y0 = (self.min_y().get() * sf.get()).floor() as i32;
        let x1 = (self.max_x().get() * sf.get()).ceil() as i32;
        let y1 = (self.max_y().get() * sf.get()).ceil() as i32;
        Rect {
            origin: Point { x: DevicePx(x0), y: DevicePx(y0) },
            size: Size { width: DevicePx(x1 - x0), height: DevicePx(y1 - y0) },
        }
    }

    /// An infinite rectangle, used as the identity element of a clip stack.
    pub const INFINITE: Self = Rect {
        origin: Point { x: Px(-1.0e9), y: Px(-1.0e9) },
        size: Size { width: Px(2.0e9), height: Px(2.0e9) },
    };
}

/// Per-side values: margins, padding, borders.
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct Edges<T> {
    /// Top side.
    pub top: T,
    /// Right side.
    pub right: T,
    /// Bottom side.
    pub bottom: T,
    /// Left side.
    pub left: T,
}

impl<T: Scalar> Edges<T> {
    /// All sides zero.
    pub const ZERO: Self = Self { top: T::ZERO, right: T::ZERO, bottom: T::ZERO, left: T::ZERO };

    /// The same value on every side.
    #[inline]
    pub fn all(v: T) -> Self {
        Self { top: v, right: v, bottom: v, left: v }
    }

    /// `vertical` on top and bottom, `horizontal` on left and right.
    #[inline]
    pub fn symmetric(vertical: T, horizontal: T) -> Self {
        Self { top: vertical, right: horizontal, bottom: vertical, left: horizontal }
    }

    /// Total horizontal contribution.
    #[inline]
    pub fn horizontal(self) -> T {
        self.left + self.right
    }

    /// Total vertical contribution.
    #[inline]
    pub fn vertical(self) -> T {
        self.top + self.bottom
    }

    /// Applies `f` to every side.
    #[inline]
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Edges<U> {
        Edges { top: f(self.top), right: f(self.right), bottom: f(self.bottom), left: f(self.left) }
    }
}

impl<T: Scalar> Neg for Edges<T> {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self { top: -self.top, right: -self.right, bottom: -self.bottom, left: -self.left }
    }
}

/// Per-corner values, used for border radii.
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct Corners<T> {
    /// Top-left corner.
    pub top_left: T,
    /// Top-right corner.
    pub top_right: T,
    /// Bottom-right corner.
    pub bottom_right: T,
    /// Bottom-left corner.
    pub bottom_left: T,
}

impl<T: Scalar> Corners<T> {
    /// All corners zero: a sharp rectangle.
    pub const ZERO: Self =
        Self { top_left: T::ZERO, top_right: T::ZERO, bottom_right: T::ZERO, bottom_left: T::ZERO };

    /// The same radius on every corner.
    #[inline]
    pub fn all(v: T) -> Self {
        Self { top_left: v, top_right: v, bottom_right: v, bottom_left: v }
    }

    /// True when every corner is zero, which lets the renderer take the cheap
    /// sharp-rectangle path instead of the rounded-rectangle SDF path.
    #[inline]
    pub fn is_zero(self) -> bool {
        self.top_left <= T::ZERO
            && self.top_right <= T::ZERO
            && self.bottom_right <= T::ZERO
            && self.bottom_left <= T::ZERO
    }

    /// Applies `f` to every corner.
    #[inline]
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Corners<U> {
        Corners {
            top_left: f(self.top_left),
            top_right: f(self.top_right),
            bottom_right: f(self.bottom_right),
            bottom_left: f(self.bottom_left),
        }
    }
}

impl Corners<Px> {
    /// Scales the radii down uniformly so that no pair of adjacent corners
    /// overlaps along a shared edge.
    ///
    /// This mirrors the CSS overlapping-curves rule. Without it, a 40 px radius
    /// on a 50 px-tall rectangle produces a self-intersecting outline that the
    /// SDF renders as a visible pinch.
    pub fn clamp_for(self, size: Size<Px>) -> Self {
        let w = size.width.get().max(0.0);
        let h = size.height.get().max(0.0);
        let r = self.map(|v| Px(v.get().max(0.0)));
        let pairs = [
            (r.top_left.get() + r.top_right.get(), w),
            (r.bottom_left.get() + r.bottom_right.get(), w),
            (r.top_left.get() + r.bottom_left.get(), h),
            (r.top_right.get() + r.bottom_right.get(), h),
        ];
        // A single uniform factor, the minimum over all four edges, is what
        // preserves the ratios the author asked for. Clamping each corner
        // independently would silently reshape an asymmetric design.
        let mut f = 1.0f32;
        for (sum, extent) in pairs {
            if sum > extent && sum > 0.0 {
                f = f.min(extent / sum);
            }
        }
        r.map(|v| Px(v.get() * f))
    }

    /// The four radii as a GPU-friendly array in `[tl, tr, br, bl]` order.
    #[inline]
    pub fn to_array(self) -> [f32; 4] {
        [self.top_left.get(), self.top_right.get(), self.bottom_right.get(), self.bottom_left.get()]
    }
}

/// A rectangle with independently rounded corners.
#[derive(Copy, Clone, Default, PartialEq, Debug)]
pub struct RoundedRect {
    /// The underlying rectangle.
    pub rect: Rect<Px>,
    /// Corner radii, not yet clamped to the rectangle.
    pub radii: Corners<Px>,
}

impl RoundedRect {
    /// Builds a rounded rectangle with a uniform radius.
    #[inline]
    pub fn uniform(rect: Rect<Px>, radius: Px) -> Self {
        Self { rect, radii: Corners::all(radius) }
    }

    /// Builds a rounded rectangle from an existing rectangle and radii.
    #[inline]
    pub fn new(rect: Rect<Px>, radii: Corners<Px>) -> Self {
        Self { rect, radii }
    }

    /// The radii clamped so adjacent corners cannot overlap.
    #[inline]
    pub fn clamped_radii(&self) -> Corners<Px> {
        self.radii.clamp_for(self.rect.size)
    }

    /// True when `p` lies inside the rounded outline.
    ///
    /// Corner regions are tested against the corresponding ellipse quadrant so
    /// that hit testing agrees with what the SDF shader actually draws.
    pub fn contains(&self, p: Point<Px>) -> bool {
        if !self.rect.contains(p) {
            return false;
        }
        let r = self.clamped_radii();
        let (x, y) = (p.x.get(), p.y.get());
        let (x0, y0) = (self.rect.min_x().get(), self.rect.min_y().get());
        let (x1, y1) = (self.rect.max_x().get(), self.rect.max_y().get());

        let corner = |cx: f32, cy: f32, rad: f32| -> bool {
            if rad <= 0.0 {
                return true;
            }
            let dx = (x - cx) / rad;
            let dy = (y - cy) / rad;
            dx * dx + dy * dy <= 1.0
        };

        let (tl, tr, br, bl) =
            (r.top_left.get(), r.top_right.get(), r.bottom_right.get(), r.bottom_left.get());

        if x < x0 + tl && y < y0 + tl {
            return corner(x0 + tl, y0 + tl, tl);
        }
        if x > x1 - tr && y < y0 + tr {
            return corner(x1 - tr, y0 + tr, tr);
        }
        if x > x1 - br && y > y1 - br {
            return corner(x1 - br, y1 - br, br);
        }
        if x < x0 + bl && y > y1 - bl {
            return corner(x0 + bl, y1 - bl, bl);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::px;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect<Px> {
        rect(px(x), px(y), px(w), px(h))
    }

    #[test]
    fn containment_is_half_open_so_adjacent_rects_tile() {
        let a = r(0.0, 0.0, 10.0, 10.0);
        let b = r(10.0, 0.0, 10.0, 10.0);
        let p = point(px(10.0), px(5.0));
        assert!(!a.contains(p), "max edge must be exclusive");
        assert!(b.contains(p), "min edge must be inclusive");
    }

    #[test]
    fn intersection_of_disjoint_rects_is_empty_not_negative() {
        let i = r(0.0, 0.0, 10.0, 10.0).intersection(r(50.0, 50.0, 10.0, 10.0));
        assert!(i.is_empty());
        assert!(i.width() >= Px::ZERO && i.height() >= Px::ZERO, "must not go inside-out");
    }

    #[test]
    fn clip_stack_chain_stays_valid() {
        // A clip stack is a fold of intersections; it must never produce a
        // rectangle with negative extent regardless of ordering.
        let clips = [r(0.0, 0.0, 100.0, 100.0), r(90.0, 90.0, 50.0, 50.0), r(0.0, 0.0, 20.0, 20.0)];
        let acc = clips.iter().fold(Rect::INFINITE, |a, &b| a.intersection(b));
        assert!(acc.width() >= Px::ZERO && acc.height() >= Px::ZERO);
        assert!(acc.is_empty());
    }

    #[test]
    fn union_ignores_empty_operands() {
        let a = r(10.0, 10.0, 5.0, 5.0);
        assert_eq!(a.union(Rect::ZERO), a);
        assert_eq!(Rect::ZERO.union(a), a);
    }

    #[test]
    fn inset_clamps_instead_of_inverting() {
        let a = r(0.0, 0.0, 10.0, 10.0).inset(Edges::all(px(20.0)));
        assert_eq!(a.size, Size::ZERO);
    }

    #[test]
    fn round_out_covers_the_source_rect_at_fractional_dpi() {
        let sf = ScaleFactor::new(1.25);
        let src = r(1.1, 2.2, 3.3, 4.4);
        let d = src.round_out(sf);
        assert!(d.min_x().get() as f32 <= src.min_x().get() * 1.25);
        assert!(d.max_x().get() as f32 >= src.max_x().get() * 1.25);
        assert!(d.min_y().get() as f32 <= src.min_y().get() * 1.25);
        assert!(d.max_y().get() as f32 >= src.max_y().get() * 1.25);
    }

    #[test]
    fn corner_radii_clamp_to_avoid_self_intersection() {
        let c = Corners::all(px(40.0)).clamp_for(size(px(50.0), px(50.0)));
        assert!(c.top_left.get() <= 25.0 + 1e-4);
        // Adjacent radii must not sum past the edge they share.
        assert!(c.top_left.get() + c.top_right.get() <= 50.0 + 1e-4);
    }

    #[test]
    fn asymmetric_radii_scale_uniformly() {
        let c = Corners {
            top_left: px(80.0),
            top_right: px(20.0),
            bottom_right: px(0.0),
            bottom_left: px(0.0),
        };
        let clamped = c.clamp_for(size(px(50.0), px(200.0)));
        assert!(clamped.top_left.get() + clamped.top_right.get() <= 50.0 + 1e-4);
        // Uniform scaling preserves the 4:1 ratio between the two.
        assert!((clamped.top_left.get() / clamped.top_right.get() - 4.0).abs() < 1e-3);
    }

    #[test]
    fn rounded_rect_corner_hit_testing() {
        let rr = RoundedRect::uniform(r(0.0, 0.0, 100.0, 100.0), px(20.0));
        // Dead centre of the top-left corner square is outside the arc.
        assert!(!rr.contains(point(px(1.0), px(1.0))));
        // ...but the middle of the shape is inside.
        assert!(rr.contains(point(px(50.0), px(50.0))));
        // ...and a point on the flat top edge is inside.
        assert!(rr.contains(point(px(50.0), px(0.5))));
    }

    #[test]
    fn point_minus_point_is_an_offset() {
        let d = point(px(10.0), px(4.0)) - point(px(4.0), px(1.0));
        assert_eq!(d, size(px(6.0), px(3.0)));
    }

    #[test]
    fn surface_extent_never_degenerates_on_minimise() {
        let s = size(px(0.0), px(0.0));
        assert_eq!(s.to_surface_extent(ScaleFactor::IDENTITY), (1, 1));
    }
}
