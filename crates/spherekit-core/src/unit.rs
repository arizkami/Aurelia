//! Strongly typed units.
//!
//! SphereKit separates *logical* pixels ([`Px`]) from *physical device* pixels
//! ([`DevicePx`]). Every public authoring API speaks logical pixels; only the
//! renderer and the platform layer see device pixels. The conversion is always
//! explicit and always goes through a [`ScaleFactor`], which makes accidental
//! `HiDPI` mixing a type error rather than a blurry frame.

use core::fmt;
use core::iter::Sum;
use core::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Rem, Sub, SubAssign};

/// Scalar behaviour shared by every coordinate type used in [`crate::geometry`].
///
/// This is deliberately smaller than `num-traits`: it is exactly what the
/// geometry types need, so adding a new unit stays a five-line job.
pub trait Scalar:
    Copy
    + Clone
    + fmt::Debug
    + Default
    + PartialEq
    + PartialOrd
    + Add<Output = Self>
    + Sub<Output = Self>
    + Neg<Output = Self>
{
    /// The additive identity.
    const ZERO: Self;

    /// Component-wise minimum.
    fn min_of(self, other: Self) -> Self {
        if self < other { self } else { other }
    }

    /// Component-wise maximum.
    fn max_of(self, other: Self) -> Self {
        if self > other { self } else { other }
    }

    /// Clamp into `[lo, hi]`. `lo` is assumed to be `<= hi`.
    fn clamp_to(self, lo: Self, hi: Self) -> Self {
        self.max_of(lo).min_of(hi)
    }

    /// Linear midpoint, used by geometry helpers such as `Rect::center`.
    fn half(self) -> Self;
}

macro_rules! scalar_float {
    ($($t:ty),*) => {$(
        impl Scalar for $t {
            const ZERO: Self = 0.0;
            fn half(self) -> Self { self * 0.5 }
        }
    )*};
}
scalar_float!(f32, f64);

impl Scalar for i32 {
    const ZERO: Self = 0;
    fn half(self) -> Self {
        self / 2
    }
}

/// Declares an `f32` newtype unit with the full arithmetic surface.
macro_rules! float_unit {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Copy, Clone, Default, PartialEq, PartialOrd, bytemuck::Pod, bytemuck::Zeroable)]
        #[repr(transparent)]
        pub struct $name(pub f32);

        impl $name {
            /// The zero value.
            pub const ZERO: Self = Self(0.0);
            /// The unit value.
            pub const ONE: Self = Self(1.0);
            /// Positive infinity, used as an "unconstrained" sentinel.
            pub const INFINITY: Self = Self(f32::INFINITY);

            /// The raw scalar. Prefer the typed operations where possible.
            #[inline]
            pub const fn get(self) -> f32 { self.0 }

            /// Rounds to the nearest integral value.
            #[inline]
            pub fn round(self) -> Self { Self(self.0.round()) }
            /// Rounds toward negative infinity.
            #[inline]
            pub fn floor(self) -> Self { Self(self.0.floor()) }
            /// Rounds toward positive infinity.
            #[inline]
            pub fn ceil(self) -> Self { Self(self.0.ceil()) }
            /// Absolute value.
            #[inline]
            pub fn abs(self) -> Self { Self(self.0.abs()) }
            /// True when the value is finite: neither infinite nor NaN.
            #[inline]
            pub fn is_finite(self) -> bool { self.0.is_finite() }
            /// The smaller of two values.
            #[inline]
            pub fn min(self, o: Self) -> Self { Self(self.0.min(o.0)) }
            /// The larger of two values.
            #[inline]
            pub fn max(self, o: Self) -> Self { Self(self.0.max(o.0)) }
            /// Clamps into `[lo, hi]`.
            #[inline]
            pub fn clamp(self, lo: Self, hi: Self) -> Self { Self(self.0.clamp(lo.0, hi.0)) }
        }

        impl Scalar for $name {
            const ZERO: Self = Self(0.0);
            #[inline]
            fn half(self) -> Self { Self(self.0 * 0.5) }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl Add for $name {
            type Output = Self;
            #[inline]
            fn add(self, o: Self) -> Self { Self(self.0 + o.0) }
        }
        impl Sub for $name {
            type Output = Self;
            #[inline]
            fn sub(self, o: Self) -> Self { Self(self.0 - o.0) }
        }
        impl Neg for $name {
            type Output = Self;
            #[inline]
            fn neg(self) -> Self { Self(-self.0) }
        }
        impl AddAssign for $name {
            #[inline]
            fn add_assign(&mut self, o: Self) { self.0 += o.0 }
        }
        impl SubAssign for $name {
            #[inline]
            fn sub_assign(&mut self, o: Self) { self.0 -= o.0 }
        }
        impl Mul<f32> for $name {
            type Output = Self;
            #[inline]
            fn mul(self, s: f32) -> Self { Self(self.0 * s) }
        }
        impl Mul<$name> for f32 {
            type Output = $name;
            #[inline]
            fn mul(self, v: $name) -> $name { $name(self * v.0) }
        }
        impl MulAssign<f32> for $name {
            #[inline]
            fn mul_assign(&mut self, s: f32) { self.0 *= s }
        }
        impl Div<f32> for $name {
            type Output = Self;
            #[inline]
            fn div(self, s: f32) -> Self { Self(self.0 / s) }
        }
        /// Dividing two like units yields a bare ratio.
        impl Div for $name {
            type Output = f32;
            #[inline]
            fn div(self, o: Self) -> f32 { self.0 / o.0 }
        }
        impl Rem for $name {
            type Output = Self;
            #[inline]
            fn rem(self, o: Self) -> Self { Self(self.0 % o.0) }
        }
        impl Sum for $name {
            fn sum<I: Iterator<Item = Self>>(it: I) -> Self { Self(it.map(|v| v.0).sum()) }
        }
        impl From<f32> for $name {
            #[inline]
            fn from(v: f32) -> Self { Self(v) }
        }
        impl From<$name> for f32 {
            #[inline]
            fn from(v: $name) -> f32 { v.0 }
        }
    };
}

float_unit! {
    /// A logical pixel: the unit every public SphereKit API speaks.
    ///
    /// One `Px` is one CSS-style pixel. On a 200 % display a single `Px` covers
    /// two device pixels; nothing in the layout or widget layer needs to know that.
    Px
}

float_unit! {
    /// A font-relative size in points, kept distinct so a point size is never
    /// silently used where a pixel size belongs.
    Pt
}

/// Shorthand constructor for [`Px`].
#[inline]
pub const fn px(v: f32) -> Px {
    Px(v)
}

/// Shorthand constructor for [`Pt`].
#[inline]
pub const fn pt(v: f32) -> Pt {
    Pt(v)
}

impl Pt {
    /// Converts to logical pixels at the CSS-standard 96 dpi / 72 pt ratio.
    #[inline]
    pub fn to_px(self) -> Px {
        Px(self.0 * (96.0 / 72.0))
    }
}

/// A physical pixel on the actual display surface.
///
/// Integral by construction: fractional device pixels do not exist, and rounding
/// at exactly one place, the [`ScaleFactor`] conversion, is what keeps geometry
/// crisp at 125 % and 150 % scaling.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, bytemuck::Pod, bytemuck::Zeroable,
)]
#[repr(transparent)]
pub struct DevicePx(pub i32);

impl DevicePx {
    /// Zero device pixels.
    pub const ZERO: Self = Self(0);

    /// The raw integral value.
    #[inline]
    pub const fn get(self) -> i32 {
        self.0
    }

    /// Saturating conversion to an unsigned extent, for GPU texture and surface sizes.
    #[inline]
    pub const fn as_u32(self) -> u32 {
        if self.0 < 0 { 0 } else { self.0 as u32 }
    }
}

impl Scalar for DevicePx {
    const ZERO: Self = Self(0);
    #[inline]
    fn half(self) -> Self {
        Self(self.0 / 2)
    }
}

impl fmt::Debug for DevicePx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DevicePx({})", self.0)
    }
}
impl Add for DevicePx {
    type Output = Self;
    #[inline]
    fn add(self, o: Self) -> Self {
        Self(self.0 + o.0)
    }
}
impl Sub for DevicePx {
    type Output = Self;
    #[inline]
    fn sub(self, o: Self) -> Self {
        Self(self.0 - o.0)
    }
}
impl Neg for DevicePx {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

/// The ratio between device pixels and logical pixels for one surface.
///
/// Fractional values (1.25, 1.5, 1.75) are first-class; nothing in SphereKit
/// assumes an integral scale.
#[derive(Copy, Clone, PartialEq, PartialOrd, Debug)]
#[repr(transparent)]
pub struct ScaleFactor(f32);

impl Default for ScaleFactor {
    fn default() -> Self {
        Self(1.0)
    }
}

impl ScaleFactor {
    /// 100 % scaling.
    pub const IDENTITY: Self = Self(1.0);

    /// Builds a scale factor, clamping to a sane positive range.
    ///
    /// A zero or negative scale would make every conversion produce degenerate
    /// geometry, and platforms have been observed to report `0.0` transiently
    /// while a window moves between monitors.
    #[inline]
    pub fn new(v: f32) -> Self {
        Self(if v.is_finite() && v > 0.0 { v.clamp(0.05, 64.0) } else { 1.0 })
    }

    /// The raw ratio.
    #[inline]
    pub const fn get(self) -> f32 {
        self.0
    }

    /// Converts logical pixels to device pixels, rounding to the nearest integer.
    #[inline]
    pub fn to_device(self, v: Px) -> DevicePx {
        DevicePx((v.0 * self.0).round() as i32)
    }

    /// Converts logical pixels to unrounded device-space float, for GPU vertex data.
    ///
    /// Vertex positions must *not* be rounded: rounding them is what produces
    /// unstable geometry when a fractional scale factor is in play.
    #[inline]
    pub fn to_device_f32(self, v: Px) -> f32 {
        v.0 * self.0
    }

    /// Converts device pixels back to logical pixels.
    #[inline]
    pub fn to_logical(self, v: DevicePx) -> Px {
        Px(v.0 as f32 / self.0)
    }

    /// Converts a raw physical extent to logical pixels.
    #[inline]
    pub fn logical_from_physical(self, v: u32) -> Px {
        Px(v as f32 / self.0)
    }
}

/// An angle in radians.
#[derive(Copy, Clone, Default, PartialEq, PartialOrd, Debug)]
#[repr(transparent)]
pub struct Rad(pub f32);

/// An angle in degrees.
#[derive(Copy, Clone, Default, PartialEq, PartialOrd, Debug)]
#[repr(transparent)]
pub struct Deg(pub f32);

impl Rad {
    /// A full turn.
    pub const TAU: Self = Self(core::f32::consts::TAU);
    /// Converts to degrees.
    #[inline]
    pub fn to_deg(self) -> Deg {
        Deg(self.0.to_degrees())
    }
    /// `(sin, cos)` of the angle.
    #[inline]
    pub fn sin_cos(self) -> (f32, f32) {
        self.0.sin_cos()
    }
}

impl Deg {
    /// Converts to radians.
    #[inline]
    pub fn to_rad(self) -> Rad {
        Rad(self.0.to_radians())
    }
}

impl From<Deg> for Rad {
    #[inline]
    fn from(d: Deg) -> Rad {
        d.to_rad()
    }
}
impl From<Rad> for Deg {
    #[inline]
    fn from(r: Rad) -> Deg {
        r.to_deg()
    }
}

/// A length that may be absolute, relative to the parent, or content-derived.
///
/// This is the authoring-side length used by SphereKit consumers; the layout crate
/// maps it onto whatever the backing layout algorithm needs.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub enum Length {
    /// Size determined by the layout algorithm and the node's content.
    #[default]
    Auto,
    /// A fixed number of logical pixels.
    Px(Px),
    /// A fraction of the parent's corresponding axis, where `1.0` is 100 %.
    Fraction(f32),
}

impl Length {
    /// Resolves against a parent extent, yielding `None` for [`Length::Auto`].
    #[inline]
    pub fn resolve(self, parent: Px) -> Option<Px> {
        match self {
            Length::Auto => None,
            Length::Px(v) => Some(v),
            Length::Fraction(f) => Some(Px(parent.0 * f)),
        }
    }

    /// Resolves against a parent extent, treating `Auto` as zero.
    #[inline]
    pub fn resolve_or_zero(self, parent: Px) -> Px {
        self.resolve(parent).unwrap_or(Px::ZERO)
    }
}

impl From<Px> for Length {
    #[inline]
    fn from(v: Px) -> Self {
        Length::Px(v)
    }
}

/// `percent(50.0)` is half the parent extent.
#[inline]
pub fn percent(v: f32) -> Length {
    Length::Fraction(v / 100.0)
}

/// A relative fraction of the parent extent, where `1.0` is the whole thing.
#[inline]
pub fn relative(v: f32) -> Length {
    Length::Fraction(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_arithmetic_is_unit_preserving() {
        assert_eq!(px(2.0) + px(3.0), px(5.0));
        assert_eq!(px(6.0) * 0.5, px(3.0));
        // Dividing like units drops to a bare ratio, which is what callers want.
        assert_eq!(px(6.0) / px(3.0), 2.0);
    }

    #[test]
    fn scale_factor_rejects_degenerate_input() {
        assert_eq!(ScaleFactor::new(0.0).get(), 1.0);
        assert_eq!(ScaleFactor::new(-2.0).get(), 1.0);
        assert_eq!(ScaleFactor::new(f32::NAN).get(), 1.0);
        assert_eq!(ScaleFactor::new(1.5).get(), 1.5);
    }

    #[test]
    fn fractional_dpi_roundtrips_within_half_a_device_pixel() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let sf = ScaleFactor::new(scale);
            for logical in [0.0, 1.0, 7.5, 13.0, 100.25] {
                let d = sf.to_device(px(logical));
                let back = sf.to_logical(d);
                assert!((back.get() - logical).abs() <= 0.5 / scale + 1e-4, "{scale} {logical}");
            }
        }
    }

    #[test]
    fn vertex_space_conversion_is_not_rounded() {
        let sf = ScaleFactor::new(1.5);
        assert_eq!(sf.to_device_f32(px(1.0)), 1.5);
        // ...whereas the integral conversion is.
        assert_eq!(sf.to_device(px(1.0)), DevicePx(2));
    }

    #[test]
    fn length_resolution() {
        assert_eq!(Length::Auto.resolve(px(100.0)), None);
        assert_eq!(Length::Px(px(20.0)).resolve(px(100.0)), Some(px(20.0)));
        assert_eq!(percent(25.0).resolve(px(100.0)), Some(px(25.0)));
        assert_eq!(Length::Auto.resolve_or_zero(px(100.0)), Px::ZERO);
    }

    #[test]
    fn angle_conversion() {
        assert!((Deg(180.0).to_rad().0 - core::f32::consts::PI).abs() < 1e-6);
        assert!((Rad(core::f32::consts::PI).to_deg().0 - 180.0).abs() < 1e-4);
    }

    #[test]
    fn pt_to_px_uses_css_ratio() {
        assert!((pt(72.0).to_px().get() - 96.0).abs() < 1e-4);
    }
}
