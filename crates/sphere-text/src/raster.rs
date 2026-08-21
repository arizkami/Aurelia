//! Grayscale coverage rasterisation, the small-text fallback to MTSDF.
//!
//! ## Why a second path exists at all
//!
//! A distance field is sampled, and sampling has a resolution. Below roughly a
//! dozen device pixels per em a stem is barely one texel wide in the field, so
//! the reconstructed edge wanders by a noticeable fraction of a pixel from glyph
//! to glyph and the text shimmers when the view scrolls. At those sizes the
//! cheapest fix is also the oldest one: rasterise the outline directly at the
//! exact pixel size and cache the bitmap.
//!
//! ## The algorithm
//!
//! Analytic coverage accumulation, the approach `font-rs` popularised. Each
//! flattened line deposits its *signed area contribution* into the cells it
//! passes through; a prefix sum along each scanline then turns those local
//! contributions into total coverage. It is exact for polygons — the coverage of
//! a pixel is the true area of the polygon inside it, not an estimate from N
//! samples — so no supersampling is needed and cost is proportional to the
//! outline's length rather than to the pixel count times a sample count.
//!
//! Curves are flattened first. Coverage is exact for the *polygon* it is given,
//! so flattening error is the only error in the output, and it converts directly
//! into coverage error at the boundary.

use crate::mtsdf::{Shape, extract_shape};
use crate::types::{GlyphFormat, GlyphImage};
use sphere_core::{FontError, GlyphId, Point, Px, Rect, ScaleFactor, Size};

/// Physical pixel size below which the grayscale fallback wins.
///
/// Twelve device pixels per em is where a distance field stops having enough
/// texels to place a stem edge consistently: at 12 px an `l` stem is a little
/// over one pixel wide, and the field's reconstruction error is a visible
/// fraction of that. Above it MTSDF is both sharper and cheaper, since one field
/// serves every size while bitmaps must be re-rasterised per size and per scale
/// factor.
///
/// It is a threshold on *device* pixels deliberately: 11 logical pixels on a
/// 200 % display is 22 physical pixels and wants the distance field, while 11
/// logical pixels at 100 % wants the bitmap. Deciding on logical size would get
/// exactly one of those two cases wrong.
pub const BITMAP_MAX_DEVICE_PX: f32 = 12.0;

/// Curve flattening tolerance in device pixels.
///
/// A chord that sits `t` pixels off the true curve displaces the edge by `t`,
/// which is `255 * t` coverage levels in the pixel it crosses. A fiftieth of a
/// pixel therefore caps the error at five levels out of 255 — below what a
/// gamma-corrected blend can show — while costing only about twice as many
/// segments as the sloppier tenth-of-a-pixel tolerance rasterisers often use.
const FLATTEN_TOLERANCE_PX: f32 = 0.02;

/// Which rasterisation path a glyph should take.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RasterStrategy {
    /// The size-independent multi-channel distance field.
    Mtsdf,
    /// A grayscale bitmap rasterised for one exact device size.
    Bitmap,
}

/// The em size in physical pixels, which is what every decision here keys off.
#[inline]
pub fn device_font_size(font_size: Px, scale: ScaleFactor) -> f32 {
    (font_size.get() * scale.get()).max(0.0)
}

/// Chooses between the distance field and the bitmap fallback.
///
/// This is the policy behind `TextRasterMode::Auto`; callers that have been told
/// `Mtsdf` or `Bitmap` explicitly should not consult it. It lives here rather
/// than in the renderer because the threshold is a property of what the
/// rasteriser can deliver, not of how a scene was recorded.
#[inline]
pub fn choose_raster_strategy(font_size: Px, scale: ScaleFactor) -> RasterStrategy {
    if device_font_size(font_size, scale) < BITMAP_MAX_DEVICE_PX {
        RasterStrategy::Bitmap
    } else {
        RasterStrategy::Mtsdf
    }
}

/// The rounded device pixel size a bitmap glyph should be rasterised at.
///
/// Rounded, because the whole point of the fallback is landing on the pixel
/// grid; and clamped away from zero, because a zero here would collide with the
/// distance-field sentinel in [`crate::types::GlyphKey`].
#[inline]
pub fn bitmap_size_px(font_size: Px, scale: ScaleFactor) -> u16 {
    let size = device_font_size(font_size, scale).round();
    if !size.is_finite() || size < 1.0 { 1 } else { size.min(u16::MAX as f32) as u16 }
}

/// An analytic coverage accumulator.
///
/// Holds one `f32` per cell of signed area difference; [`Rasterizer::coverage`]
/// integrates those along each row into actual coverage. Reusable across glyphs
/// via [`Rasterizer::reset`], which matters when a cache miss storm re-rasterises
/// a whole string at once.
#[derive(Clone, Debug)]
pub struct Rasterizer {
    width: u32,
    height: u32,
    /// `width + 2`: a line landing exactly on the right edge deposits into the
    /// column one past the last visible one, and that must not bleed into the
    /// next row.
    stride: usize,
    area: Vec<f32>,
}

impl Rasterizer {
    /// A rasteriser for a `width` by `height` pixel target.
    pub fn new(width: u32, height: u32) -> Self {
        let stride = width as usize + 2;
        Self { width, height, stride, area: vec![0.0; stride * height as usize] }
    }

    /// Target width in pixels.
    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Target height in pixels.
    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Clears the accumulated area, keeping the allocation.
    pub fn reset(&mut self) {
        self.area.fill(0.0);
    }

    /// Accumulates one line segment, in pixel coordinates with y down.
    ///
    /// Horizontal lines contribute nothing (they cross no scanline) and are
    /// skipped. Geometry outside the target is clipped rather than rejected: a
    /// shape overhanging the left edge still has to fill the columns it covers.
    pub fn add_line(&mut self, p0: Point<f32>, p1: Point<f32>) {
        if !(p0.x.is_finite() && p0.y.is_finite() && p1.x.is_finite() && p1.y.is_finite()) {
            return;
        }
        if p0.y == p1.y {
            return;
        }
        // `dir` carries the winding: reversing a line negates its contribution,
        // which is what makes a counter cancel the contour that contains it.
        let (dir, top, bottom) = if p0.y < p1.y { (1.0, p0, p1) } else { (-1.0, p1, p0) };
        // A near-horizontal segment overflows this slope to infinity from
        // perfectly finite endpoints, and then `(y_start - top.y) * dxdy` below
        // is `0 * inf` whenever the segment starts inside the target — a NaN
        // that no later clamp catches, because `clamp` and `min` both pass NaN
        // through or prefer it. It reaches `area`, and `acc.abs().min(1.0)`
        // reads it as full coverage, so one such segment turns a whole row
        // solid. Saturating the slope leaves every derived value either finite
        // or infinite, which the clamps below do handle correctly, and costs
        // nothing: the segment is still walked, its x just pins to the edge.
        let dxdy = ((bottom.x - top.x) / (bottom.y - top.y)).clamp(-f32::MAX, f32::MAX);

        let y_start = top.y.max(0.0);
        let y_end = bottom.y.min(self.height as f32);
        if y_end <= y_start {
            return;
        }
        let mut x = top.x + (y_start - top.y) * dxdy;
        let y_first = y_start.floor() as u32;
        let y_last = (y_end.ceil() as u32).min(self.height);
        let right = self.width as f32;

        for y in y_first..y_last {
            let row = y as usize * self.stride;
            let dy = ((y + 1) as f32).min(y_end) - (y as f32).max(y_start);
            if dy <= 0.0 {
                x += dxdy * dy;
                continue;
            }
            let x_next = x + dxdy * dy;
            // Clamping both ends keeps every index in range while preserving the
            // total deposited area, so the prefix sum stays exact.
            let xa = x.clamp(0.0, right);
            let xb = x_next.clamp(0.0, right);
            let d = dy * dir;
            let (x0, x1) = if xa < xb { (xa, xb) } else { (xb, xa) };
            let x0_floor = x0.floor();
            let x0i = x0_floor as usize;
            let x1_ceil = x1.ceil();
            let x1i = x1_ceil as usize;

            if x1i <= x0i + 1 {
                // The whole span lies within one column: split `d` between it
                // and its right neighbour by where the midpoint falls.
                let xmf = 0.5 * (xa + xb) - x0_floor;
                self.area[row + x0i] += d - d * xmf;
                self.area[row + x0i + 1] += d * xmf;
            } else {
                // The span crosses several columns; distribute `d` by the area of
                // the trapezoid falling in each.
                let s = (x1 - x0).recip();
                let x0f = x0 - x0_floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = x1 - x1_ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                self.area[row + x0i] += d * a0;
                if x1i == x0i + 2 {
                    self.area[row + x0i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    self.area[row + x0i + 1] += d * (a1 - a0);
                    for xi in x0i + 2..x1i - 1 {
                        self.area[row + xi] += d * s;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    self.area[row + x1i - 1] += d * (1.0 - a2 - am);
                }
                self.area[row + x1i] += d * am;
            }
            x = x_next;
        }
    }

    /// Accumulates a polyline, closing it if it is not already closed.
    pub fn add_polyline(&mut self, points: &[Point<f32>]) {
        if points.len() < 2 {
            return;
        }
        for pair in points.windows(2) {
            self.add_line(pair[0], pair[1]);
        }
        let (first, last) = (points[0], points[points.len() - 1]);
        if first.x != last.x || first.y != last.y {
            self.add_line(last, first);
        }
    }

    /// Integrates the accumulated area into 8-bit coverage, row-major.
    ///
    /// The running sum restarts on every row: a closed contour deposits a net
    /// zero per row, so restarting costs nothing and stops a numerically ragged
    /// row from tinting every row below it.
    ///
    /// Coverage is `|accumulated|` clamped to one, which implements the nonzero
    /// winding rule for the overlaps that matter — two contours in the same
    /// direction saturate instead of doubling, and a counter cancels its parent.
    pub fn coverage(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity((self.width * self.height) as usize);
        for y in 0..self.height as usize {
            let row = y * self.stride;
            let mut acc = 0.0f32;
            for x in 0..self.width as usize {
                acc += self.area[row + x];
                // `f32::min` returns the *other* operand for a NaN, so a plain
                // `.min(1.0)` would turn a poisoned cell into full coverage —
                // the loudest possible way to fail. Nothing should be able to
                // put a NaN here, and a transparent pixel is the quiet failure.
                let v = acc.abs();
                let v = if v.is_nan() { 0.0 } else { v.min(1.0) };
                out.push((v * 255.0 + 0.5) as u8);
            }
        }
        out
    }
}

/// Rasterises an outline already in em units, y-down, at an exact pixel size.
///
/// `size_px` is the em size in device pixels. The returned image's `bounds_em`
/// is the ink box snapped outwards to whole device pixels, so multiplying it by
/// `size_px` lands the bitmap on the pixel grid one texel to one pixel — which is
/// the entire reason this path exists.
pub fn rasterize_shape(shape: &Shape, size_px: f32) -> GlyphImage {
    let empty = GlyphImage::empty(GlyphFormat::Grayscale);
    if shape.is_empty() || !size_px.is_finite() || size_px <= 0.0 {
        return empty;
    }
    let Some(ink) = shape.bounds() else { return empty };

    // Round the ink box out to whole pixels. The partially covered pixels at the
    // boundary have to be included or the antialiased edge is cut off.
    let x0 = (ink.min_x() * size_px).floor();
    let y0 = (ink.min_y() * size_px).floor();
    let x1 = (ink.max_x() * size_px).ceil();
    let y1 = (ink.max_y() * size_px).ceil();
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return empty;
    }
    let width = (x1 - x0).max(1.0);
    let height = (y1 - y0).max(1.0);
    // A glyph this large is not text; refusing beats a multi-gigabyte allocation
    // driven by a malformed font.
    if width > 4096.0 || height > 4096.0 {
        return empty;
    }

    let mut rasterizer = Rasterizer::new(width as u32, height as u32);
    let tolerance = FLATTEN_TOLERANCE_PX / size_px;
    let mut buffer: Vec<Point<f32>> = Vec::new();
    for polyline in shape.flatten(tolerance) {
        buffer.clear();
        buffer.extend(polyline.iter().map(|p| Point::new(p.x * size_px - x0, p.y * size_px - y0)));
        rasterizer.add_polyline(&buffer);
    }

    GlyphImage {
        width: width as u32,
        height: height as u32,
        format: GlyphFormat::Grayscale,
        data: rasterizer.coverage(),
        bounds_em: Rect::new(
            Point::new(x0 / size_px, y0 / size_px),
            Size::new(width / size_px, height / size_px),
        ),
        // Coverage is not a distance field; there is no range to travel with it.
        range_em: 0.0,
    }
}

/// Rasterises one glyph of a face into a grayscale bitmap.
///
/// `size_px` is the em size in *device* pixels; use [`device_font_size`] to get
/// it from a logical size and a scale factor. Glyphs with no outline produce an
/// empty image rather than an error, exactly as in the distance-field path.
pub fn rasterize_glyph(
    face: &ttf_parser::Face<'_>,
    glyph: GlyphId,
    size_px: f32,
) -> Result<GlyphImage, FontError> {
    let shape = extract_shape(face, glyph)?;
    Ok(rasterize_shape(&shape, size_px))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mtsdf::{EdgeSegment, glyph_metrics};
    use sphere_core::px;

    fn p(x: f32, y: f32) -> Point<f32> {
        Point::new(x, y)
    }

    /// Coverage of a rasteriser fed one closed polygon in pixel coordinates.
    fn raster(width: u32, height: u32, polygon: &[Point<f32>]) -> Vec<u8> {
        let mut r = Rasterizer::new(width, height);
        r.add_polyline(polygon);
        r.coverage()
    }

    fn at(cov: &[u8], width: u32, x: u32, y: u32) -> u8 {
        cov[(y * width + x) as usize]
    }

    // -- the rasteriser ------------------------------------------------------

    #[test]
    fn a_pixel_aligned_rectangle_is_fully_on_or_fully_off() {
        // Rectangle covering columns 1..3 of rows 1..3 exactly.
        let cov = raster(4, 4, &[p(1.0, 1.0), p(3.0, 1.0), p(3.0, 3.0), p(1.0, 3.0)]);
        for y in 0..4 {
            for x in 0..4 {
                let inside = (1..3).contains(&x) && (1..3).contains(&y);
                let want = if inside { 255 } else { 0 };
                assert_eq!(at(&cov, 4, x, y), want, "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn a_half_covered_pixel_reads_as_half() {
        // The left edge cuts column 1 down the middle.
        let cov = raster(4, 2, &[p(1.5, 0.0), p(4.0, 0.0), p(4.0, 2.0), p(1.5, 2.0)]);
        assert_eq!(at(&cov, 4, 0, 0), 0);
        let half = at(&cov, 4, 1, 0);
        assert!((half as i32 - 128).abs() <= 2, "half coverage read as {half}");
        assert_eq!(at(&cov, 4, 2, 0), 255);
        assert_eq!(at(&cov, 4, 3, 0), 255);
        // A quarter and three quarters, on the same edge.
        let cov = raster(4, 1, &[p(0.75, 0.0), p(4.0, 0.0), p(4.0, 1.0), p(0.75, 1.0)]);
        assert!((at(&cov, 4, 0, 0) as i32 - 64).abs() <= 2, "{}", at(&cov, 4, 0, 0));
    }

    #[test]
    fn total_coverage_of_a_triangle_equals_its_area() {
        // Right triangle with legs 6 and 6: area 18.
        let cov = raster(8, 8, &[p(1.0, 1.0), p(7.0, 1.0), p(1.0, 7.0)]);
        let total: f32 = cov.iter().map(|&c| c as f32 / 255.0).sum();
        assert!((total - 18.0).abs() < 0.1, "total coverage {total}, expected 18");

        // A slanted triangle, whose edges cross many columns per row.
        let cov = raster(16, 16, &[p(2.0, 1.0), p(14.0, 5.0), p(6.0, 15.0)]);
        let total: f32 = cov.iter().map(|&c| c as f32 / 255.0).sum();
        // Shoelace: |(12)(14) - (4)(4)| / 2 = 76.
        assert!((total - 76.0).abs() < 0.2, "total coverage {total}, expected 76");
    }

    #[test]
    fn winding_cancels_a_hole() {
        let mut r = Rasterizer::new(8, 8);
        r.add_polyline(&[p(1.0, 1.0), p(7.0, 1.0), p(7.0, 7.0), p(1.0, 7.0)]);
        // Inner square wound the other way.
        r.add_polyline(&[p(3.0, 3.0), p(3.0, 5.0), p(5.0, 5.0), p(5.0, 3.0)]);
        let cov = r.coverage();
        assert_eq!(at(&cov, 8, 2, 2), 255, "the ring must be solid");
        assert_eq!(at(&cov, 8, 4, 4), 0, "the counter must be empty");
        assert_eq!(at(&cov, 8, 0, 0), 0);
    }

    #[test]
    fn overlapping_contours_saturate_instead_of_doubling() {
        let mut r = Rasterizer::new(8, 4);
        let square = [p(1.0, 1.0), p(5.0, 1.0), p(5.0, 3.0), p(1.0, 3.0)];
        r.add_polyline(&square);
        r.add_polyline(&square);
        let cov = r.coverage();
        assert_eq!(at(&cov, 8, 2, 2), 255, "double winding must clamp, not wrap");
        assert_eq!(at(&cov, 8, 6, 2), 0);
    }

    #[test]
    fn geometry_outside_the_target_is_clipped_not_dropped() {
        // A rectangle hanging off every edge: the visible part is still filled,
        // and nothing indexes out of bounds.
        let cov = raster(4, 4, &[p(-10.0, -10.0), p(10.0, -10.0), p(10.0, 2.0), p(-10.0, 2.0)]);
        for x in 0..4 {
            assert_eq!(at(&cov, 4, x, 0), 255, "column {x}");
            assert_eq!(at(&cov, 4, x, 1), 255, "column {x}");
            assert_eq!(at(&cov, 4, x, 3), 0, "column {x}");
        }
        // Entirely outside in each direction: nothing is drawn, nothing panics.
        for polygon in [
            [p(-9.0, -9.0), p(-5.0, -9.0), p(-5.0, -5.0), p(-9.0, -5.0)],
            [p(90.0, 1.0), p(95.0, 1.0), p(95.0, 3.0), p(90.0, 3.0)],
            [p(1.0, -20.0), p(3.0, -20.0), p(3.0, -10.0), p(1.0, -10.0)],
            [p(1.0, 20.0), p(3.0, 20.0), p(3.0, 30.0), p(1.0, 30.0)],
        ] {
            assert!(raster(4, 4, &polygon).iter().all(|&c| c == 0), "{polygon:?}");
        }
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        assert!(raster(4, 4, &[]).iter().all(|&c| c == 0));
        assert!(raster(4, 4, &[p(1.0, 1.0)]).iter().all(|&c| c == 0));
        // A horizontal hairline crosses no scanline and encloses nothing.
        assert!(raster(4, 4, &[p(0.0, 2.0), p(4.0, 2.0)]).iter().all(|&c| c == 0));
        // Coincident points.
        assert!(raster(4, 4, &[p(2.0, 2.0), p(2.0, 2.0), p(2.0, 2.0)]).iter().all(|&c| c == 0));
        // Non-finite coordinates must be ignored rather than poison the buffer.
        let mut r = Rasterizer::new(4, 4);
        r.add_line(p(f32::NAN, 0.0), p(2.0, 3.0));
        r.add_line(p(0.0, f32::INFINITY), p(2.0, 3.0));
        assert!(r.coverage().iter().all(|&c| c == 0));
    }

    #[test]
    fn a_slope_that_overflows_to_infinity_does_not_poison_the_row() {
        // Finite endpoints whose dx/dy overflows f32. The old code computed
        // `0.0 * inf` for the starting x, wrote NaN into the accumulator, and
        // `NaN.min(1.0) == 1.0` turned the whole row solid.
        let mut r = Rasterizer::new(8, 8);
        r.add_line(p(0.0, f32::MIN_POSITIVE), p(1.0e38, 2.0 * f32::MIN_POSITIVE));
        let cov = r.coverage();
        assert!(cov.iter().all(|&c| c == 0), "row 0 = {:?}", &cov[..8]);

        // The same segment reached from the other direction, and with the slope
        // running the other way.
        let mut r = Rasterizer::new(8, 8);
        r.add_line(p(1.0e38, 2.0 * f32::MIN_POSITIVE), p(0.0, f32::MIN_POSITIVE));
        assert!(r.coverage().iter().all(|&c| c == 0));
        let mut r = Rasterizer::new(8, 8);
        r.add_line(p(0.0, 1.0), p(-1.0e38, 1.0 + f32::MIN_POSITIVE));
        assert!(r.coverage().iter().all(|&c| c == 0));
    }

    #[test]
    fn a_line_on_the_right_edge_does_not_spill_into_the_next_row() {
        // Column `width` is written to for a span ending exactly on the edge; if
        // the stride were `width` that write would land on the next row's first
        // pixel and light it up.
        let mut r = Rasterizer::new(4, 4);
        r.add_polyline(&[p(2.0, 0.0), p(4.0, 0.0), p(4.0, 2.0), p(2.0, 2.0)]);
        let cov = r.coverage();
        assert_eq!(at(&cov, 4, 0, 2), 0, "spill into the row below");
        assert_eq!(at(&cov, 4, 3, 1), 255);
    }

    #[test]
    fn reset_makes_a_rasterizer_reusable() {
        let mut r = Rasterizer::new(4, 4);
        r.add_polyline(&[p(0.0, 0.0), p(4.0, 0.0), p(4.0, 4.0), p(0.0, 4.0)]);
        assert!(r.coverage().iter().all(|&c| c == 255));
        r.reset();
        assert!(r.coverage().iter().all(|&c| c == 0));
        assert_eq!((r.width(), r.height()), (4, 4));
    }

    // -- shape rasterisation -------------------------------------------------

    #[test]
    fn an_empty_shape_produces_an_empty_image() {
        let img = rasterize_shape(&Shape::new(), 12.0);
        assert!(img.is_empty());
        assert_eq!(img.format, GlyphFormat::Grayscale);
        assert_eq!(img.range_em, 0.0);
        assert!(img.data.is_empty());
    }

    #[test]
    fn a_nonsense_size_produces_an_empty_image() {
        let square = Shape::from_polygon(&[p(0.0, 0.0), p(0.0, 1.0), p(1.0, 1.0), p(1.0, 0.0)]);
        for size in [0.0, -12.0, f32::NAN, f32::INFINITY] {
            assert!(rasterize_shape(&square, size).is_empty(), "size {size}");
        }
    }

    #[test]
    fn a_square_glyph_rasterises_to_exact_pixels() {
        // Half an em square starting at the origin, at 16 px/em: 8 by 8 pixels,
        // every one of them solid.
        let square = Shape::from_polygon(&[p(0.0, 0.0), p(0.0, 0.5), p(0.5, 0.5), p(0.5, 0.0)]);
        let img = rasterize_shape(&square, 16.0);
        assert_eq!((img.width, img.height), (8, 8));
        assert_eq!(img.data.len(), 64);
        assert!(img.data.iter().all(|&c| c == 255), "{:?}", &img.data[..8]);
        assert_eq!(img.format, GlyphFormat::Grayscale);
        assert_eq!(img.bounds_em, Rect::new(p(0.0, 0.0), Size::new(0.5, 0.5)));
    }

    #[test]
    fn bounds_snap_outwards_to_whole_device_pixels() {
        // An ink box that lands between pixels must grow, not shift: the bitmap
        // is only crisp if its texels sit on the pixel grid.
        let shape =
            Shape::from_polygon(&[p(0.13, -0.4), p(0.13, 0.02), p(0.61, 0.02), p(0.61, -0.4)]);
        let size = 16.0;
        let img = rasterize_shape(&shape, size);
        for edge in [img.bounds_em.min_x(), img.bounds_em.min_y(), img.bounds_em.max_x()] {
            let pixels = edge * size;
            assert!((pixels - pixels.round()).abs() < 1e-4, "{pixels} is not a whole pixel");
        }
        assert!(img.bounds_em.min_x() <= 0.13 && img.bounds_em.max_x() >= 0.61);
        assert_eq!(img.width, (img.bounds_em.size.width * size).round() as u32);
        assert_eq!(img.height, (img.bounds_em.size.height * size).round() as u32);
    }

    #[test]
    fn a_curved_outline_is_flattened_before_rasterising() {
        // A disc of radius 0.25 em from four cubic quarter-arcs. Total coverage
        // must match pi r^2 in pixels, which only holds if the curves were
        // flattened finely enough.
        let k = 0.552_285_f32 * 0.25;
        let r = 0.25_f32;
        let mut contour = crate::mtsdf::Contour::new();
        for (a, b, c, d) in [
            (p(r, 0.0), p(r, k), p(k, r), p(0.0, r)),
            (p(0.0, r), p(-k, r), p(-r, k), p(-r, 0.0)),
            (p(-r, 0.0), p(-r, -k), p(-k, -r), p(0.0, -r)),
            (p(0.0, -r), p(k, -r), p(r, -k), p(r, 0.0)),
        ] {
            contour.push(EdgeSegment::Cubic { p0: a, control0: b, control1: c, p1: d });
        }
        let shape = Shape { contours: vec![contour] };
        let size = 64.0;
        let img = rasterize_shape(&shape, size);
        let total: f32 = img.data.iter().map(|&c| c as f32 / 255.0).sum();
        let expected = core::f32::consts::PI * (r * size) * (r * size);
        assert!((total - expected).abs() / expected < 0.01, "{total} vs {expected}");
        // The centre is solid and the corners of the box are empty.
        let centre = (img.height / 2 * img.width + img.width / 2) as usize;
        assert_eq!(img.data[centre], 255);
        assert_eq!(img.data[0], 0);
    }

    // -- the MTSDF-versus-bitmap policy -------------------------------------

    #[test]
    fn the_policy_thresholds_on_physical_pixels_not_logical_ones() {
        let one = ScaleFactor::new(1.0);
        let two = ScaleFactor::new(2.0);
        // The same logical size decides differently on a HiDPI display.
        assert_eq!(choose_raster_strategy(px(11.0), one), RasterStrategy::Bitmap);
        assert_eq!(choose_raster_strategy(px(11.0), two), RasterStrategy::Mtsdf);
        // ...and a large logical size at a tiny scale falls back.
        assert_eq!(choose_raster_strategy(px(20.0), ScaleFactor::new(0.5)), RasterStrategy::Bitmap);
    }

    #[test]
    fn the_policy_boundary_is_exactly_twelve_device_pixels() {
        let one = ScaleFactor::new(1.0);
        assert_eq!(choose_raster_strategy(px(11.99), one), RasterStrategy::Bitmap);
        assert_eq!(choose_raster_strategy(px(12.0), one), RasterStrategy::Mtsdf);
        assert_eq!(choose_raster_strategy(px(12.01), one), RasterStrategy::Mtsdf);
        // Fractional scaling, the case that actually ships: 1.25 and 1.5.
        assert_eq!(choose_raster_strategy(px(10.0), ScaleFactor::new(1.25)), RasterStrategy::Mtsdf);
        assert_eq!(choose_raster_strategy(px(9.0), ScaleFactor::new(1.25)), RasterStrategy::Bitmap);
        assert_eq!(choose_raster_strategy(px(8.0), ScaleFactor::new(1.5)), RasterStrategy::Mtsdf);
    }

    #[test]
    fn bitmap_sizes_round_to_whole_device_pixels_and_never_reach_zero() {
        assert_eq!(bitmap_size_px(px(11.0), ScaleFactor::new(1.0)), 11);
        assert_eq!(bitmap_size_px(px(11.0), ScaleFactor::new(1.5)), 17); // 16.5 rounds up
        assert_eq!(bitmap_size_px(px(9.4), ScaleFactor::new(1.0)), 9);
        assert_eq!(bitmap_size_px(px(9.6), ScaleFactor::new(1.0)), 10);
        // A zero size would alias onto the distance-field key.
        assert_eq!(bitmap_size_px(px(0.0), ScaleFactor::new(1.0)), 1);
        assert_eq!(bitmap_size_px(px(0.2), ScaleFactor::new(1.0)), 1);
        assert_eq!(bitmap_size_px(px(f32::NAN), ScaleFactor::new(1.0)), 1);
        // And an absurd size must not wrap around.
        assert_eq!(bitmap_size_px(px(1.0e9), ScaleFactor::new(4.0)), u16::MAX);
    }

    #[test]
    fn device_font_size_never_goes_negative() {
        assert_eq!(device_font_size(px(-5.0), ScaleFactor::new(2.0)), 0.0);
        assert_eq!(device_font_size(px(10.0), ScaleFactor::new(1.25)), 12.5);
    }

    // -- integration with a real face ---------------------------------------

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
    fn a_real_glyph_rasterises_to_plausible_coverage() {
        let Some(data) = system_font() else {
            eprintln!("skipping: no system font found");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        let gid = face.glyph_index('H').expect("a text font has an 'H'");
        let glyph = GlyphId(gid.0);
        let metrics = glyph_metrics(&face, glyph).unwrap();

        let size = 11.0;
        let img = rasterize_glyph(&face, glyph, size).unwrap();
        assert_eq!(img.format, GlyphFormat::Grayscale);
        assert!(!img.is_empty());
        assert_eq!(img.data.len(), (img.width * img.height) as usize);
        // An 'H' at 11 px is a handful of pixels tall, and its cap height is most
        // of the em.
        assert!(img.height >= 6 && img.height <= 12, "height {}", img.height);
        assert!(img.width >= 4 && img.width <= 14, "width {}", img.width);
        // The bitmap covers the ink and snaps outwards, never inwards.
        assert!(img.bounds_em.min_x() <= metrics.bounds.min_x() + 1e-6);
        assert!(img.bounds_em.max_y() >= metrics.bounds.max_y() - 1e-6);
        // Some ink, but nowhere near solid: an 'H' is mostly counter.
        let ink: u32 = img.data.iter().map(|&c| c as u32).sum();
        let full = img.data.len() as u32 * 255;
        assert!(ink > full / 10, "an 'H' should have ink: {ink}/{full}");
        assert!(ink < full * 9 / 10, "an 'H' should not be a solid block: {ink}/{full}");
    }

    #[test]
    fn a_space_rasterises_to_nothing_without_erroring() {
        let Some(data) = system_font() else {
            eprintln!("skipping: no system font found");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        let gid = face.glyph_index(' ').unwrap();
        let img = rasterize_glyph(&face, GlyphId(gid.0), 11.0).unwrap();
        assert!(img.is_empty());
    }

    #[test]
    fn an_out_of_range_glyph_is_an_error() {
        let Some(data) = system_font() else {
            eprintln!("skipping: no system font found");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).unwrap();
        assert!(matches!(
            rasterize_glyph(&face, GlyphId(face.number_of_glyphs()), 11.0),
            Err(FontError::MissingGlyph(_))
        ));
    }
}
