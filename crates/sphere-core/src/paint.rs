//! Paint description: what fills a shape and what outlines it.

use crate::color::{BlendMode, Color};
use crate::geometry::{Point, Size};
use crate::id::ImageId;
use crate::unit::Px;
use smallvec::SmallVec;

/// One color stop in a gradient.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct GradientStop {
    /// Position along the gradient, `0..=1`.
    pub offset: f32,
    /// Color at this position, in authoring sRGB.
    pub color: Color,
}

impl GradientStop {
    /// Builds a stop.
    #[inline]
    pub fn new(offset: f32, color: Color) -> Self {
        Self { offset: offset.clamp(0.0, 1.0), color }
    }
}

/// The maximum number of stops carried inline before spilling to the heap.
///
/// Sixteen covers essentially every UI gradient, so the common case never
/// allocates while building a frame.
pub type Stops = SmallVec<[GradientStop; 8]>;

/// A gradient definition.
#[derive(Clone, PartialEq, Debug)]
pub enum Gradient {
    /// Interpolates along the line from `start` to `end`.
    Linear {
        /// Start point, in the shape's local space.
        start: Point<Px>,
        /// End point, in the shape's local space.
        end: Point<Px>,
        /// Color stops, sorted by offset.
        stops: Stops,
    },
    /// Interpolates outward from `center`.
    Radial {
        /// Centre, in the shape's local space.
        center: Point<Px>,
        /// Radii along each axis, allowing elliptical gradients.
        radius: Size<Px>,
        /// Color stops, sorted by offset.
        stops: Stops,
    },
    /// Interpolates around `center`, used for knob and dial arcs.
    Sweep {
        /// Centre, in the shape's local space.
        center: Point<Px>,
        /// Starting angle in turns.
        start_angle: f32,
        /// Angular extent in turns.
        sweep: f32,
        /// Color stops, sorted by offset.
        stops: Stops,
    },
}

impl Gradient {
    /// A two-stop vertical linear gradient across a box of the given height.
    pub fn vertical(height: Px, top: Color, bottom: Color) -> Self {
        Gradient::Linear {
            start: Point::new(Px::ZERO, Px::ZERO),
            end: Point::new(Px::ZERO, height),
            stops: SmallVec::from_slice(&[
                GradientStop::new(0.0, top),
                GradientStop::new(1.0, bottom),
            ]),
        }
    }

    /// A two-stop horizontal linear gradient across a box of the given width.
    pub fn horizontal(width: Px, left: Color, right: Color) -> Self {
        Gradient::Linear {
            start: Point::new(Px::ZERO, Px::ZERO),
            end: Point::new(width, Px::ZERO),
            stops: SmallVec::from_slice(&[
                GradientStop::new(0.0, left),
                GradientStop::new(1.0, right),
            ]),
        }
    }

    /// The stops backing this gradient.
    #[inline]
    pub fn stops(&self) -> &[GradientStop] {
        match self {
            Gradient::Linear { stops, .. }
            | Gradient::Radial { stops, .. }
            | Gradient::Sweep { stops, .. } => stops,
        }
    }

    /// Sorts stops by offset and clamps them into range.
    ///
    /// Unsorted stops produce garbage in the shader rather than a nice error, so
    /// the scene builder normalises on the way in.
    pub fn normalize(&mut self) {
        let stops = match self {
            Gradient::Linear { stops, .. }
            | Gradient::Radial { stops, .. }
            | Gradient::Sweep { stops, .. } => stops,
        };
        for s in stops.iter_mut() {
            s.offset = s.offset.clamp(0.0, 1.0);
        }
        stops.sort_by(|a, b| a.offset.partial_cmp(&b.offset).unwrap_or(core::cmp::Ordering::Equal));
    }

    /// Samples the gradient ramp at `t`, interpolating in linear space.
    pub fn sample(&self, t: f32) -> Color {
        let stops = self.stops();
        match stops {
            [] => Color::TRANSPARENT,
            [only] => only.color,
            _ => {
                let t = t.clamp(0.0, 1.0);
                if t <= stops[0].offset {
                    return stops[0].color;
                }
                if t >= stops[stops.len() - 1].offset {
                    return stops[stops.len() - 1].color;
                }
                for w in stops.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    if t >= a.offset && t <= b.offset {
                        let span = b.offset - a.offset;
                        let local = if span > 1e-6 { (t - a.offset) / span } else { 0.0 };
                        return a.color.lerp(b.color, local);
                    }
                }
                stops[stops.len() - 1].color
            }
        }
    }
}

/// How an image is mapped into the shape it paints.
#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub enum ImageFit {
    /// Stretch to exactly fill the destination.
    #[default]
    Fill,
    /// Scale uniformly until the image fits inside, letterboxing.
    Contain,
    /// Scale uniformly until the image covers, cropping.
    Cover,
    /// Draw at natural size, anchored at the destination's top-left.
    None,
}

/// What fills a shape.
#[derive(Clone, PartialEq, Debug)]
pub enum Brush {
    /// A single flat color.
    Solid(Color),
    /// A gradient ramp.
    Gradient(Gradient),
    /// A texture.
    Image {
        /// The texture to sample.
        image: ImageId,
        /// How to map it into the destination.
        fit: ImageFit,
        /// Multiplied over the sampled texels, for tinting.
        tint: Color,
    },
}

impl Brush {
    /// True when the brush contributes nothing to the frame.
    pub fn is_transparent(&self) -> bool {
        match self {
            Brush::Solid(c) => c.is_transparent(),
            Brush::Gradient(g) => g.stops().iter().all(|s| s.color.is_transparent()),
            Brush::Image { tint, .. } => tint.is_transparent(),
        }
    }

    /// Multiplies the brush's alpha, for opacity inheritance.
    pub fn scale_alpha(&self, factor: f32) -> Brush {
        match self {
            Brush::Solid(c) => Brush::Solid(c.scale_alpha(factor)),
            Brush::Gradient(g) => {
                let mut g = g.clone();
                let stops = match &mut g {
                    Gradient::Linear { stops, .. }
                    | Gradient::Radial { stops, .. }
                    | Gradient::Sweep { stops, .. } => stops,
                };
                for s in stops.iter_mut() {
                    s.color = s.color.scale_alpha(factor);
                }
                Brush::Gradient(g)
            }
            Brush::Image { image, fit, tint } => {
                Brush::Image { image: *image, fit: *fit, tint: tint.scale_alpha(factor) }
            }
        }
    }
}

impl From<Color> for Brush {
    #[inline]
    fn from(c: Color) -> Self {
        Brush::Solid(c)
    }
}
impl From<Gradient> for Brush {
    #[inline]
    fn from(g: Gradient) -> Self {
        Brush::Gradient(g)
    }
}

/// How a stroke terminates at an open end.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Hash)]
pub enum LineCap {
    /// Ends exactly at the endpoint.
    #[default]
    Butt,
    /// Extends by half the stroke width, rounded.
    Round,
    /// Extends by half the stroke width, squared off.
    Square,
}

/// How two stroke segments meet.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Hash)]
pub enum LineJoin {
    /// Extends the outer edges until they meet, subject to the miter limit.
    #[default]
    Miter,
    /// Fills the gap with an arc.
    Round,
    /// Fills the gap with a straight edge.
    Bevel,
}

/// A stroke style.
#[derive(Clone, PartialEq, Debug)]
pub struct Stroke {
    /// Stroke width in logical pixels.
    pub width: Px,
    /// End cap style.
    pub cap: LineCap,
    /// Corner join style.
    pub join: LineJoin,
    /// Beyond this ratio of miter length to stroke width, a miter falls back to
    /// a bevel. Without a limit, a near-180-degree corner produces a spike that
    /// can extend arbitrarily far.
    pub miter_limit: f32,
    /// Dash pattern, alternating on and off lengths. Empty means solid.
    pub dash: SmallVec<[Px; 4]>,
    /// Offset into the dash pattern, used to animate marching ants.
    pub dash_offset: Px,
}

impl Default for Stroke {
    fn default() -> Self {
        Self {
            width: Px(1.0),
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 4.0,
            dash: SmallVec::new(),
            dash_offset: Px::ZERO,
        }
    }
}

impl Stroke {
    /// A solid stroke of the given width.
    #[inline]
    pub fn new(width: Px) -> Self {
        Self { width, ..Default::default() }
    }

    /// Sets the cap style.
    #[inline]
    pub fn with_cap(mut self, cap: LineCap) -> Self {
        self.cap = cap;
        self
    }

    /// Sets the join style.
    #[inline]
    pub fn with_join(mut self, join: LineJoin) -> Self {
        self.join = join;
        self
    }

    /// Sets a dash pattern.
    #[inline]
    pub fn dashed(mut self, pattern: &[Px]) -> Self {
        self.dash = SmallVec::from_slice(pattern);
        self
    }

    /// True when the stroke is too thin to produce any coverage.
    #[inline]
    pub fn is_invisible(&self) -> bool {
        self.width <= Px::ZERO
    }
}

/// A drop or inner shadow.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Shadow {
    /// Offset from the shape.
    pub offset: Size<Px>,
    /// Gaussian blur radius. Zero produces a hard-edged copy.
    pub blur_radius: Px,
    /// Grows the shadow shape before blurring.
    pub spread: Px,
    /// Shadow color.
    pub color: Color,
    /// When true the shadow is drawn inside the shape rather than behind it.
    pub inset: bool,
}

impl Default for Shadow {
    fn default() -> Self {
        Self {
            offset: Size::new(Px::ZERO, Px(2.0)),
            blur_radius: Px(4.0),
            spread: Px::ZERO,
            color: Color::BLACK.with_alpha(0.25),
            inset: false,
        }
    }
}

impl Shadow {
    /// How far outside the source shape this shadow can reach.
    ///
    /// Invalidation and culling need this: a shape whose own bounds are
    /// offscreen can still cast a visible shadow into view.
    pub fn extent(&self) -> Px {
        // A Gaussian is truncated at three sigma in the blur pass, and the
        // shader maps `blur_radius` to `2 * sigma`, so 1.5x covers the tail.
        Px(self.blur_radius.get() * 1.5
            + self.spread.get()
            + self.offset.width.get().abs().max(self.offset.height.get().abs()))
    }
}

/// A complete paint: brush plus compositing behaviour.
#[derive(Clone, PartialEq, Debug)]
pub struct Paint {
    /// What fills the shape.
    pub brush: Brush,
    /// How it composites with the backdrop.
    pub blend: BlendMode,
    /// A final opacity multiplier applied on top of the brush's own alpha.
    pub opacity: f32,
}

impl Default for Paint {
    fn default() -> Self {
        Self { brush: Brush::Solid(Color::BLACK), blend: BlendMode::Normal, opacity: 1.0 }
    }
}

impl Paint {
    /// A flat-color paint.
    #[inline]
    pub fn solid(color: Color) -> Self {
        Self { brush: Brush::Solid(color), ..Default::default() }
    }

    /// True when the paint contributes nothing to the frame.
    #[inline]
    pub fn is_invisible(&self) -> bool {
        self.opacity <= 0.0 || self.brush.is_transparent()
    }
}

impl From<Color> for Paint {
    #[inline]
    fn from(c: Color) -> Self {
        Paint::solid(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::px;

    #[test]
    fn gradient_sampling_hits_the_endpoints_exactly() {
        let g = Gradient::vertical(px(100.0), Color::RED, Color::BLUE);
        assert_eq!(g.sample(0.0), Color::RED);
        assert_eq!(g.sample(1.0), Color::BLUE);
    }

    #[test]
    fn gradient_sampling_clamps_outside_the_range() {
        let g = Gradient::vertical(px(10.0), Color::RED, Color::BLUE);
        assert_eq!(g.sample(-5.0), Color::RED);
        assert_eq!(g.sample(5.0), Color::BLUE);
    }

    #[test]
    fn normalize_sorts_unsorted_stops() {
        let mut g = Gradient::Linear {
            start: Point::ZERO,
            end: Point::new(px(10.0), Px::ZERO),
            stops: SmallVec::from_slice(&[
                GradientStop::new(1.0, Color::BLUE),
                GradientStop::new(0.0, Color::RED),
                GradientStop::new(0.5, Color::GREEN),
            ]),
        };
        g.normalize();
        let offsets: Vec<f32> = g.stops().iter().map(|s| s.offset).collect();
        assert_eq!(offsets, vec![0.0, 0.5, 1.0]);
        assert_eq!(g.sample(0.0), Color::RED);
    }

    #[test]
    fn single_stop_gradient_is_flat() {
        let g = Gradient::Linear {
            start: Point::ZERO,
            end: Point::new(px(10.0), Px::ZERO),
            stops: SmallVec::from_slice(&[GradientStop::new(0.3, Color::GREEN)]),
        };
        assert_eq!(g.sample(0.0), Color::GREEN);
        assert_eq!(g.sample(1.0), Color::GREEN);
    }

    #[test]
    fn coincident_stops_do_not_divide_by_zero() {
        let g = Gradient::Linear {
            start: Point::ZERO,
            end: Point::new(px(10.0), Px::ZERO),
            stops: SmallVec::from_slice(&[
                GradientStop::new(0.5, Color::RED),
                GradientStop::new(0.5, Color::BLUE),
            ]),
        };
        let c = g.sample(0.5);
        assert!(c.r.is_finite() && c.g.is_finite() && c.b.is_finite());
    }

    #[test]
    fn scale_alpha_reaches_every_gradient_stop() {
        let b = Brush::Gradient(Gradient::vertical(px(10.0), Color::RED, Color::BLUE));
        let faded = b.scale_alpha(0.5);
        match faded {
            Brush::Gradient(g) => {
                assert!(g.stops().iter().all(|s| (s.color.a - 0.5).abs() < 1e-6));
            }
            _ => panic!("expected gradient"),
        }
    }

    #[test]
    fn fully_faded_brush_is_reported_transparent() {
        assert!(Brush::Solid(Color::RED).scale_alpha(0.0).is_transparent());
        assert!(
            Brush::Gradient(Gradient::vertical(px(10.0), Color::RED, Color::BLUE))
                .scale_alpha(0.0)
                .is_transparent()
        );
    }

    #[test]
    fn shadow_extent_accounts_for_blur_spread_and_offset() {
        let s = Shadow {
            offset: Size::new(px(0.0), px(10.0)),
            blur_radius: px(20.0),
            spread: px(5.0),
            ..Default::default()
        };
        // Must cover at least the offset plus the blur tail.
        assert!(s.extent().get() >= 10.0 + 20.0);
    }

    #[test]
    fn zero_width_stroke_is_invisible() {
        assert!(Stroke::new(Px::ZERO).is_invisible());
        assert!(!Stroke::new(px(0.5)).is_invisible());
    }
}
