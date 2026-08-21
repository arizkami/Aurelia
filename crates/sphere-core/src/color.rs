//! Color, with an explicit sRGB / linear split.
//!
//! Sphere keeps three color representations apart on purpose:
//!
//! * [`Color`] — authoring-side, sRGB-encoded, straight (non-premultiplied) alpha.
//!   This is what application code writes.
//! * [`LinearColor`] — linear-light, premultiplied alpha. This is what goes into
//!   GPU buffers, because correct blending is a linear-space operation and
//!   premultiplied alpha is what avoids dark fringes on filtered edges.
//! * [`Hsla`] — an authoring convenience for themes and generated palettes.
//!
//! Blending sRGB values directly is the single most common source of muddy
//! gradients and grey halos around text, so the conversion is not optional and
//! not implicit.

use crate::unit::Scalar;

/// An sRGB-encoded color with straight alpha, in the `0..=1` range.
#[derive(Copy, Clone, PartialEq, Debug, Default)]
#[repr(C)]
pub struct Color {
    /// Red, sRGB-encoded.
    pub r: f32,
    /// Green, sRGB-encoded.
    pub g: f32,
    /// Blue, sRGB-encoded.
    pub b: f32,
    /// Alpha, always linear.
    pub a: f32,
}

impl Color {
    /// Fully transparent.
    pub const TRANSPARENT: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    /// Opaque black.
    pub const BLACK: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    /// Opaque white.
    pub const WHITE: Self = Self { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    /// Opaque red, useful as a debug color.
    pub const RED: Self = Self { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
    /// Opaque green.
    pub const GREEN: Self = Self { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
    /// Opaque blue.
    pub const BLUE: Self = Self { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };

    /// Builds an opaque color from sRGB components.
    #[inline]
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// Builds a color from sRGB components and straight alpha.
    #[inline]
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Builds from 8-bit sRGB components.
    #[inline]
    pub fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: a as f32 / 255.0 }
    }

    /// Parses `0xRRGGBB`, opaque.
    #[inline]
    pub fn hex(v: u32) -> Self {
        Self::rgba8((v >> 16) as u8, (v >> 8) as u8, v as u8, 255)
    }

    /// Parses `0xRRGGBBAA`.
    #[inline]
    pub fn hex_rgba(v: u32) -> Self {
        Self::rgba8((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// Returns the color with a different alpha.
    #[inline]
    pub fn with_alpha(self, a: f32) -> Self {
        Self { a: a.clamp(0.0, 1.0), ..self }
    }

    /// Multiplies the existing alpha, for opacity layers.
    #[inline]
    pub fn scale_alpha(self, factor: f32) -> Self {
        Self { a: (self.a * factor).clamp(0.0, 1.0), ..self }
    }

    /// True when the color contributes nothing, so the primitive can be dropped
    /// before it ever reaches a vertex buffer.
    #[inline]
    pub fn is_transparent(self) -> bool {
        self.a <= 0.0
    }

    /// True when the color fully covers what is behind it, which lets the batch
    /// compiler skip blending.
    #[inline]
    pub fn is_opaque(self) -> bool {
        self.a >= 1.0
    }

    /// Converts to linear-light premultiplied form for GPU submission.
    #[inline]
    pub fn to_linear(self) -> LinearColor {
        let a = self.a.clamp(0.0, 1.0);
        LinearColor {
            r: srgb_to_linear(self.r) * a,
            g: srgb_to_linear(self.g) * a,
            b: srgb_to_linear(self.b) * a,
            a,
        }
    }

    /// Mixes two colors in linear space.
    ///
    /// Interpolating sRGB values directly is what makes a red-to-green gradient
    /// pass through a muddy brown, so this deliberately round-trips.
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let (a, b) = (self.to_linear().unpremultiply(), other.to_linear().unpremultiply());
        let mix = [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
            a[3] + (b[3] - a[3]) * t,
        ];
        Self {
            r: linear_to_srgb(mix[0]),
            g: linear_to_srgb(mix[1]),
            b: linear_to_srgb(mix[2]),
            a: mix[3],
        }
    }

    /// Relative luminance per WCAG 2.x, used to pick readable foregrounds.
    #[inline]
    pub fn luminance(self) -> f32 {
        0.2126 * srgb_to_linear(self.r)
            + 0.7152 * srgb_to_linear(self.g)
            + 0.0722 * srgb_to_linear(self.b)
    }

    /// Packs to 8-bit sRGB, the format used by image encoders and debug output.
    #[inline]
    pub fn to_rgba8(self) -> [u8; 4] {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        [q(self.r), q(self.g), q(self.b), q(self.a)]
    }
}

/// A linear-light color with premultiplied alpha, ready for the GPU.
#[derive(Copy, Clone, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct LinearColor {
    /// Red, linear, premultiplied.
    pub r: f32,
    /// Green, linear, premultiplied.
    pub g: f32,
    /// Blue, linear, premultiplied.
    pub b: f32,
    /// Alpha.
    pub a: f32,
}

impl LinearColor {
    /// Fully transparent.
    pub const TRANSPARENT: Self = Self { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    /// The four channels as an array, the layout a WGSL `vec4<f32>` expects.
    #[inline]
    pub const fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// Recovers straight-alpha linear components.
    #[inline]
    pub fn unpremultiply(self) -> [f32; 4] {
        if self.a <= 0.0 {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            [self.r / self.a, self.g / self.a, self.b / self.a, self.a]
        }
    }

    /// Converts back to authoring-side sRGB.
    #[inline]
    pub fn to_srgb(self) -> Color {
        let [r, g, b, a] = self.unpremultiply();
        Color { r: linear_to_srgb(r), g: linear_to_srgb(g), b: linear_to_srgb(b), a }
    }
}

impl From<Color> for LinearColor {
    #[inline]
    fn from(c: Color) -> Self {
        c.to_linear()
    }
}

/// Decodes one sRGB channel to linear light.
#[inline]
pub fn srgb_to_linear(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// Encodes one linear-light channel as sRGB.
#[inline]
pub fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// An HSLA color, for theme authoring.
///
/// Hue is in turns (`0.0..=1.0`) rather than degrees so that arithmetic on it
/// wraps naturally with `fract()`.
#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub struct Hsla {
    /// Hue, in turns.
    pub h: f32,
    /// Saturation, `0..=1`.
    pub s: f32,
    /// Lightness, `0..=1`.
    pub l: f32,
    /// Alpha, `0..=1`.
    pub a: f32,
}

/// Shorthand constructor for [`Hsla`].
#[inline]
pub fn hsla(h: f32, s: f32, l: f32, a: f32) -> Hsla {
    Hsla { h, s, l, a }
}

impl Hsla {
    /// Converts to sRGB.
    pub fn to_color(self) -> Color {
        let h = self.h.rem_euclid(1.0);
        let s = self.s.clamp(0.0, 1.0);
        let l = self.l.clamp(0.0, 1.0);

        if s <= 0.0 {
            return Color { r: l, g: l, b: l, a: self.a };
        }
        let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
        let p = 2.0 * l - q;
        Color {
            r: hue_to_rgb(p, q, h + 1.0 / 3.0),
            g: hue_to_rgb(p, q, h),
            b: hue_to_rgb(p, q, h - 1.0 / 3.0),
            a: self.a,
        }
    }

    /// Shifts lightness, clamped, for hover and pressed states.
    #[inline]
    pub fn lighten(self, amount: f32) -> Self {
        Self { l: (self.l + amount).clamp(0.0, 1.0), ..self }
    }

    /// Shifts lightness downward, clamped.
    #[inline]
    pub fn darken(self, amount: f32) -> Self {
        self.lighten(-amount)
    }
}

fn hue_to_rgb(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 0.5 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

impl From<Hsla> for Color {
    #[inline]
    fn from(v: Hsla) -> Self {
        v.to_color()
    }
}

/// How a primitive combines with what is already in the target.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Hash)]
pub enum BlendMode {
    /// Standard source-over compositing.
    #[default]
    Normal,
    /// Additive, used for glow and spectrum overlays.
    Add,
    /// Multiplicative darkening.
    Multiply,
    /// Inverse-multiply lightening.
    Screen,
    /// Multiply or screen depending on the backdrop.
    Overlay,
    /// Keeps the darker of the two.
    Darken,
    /// Keeps the lighter of the two.
    Lighten,
    /// Absolute per-channel difference.
    Difference,
    /// Replaces the destination entirely.
    Copy,
}

impl BlendMode {
    /// True when the mode maps onto a fixed-function blend state, so the
    /// primitive can stay in the main batch instead of forcing a layer.
    ///
    /// The separable-but-not-fixed-function modes need to read the backdrop,
    /// which on WebGPU means an offscreen pass.
    #[inline]
    pub fn is_fixed_function(self) -> bool {
        matches!(self, BlendMode::Normal | BlendMode::Add | BlendMode::Copy)
    }
}

/// Scalar impl so gradient stop offsets can share geometry helpers.
impl Scalar for Color {
    const ZERO: Self = Self::TRANSPARENT;
    fn half(self) -> Self {
        self.scale_alpha(0.5)
    }
}

impl core::ops::Add for Color {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self { r: self.r + o.r, g: self.g + o.g, b: self.b + o.b, a: self.a + o.a }
    }
}
impl core::ops::Sub for Color {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self { r: self.r - o.r, g: self.g - o.g, b: self.b - o.b, a: self.a - o.a }
    }
}
impl core::ops::Neg for Color {
    type Output = Self;
    fn neg(self) -> Self {
        Self { r: -self.r, g: -self.g, b: -self.b, a: -self.a }
    }
}
impl PartialOrd for Color {
    fn partial_cmp(&self, o: &Self) -> Option<core::cmp::Ordering> {
        self.a.partial_cmp(&o.a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_linear_roundtrip() {
        for i in 0..=100 {
            let v = i as f32 / 100.0;
            assert!((linear_to_srgb(srgb_to_linear(v)) - v).abs() < 1e-4, "{v}");
        }
    }

    #[test]
    fn srgb_transfer_matches_the_spec_at_known_points() {
        // The piecewise breakpoint and midpoint are where a wrong constant shows.
        assert!((srgb_to_linear(0.0) - 0.0).abs() < 1e-6);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
        assert!((srgb_to_linear(0.5) - 0.2140).abs() < 1e-3, "{}", srgb_to_linear(0.5));
    }

    #[test]
    fn to_linear_premultiplies() {
        let half = Color::WHITE.with_alpha(0.5).to_linear();
        assert!((half.r - 0.5).abs() < 1e-5, "{half:?}");
        assert!((half.a - 0.5).abs() < 1e-5);
    }

    #[test]
    fn premultiply_roundtrips_through_srgb() {
        let c = Color::rgba(0.25, 0.5, 0.75, 0.6);
        let back = c.to_linear().to_srgb();
        assert!((back.r - c.r).abs() < 1e-3, "{back:?}");
        assert!((back.g - c.g).abs() < 1e-3);
        assert!((back.b - c.b).abs() < 1e-3);
        assert!((back.a - c.a).abs() < 1e-5);
    }

    #[test]
    fn fully_transparent_unpremultiply_does_not_divide_by_zero() {
        assert_eq!(LinearColor::TRANSPARENT.unpremultiply(), [0.0; 4]);
    }

    #[test]
    fn lerp_happens_in_linear_space() {
        // Halfway between black and white in linear space is ~0.5 linear, which
        // is ~0.735 in sRGB, NOT 0.5. If this reads 0.5 we are lerping sRGB.
        let mid = Color::BLACK.lerp(Color::WHITE, 0.5);
        assert!((mid.r - 0.7354).abs() < 1e-2, "got {}", mid.r);
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(Color::hex(0xFF0000).to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(Color::hex_rgba(0x00FF0080).to_rgba8(), [0, 255, 0, 128]);
    }

    #[test]
    fn hsla_primaries() {
        assert_eq!(hsla(0.0, 1.0, 0.5, 1.0).to_color().to_rgba8(), [255, 0, 0, 255]);
        let g = hsla(1.0 / 3.0, 1.0, 0.5, 1.0).to_color().to_rgba8();
        assert_eq!(g, [0, 255, 0, 255]);
        let b = hsla(2.0 / 3.0, 1.0, 0.5, 1.0).to_color().to_rgba8();
        assert_eq!(b, [0, 0, 255, 255]);
    }

    #[test]
    fn hsla_hue_wraps() {
        assert_eq!(hsla(1.25, 1.0, 0.5, 1.0).to_color(), hsla(0.25, 1.0, 0.5, 1.0).to_color());
        assert_eq!(hsla(-0.75, 1.0, 0.5, 1.0).to_color(), hsla(0.25, 1.0, 0.5, 1.0).to_color());
    }

    #[test]
    fn zero_saturation_is_grey() {
        let c = hsla(0.42, 0.0, 0.3, 1.0).to_color();
        assert_eq!(c.r, c.g);
        assert_eq!(c.g, c.b);
    }

    #[test]
    fn luminance_ordering() {
        assert!(Color::WHITE.luminance() > Color::GREEN.luminance());
        assert!(Color::GREEN.luminance() > Color::RED.luminance());
        assert!(Color::RED.luminance() > Color::BLUE.luminance());
        assert!(Color::BLACK.luminance() < 1e-6);
    }

    #[test]
    fn fixed_function_blend_modes_are_classified() {
        assert!(BlendMode::Normal.is_fixed_function());
        assert!(BlendMode::Add.is_fixed_function());
        assert!(!BlendMode::Overlay.is_fixed_function());
        assert!(!BlendMode::Multiply.is_fixed_function());
    }
}
