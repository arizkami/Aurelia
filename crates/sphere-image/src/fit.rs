//! Resolving [`ImageFit`] into concrete source and destination rectangles.
//!
//! This is the single place in the engine where "draw this image into that box"
//! becomes numbers. `sphere-render` defers to it rather than reimplementing the
//! arithmetic per primitive, because the four modes differ in *which* rectangle
//! they shrink (the destination for `Contain`, the source for `Cover`) and
//! getting that backwards produces a subtly wrong crop that survives review.

use sphere_core::{ImageFit, Point, Px, Rect, Size};

/// The pair of rectangles that describe one image draw.
///
/// `source` is expressed in the image's own texel space: origin at the top-left
/// texel, one unit per texel, so a 100x50 image spans `(0, 0)..(100, 50)`. It is
/// deliberately not normalised, because callers routinely need texel-space
/// values (to pick a mip level, to snap a crop to whole texels) and normalising
/// early throws the natural size away. [`FitRects::source_uv`] does the division
/// when a sampler needs `0..1`.
///
/// `dest` is in the same space as the destination rectangle that was passed in,
/// which for the UI layer is logical pixels.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct FitRects {
    /// The region of the image to sample, in texel space.
    pub source: Rect<Px>,
    /// Where those texels land, in destination space.
    pub dest: Rect<Px>,
}

impl FitRects {
    /// Nothing to draw, anchored at `origin` so callers can still report a
    /// position for an empty result.
    #[inline]
    fn empty_at(origin: Point<Px>) -> Self {
        Self { source: Rect::ZERO, dest: Rect::new(origin, Size::ZERO) }
    }

    /// True when the draw contributes no pixels and can be skipped entirely.
    ///
    /// Worth checking before building a draw call: a collapsed splitter pane or
    /// a zero-size image both land here, and both would otherwise cost a
    /// pipeline bind for nothing.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.source.is_empty() || self.dest.is_empty()
    }

    /// The source rectangle as normalised texture coordinates in `0..1`.
    ///
    /// Returns [`Rect::ZERO`] for a degenerate natural size rather than
    /// producing NaN, which would poison a vertex buffer and take the whole
    /// draw call with it. "Degenerate" includes a non-finite extent: `NaN` is
    /// not caught by an `is_empty` comparison, because every comparison against
    /// `NaN` is false, so it has to be excluded explicitly.
    pub fn source_uv(&self, natural: Size<Px>) -> Rect<f32> {
        if natural.is_empty() || !is_finite(natural) || !rect_is_finite(self.source) {
            return Rect::ZERO;
        }
        Rect::new(
            Point::new(self.source.min_x() / natural.width, self.source.min_y() / natural.height),
            Size::new(self.source.width() / natural.width, self.source.height() / natural.height),
        )
    }

    /// The uniform scale applied along each axis, `dest / source`.
    ///
    /// The renderer uses this to choose a mip level: a ratio well under one
    /// means the image is being minified and needs a downscaled level, which is
    /// the difference between a clean thumbnail and an aliasing mess.
    pub fn scale(&self) -> (f32, f32) {
        if self.source.is_empty() || !rect_is_finite(self.source) || !rect_is_finite(self.dest) {
            return (0.0, 0.0);
        }
        (self.dest.width() / self.source.width(), self.dest.height() / self.source.height())
    }
}

/// Resolves an [`ImageFit`] against a natural size and a destination rectangle.
///
/// - [`ImageFit::Fill`] stretches: the whole image covers the whole destination,
///   aspect ratio be damned.
/// - [`ImageFit::Contain`] scales uniformly until the image fits inside and
///   centres the result, leaving letterbox bars along the slack axis.
/// - [`ImageFit::Cover`] scales uniformly until the image covers the whole
///   destination and centres the *crop*, so the destination is filled and part
///   of the image is not sampled.
/// - [`ImageFit::None`] draws at natural size anchored to the destination's
///   top-left, clipping rather than scaling when the image is larger.
///
/// A degenerate natural size or destination yields an empty result rather than a
/// division by zero: a zero-width panel is an ordinary transient state during a
/// drag, not an error. A non-finite input is degenerate too, and is rejected
/// explicitly: `NaN <= 0.0` is false, so an `is_empty` test alone lets a `NaN`
/// straight through, and every rectangle downstream of it becomes `NaN`. A
/// layout that divides by a collapsed track produces exactly that, and the
/// symptom is a whole draw call vanishing rather than an obviously bad number.
pub fn resolve(fit: ImageFit, natural: Size<Px>, dest: Rect<Px>) -> FitRects {
    if natural.is_empty() || dest.is_empty() || !is_finite(natural) || !rect_is_finite(dest) {
        let origin = if dest.origin.x.0.is_finite() && dest.origin.y.0.is_finite() {
            dest.origin
        } else {
            Point::ZERO
        };
        return FitRects::empty_at(origin);
    }
    let full_source = Rect::new(Point::ZERO, natural);
    match fit {
        ImageFit::Fill => FitRects { source: full_source, dest },
        ImageFit::Contain => {
            let scale = (dest.width() / natural.width).min(dest.height() / natural.height);
            let scaled = Size::new(natural.width * scale, natural.height * scale);
            FitRects { source: full_source, dest: centre_in(scaled, dest) }
        }
        ImageFit::Cover => {
            let scale = (dest.width() / natural.width).max(dest.height() / natural.height);
            // Invert the scale to find how much of the source is visible. The
            // scale is strictly positive here because both extents are non-empty.
            let visible = Size::new(dest.width() / scale, dest.height() / scale);
            let visible = visible.min(natural);
            let source = centre_in(visible, full_source);
            FitRects { source, dest }
        }
        ImageFit::None => {
            let shown = natural.min(dest.size);
            FitRects { source: Rect::new(Point::ZERO, shown), dest: Rect::new(dest.origin, shown) }
        }
    }
}

/// True when both extents are finite.
#[inline]
fn is_finite(size: Size<Px>) -> bool {
    size.width.0.is_finite() && size.height.0.is_finite()
}

/// True when a rectangle's origin and extents are all finite.
#[inline]
fn rect_is_finite(r: Rect<Px>) -> bool {
    r.origin.x.0.is_finite() && r.origin.y.0.is_finite() && is_finite(r.size)
}

/// Centres `inner` inside `outer`.
///
/// Uses the outer rectangle's own origin rather than assuming zero, so a
/// letterboxed image inside a panel at `(300, 40)` lands where it should.
#[inline]
fn centre_in(inner: Size<Px>, outer: Rect<Px>) -> Rect<Px> {
    let origin = Point::new(
        outer.min_x() + (outer.width() - inner.width) * 0.5,
        outer.min_y() + (outer.height() - inner.height) * 0.5,
    );
    Rect::new(origin, inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_core::{px, rect, size};

    /// The task's reference case: a 100x50 image into a 200x200 box.
    fn wide_into_square(fit: ImageFit) -> FitRects {
        resolve(fit, size(px(100.0), px(50.0)), rect(px(0.0), px(0.0), px(200.0), px(200.0)))
    }

    #[test]
    fn fill_stretches_both_axes() {
        let r = wide_into_square(ImageFit::Fill);
        assert_eq!(r.source, rect(px(0.0), px(0.0), px(100.0), px(50.0)));
        assert_eq!(r.dest, rect(px(0.0), px(0.0), px(200.0), px(200.0)));
        // The aspect ratio is not preserved; that is the whole point of Fill.
        assert_eq!(r.scale(), (2.0, 4.0));
    }

    #[test]
    fn contain_letterboxes_on_the_slack_axis() {
        let r = wide_into_square(ImageFit::Contain);
        // scale = min(200/100, 200/50) = 2, so 200x100 centred vertically.
        assert_eq!(r.source, rect(px(0.0), px(0.0), px(100.0), px(50.0)));
        assert_eq!(r.dest, rect(px(0.0), px(50.0), px(200.0), px(100.0)));
        assert_eq!(r.scale(), (2.0, 2.0), "Contain must be uniform");
    }

    #[test]
    fn cover_crops_the_source_and_fills_the_destination() {
        let r = wide_into_square(ImageFit::Cover);
        // scale = max(2, 4) = 4, so only 200/4 = 50 texels of width are visible,
        // centred: (100 - 50) / 2 = 25.
        assert_eq!(r.source, rect(px(25.0), px(0.0), px(50.0), px(50.0)));
        assert_eq!(r.dest, rect(px(0.0), px(0.0), px(200.0), px(200.0)));
        assert_eq!(r.scale(), (4.0, 4.0), "Cover must be uniform");
    }

    #[test]
    fn none_anchors_at_the_top_left_at_natural_size() {
        let r = wide_into_square(ImageFit::None);
        assert_eq!(r.source, rect(px(0.0), px(0.0), px(100.0), px(50.0)));
        assert_eq!(r.dest, rect(px(0.0), px(0.0), px(100.0), px(50.0)));
        assert_eq!(r.scale(), (1.0, 1.0), "None must never scale");
    }

    #[test]
    fn none_clips_rather_than_scaling_when_the_image_is_larger() {
        let r = resolve(
            ImageFit::None,
            size(px(300.0), px(200.0)),
            rect(px(10.0), px(20.0), px(100.0), px(50.0)),
        );
        assert_eq!(r.source, rect(px(0.0), px(0.0), px(100.0), px(50.0)));
        assert_eq!(r.dest, rect(px(10.0), px(20.0), px(100.0), px(50.0)));
        assert_eq!(r.scale(), (1.0, 1.0));
    }

    #[test]
    fn a_tall_destination_flips_which_axis_is_cropped() {
        // The mirror of the reference case: 50x100 into 200x200.
        let natural = size(px(50.0), px(100.0));
        let dest = rect(px(0.0), px(0.0), px(200.0), px(200.0));
        let contain = resolve(ImageFit::Contain, natural, dest);
        assert_eq!(contain.dest, rect(px(50.0), px(0.0), px(100.0), px(200.0)));
        let cover = resolve(ImageFit::Cover, natural, dest);
        assert_eq!(cover.source, rect(px(0.0), px(25.0), px(50.0), px(50.0)));
        assert_eq!(cover.dest, dest);
    }

    #[test]
    fn destination_origin_is_respected_by_every_mode() {
        let natural = size(px(100.0), px(50.0));
        let dest = rect(px(300.0), px(40.0), px(200.0), px(200.0));
        assert_eq!(resolve(ImageFit::Fill, natural, dest).dest, dest);
        assert_eq!(
            resolve(ImageFit::Contain, natural, dest).dest,
            rect(px(300.0), px(90.0), px(200.0), px(100.0))
        );
        assert_eq!(resolve(ImageFit::Cover, natural, dest).dest, dest);
        assert_eq!(
            resolve(ImageFit::None, natural, dest).dest,
            rect(px(300.0), px(40.0), px(100.0), px(50.0))
        );
    }

    #[test]
    fn matching_aspect_ratios_make_contain_and_cover_agree() {
        // 100x50 into 400x200: both modes should give the identical full-bleed
        // result, with no letterbox and no crop.
        let natural = size(px(100.0), px(50.0));
        let dest = rect(px(0.0), px(0.0), px(400.0), px(200.0));
        let contain = resolve(ImageFit::Contain, natural, dest);
        let cover = resolve(ImageFit::Cover, natural, dest);
        assert_eq!(contain, cover);
        assert_eq!(contain.dest, dest);
        assert_eq!(contain.source, rect(px(0.0), px(0.0), px(100.0), px(50.0)));
    }

    #[test]
    fn zero_size_destinations_produce_nothing_and_never_nan() {
        let natural = size(px(100.0), px(50.0));
        for dest in [
            rect(px(5.0), px(6.0), px(0.0), px(200.0)),
            rect(px(5.0), px(6.0), px(200.0), px(0.0)),
            rect(px(5.0), px(6.0), px(0.0), px(0.0)),
            // A negative extent, which a bad layout can produce.
            rect(px(5.0), px(6.0), px(-10.0), px(200.0)),
        ] {
            for fit in [ImageFit::Fill, ImageFit::Contain, ImageFit::Cover, ImageFit::None] {
                let r = resolve(fit, natural, dest);
                assert!(r.is_empty(), "{fit:?} on {dest:?} was not empty");
                assert!(r.dest.width().is_finite() && r.dest.height().is_finite());
                assert!(r.source.width().is_finite() && r.source.height().is_finite());
                assert_eq!(r.dest.origin, dest.origin, "position should survive");
            }
        }
    }

    #[test]
    fn non_finite_input_produces_nothing_rather_than_a_nan_rectangle() {
        // `NaN <= 0.0` is false, so an is_empty test alone waves NaN through and
        // every downstream rectangle becomes NaN: a poisoned vertex buffer and a
        // silently missing draw. A layout dividing by a collapsed track is a
        // realistic source, so this must be a value, not a surprise.
        let good_natural = size(px(100.0), px(50.0));
        let good_dest = rect(px(0.0), px(0.0), px(200.0), px(200.0));
        let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

        for v in bad {
            let cases = [
                (size(px(v), px(50.0)), good_dest),
                (size(px(100.0), px(v)), good_dest),
                (good_natural, rect(px(0.0), px(0.0), px(v), px(200.0))),
                (good_natural, rect(px(0.0), px(0.0), px(200.0), px(v))),
                (good_natural, rect(px(v), px(0.0), px(200.0), px(200.0))),
                (good_natural, rect(px(0.0), px(v), px(200.0), px(200.0))),
            ];
            for (natural, dest) in cases {
                for fit in [ImageFit::Fill, ImageFit::Contain, ImageFit::Cover, ImageFit::None] {
                    let r = resolve(fit, natural, dest);
                    assert!(r.is_empty(), "{fit:?} {natural:?} {dest:?} was not empty");
                    for extent in [
                        r.dest.width().0,
                        r.dest.height().0,
                        r.source.width().0,
                        r.source.height().0,
                        r.dest.origin.x.0,
                        r.dest.origin.y.0,
                        r.source.origin.x.0,
                        r.source.origin.y.0,
                    ] {
                        assert!(extent.is_finite(), "{fit:?} leaked {extent} into a rectangle");
                    }
                    let uv = r.source_uv(natural);
                    assert!(
                        uv.origin.x.is_finite()
                            && uv.origin.y.is_finite()
                            && uv.width().is_finite()
                            && uv.height().is_finite(),
                        "{fit:?} leaked a non-finite uv: {uv:?}"
                    );
                    let (sx, sy) = r.scale();
                    assert!(sx.is_finite() && sy.is_finite(), "{fit:?} leaked a non-finite scale");
                }
            }
        }
    }

    #[test]
    fn zero_size_images_produce_nothing() {
        let dest = rect(px(0.0), px(0.0), px(200.0), px(200.0));
        for natural in [size(px(0.0), px(0.0)), size(px(0.0), px(50.0)), size(px(100.0), px(0.0))] {
            for fit in [ImageFit::Fill, ImageFit::Contain, ImageFit::Cover, ImageFit::None] {
                assert!(resolve(fit, natural, dest).is_empty(), "{fit:?} {natural:?}");
            }
        }
    }

    #[test]
    fn source_uv_normalises_the_crop() {
        let natural = size(px(100.0), px(50.0));
        let r = resolve(ImageFit::Cover, natural, rect(px(0.0), px(0.0), px(200.0), px(200.0)));
        let uv = r.source_uv(natural);
        assert_eq!(uv, sphere_core::rect(0.25, 0.0, 0.5, 1.0));
    }

    #[test]
    fn source_uv_of_a_degenerate_size_is_zero_not_nan() {
        let r = FitRects { source: rect(px(0.0), px(0.0), px(10.0), px(10.0)), dest: Rect::ZERO };
        let uv = r.source_uv(Size::ZERO);
        assert_eq!(uv, Rect::ZERO);
        assert!(uv.width().is_finite());
    }

    #[test]
    fn cover_never_samples_outside_the_image() {
        // Float division can nudge the visible extent a hair past the natural
        // size; clamping is what keeps the sampler off the edge texels.
        for w in [1.0f32, 3.0, 17.0, 1000.0, 4001.0] {
            for h in [1.0f32, 7.0, 999.0] {
                let natural = size(px(w), px(h));
                let dest = rect(px(0.0), px(0.0), px(333.0), px(97.0));
                let r = resolve(ImageFit::Cover, natural, dest);
                assert!(r.source.min_x() >= px(0.0), "{w}x{h}");
                assert!(r.source.min_y() >= px(0.0), "{w}x{h}");
                assert!(r.source.max_x() <= px(w) + px(1e-3), "{w}x{h}");
                assert!(r.source.max_y() <= px(h) + px(1e-3), "{w}x{h}");
            }
        }
    }

    #[test]
    fn contain_never_overflows_the_destination() {
        for w in [1.0f32, 13.0, 640.0, 4000.0] {
            for h in [1.0f32, 11.0, 480.0, 3000.0] {
                let natural = size(px(w), px(h));
                let dest = rect(px(12.0), px(9.0), px(257.0), px(131.0));
                let r = resolve(ImageFit::Contain, natural, dest);
                assert!(r.dest.min_x() >= dest.min_x() - px(1e-3), "{w}x{h}");
                assert!(r.dest.max_x() <= dest.max_x() + px(1e-3), "{w}x{h}");
                assert!(r.dest.min_y() >= dest.min_y() - px(1e-3), "{w}x{h}");
                assert!(r.dest.max_y() <= dest.max_y() + px(1e-3), "{w}x{h}");
                // ...and one axis must touch the edge exactly, or it is not a fit.
                let touches_x = (r.dest.width() - dest.width()).abs() < px(1e-3);
                let touches_y = (r.dest.height() - dest.height()).abs() < px(1e-3);
                assert!(touches_x || touches_y, "{w}x{h} left slack on both axes");
            }
        }
    }
}
