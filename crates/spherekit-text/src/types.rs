//! Shared vocabulary for the text stack.
//!
//! Keeping these in one place lets glyph rasterisation, atlas packing, font
//! discovery and shaping evolve independently: they agree on these types and
//! nothing else.

use spherekit_core::{FontId, GlyphId, Point, Px, Rect, Size};

/// Font weight on the usual 1..=1000 scale.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct FontWeight(pub u16);

impl FontWeight {
    /// 100.
    pub const THIN: Self = Self(100);
    /// 200.
    pub const EXTRA_LIGHT: Self = Self(200);
    /// 300.
    pub const LIGHT: Self = Self(300);
    /// 400.
    pub const NORMAL: Self = Self(400);
    /// 500.
    pub const MEDIUM: Self = Self(500);
    /// 600.
    pub const SEMI_BOLD: Self = Self(600);
    /// 700.
    pub const BOLD: Self = Self(700);
    /// 800.
    pub const EXTRA_BOLD: Self = Self(800);
    /// 900.
    pub const BLACK: Self = Self(900);
}

impl Default for FontWeight {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// Upright, italic or oblique.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum FontStyle {
    /// Upright.
    #[default]
    Normal,
    /// A distinct italic design.
    Italic,
    /// A slanted version of the upright design.
    Oblique,
}

/// Horizontal width class.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum FontStretch {
    /// 50 %.
    UltraCondensed,
    /// 62.5 %.
    ExtraCondensed,
    /// 75 %.
    Condensed,
    /// 87.5 %.
    SemiCondensed,
    /// 100 %.
    #[default]
    Normal,
    /// 112.5 %.
    SemiExpanded,
    /// 125 %.
    Expanded,
    /// 150 %.
    ExtraExpanded,
    /// 200 %.
    UltraExpanded,
}

/// A request for a font face.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct FontRequest {
    /// Family names in priority order. The first that resolves wins.
    pub families: Vec<String>,
    /// Desired weight.
    pub weight: FontWeight,
    /// Desired style.
    pub style: FontStyle,
    /// Desired width class.
    pub stretch: FontStretch,
}

impl FontRequest {
    /// A request for one family at default weight and style.
    pub fn family(name: impl Into<String>) -> Self {
        Self { families: vec![name.into()], ..Default::default() }
    }

    /// Sets the weight.
    pub fn weight(mut self, w: FontWeight) -> Self {
        self.weight = w;
        self
    }

    /// Sets the style.
    pub fn style(mut self, s: FontStyle) -> Self {
        self.style = s;
        self
    }
}

/// A variable-font axis setting.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VariationAxis {
    /// The four-byte axis tag, such as `wght` or `wdth`.
    pub tag: [u8; 4],
    /// The value in the axis's own units.
    pub value: f32,
}

/// Identifies one rasterisable glyph.
///
/// The variation hash is what keeps two instances of the same variable font at
/// different weights from colliding in the atlas. For a static font it is zero,
/// so the common case costs nothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// The face this glyph belongs to.
    pub font: FontId,
    /// The glyph index within that face.
    pub glyph: GlyphId,
    /// Hash of the active variation coordinates, or zero for a static instance.
    pub variation_hash: u64,
    /// The rasterisation size in *device* pixels, rounded.
    ///
    /// Zero for distance-field glyphs, which are size-independent: that is the
    /// whole point of MTSDF, and caching one field per size would defeat it.
    /// Nonzero only for the small-size bitmap fallback.
    pub bitmap_size: u16,
    /// Pixel format this key was rasterised into.
    ///
    /// Grayscale and RGB-subpixel coverage of the same outline are different
    /// images and must never alias in the atlas.
    pub format: GlyphFormat,
    /// Quarter-pixel horizontal phase, in `0..=3`.
    ///
    /// Distance fields are position-independent and always use zero. Bitmap
    /// rasterisation applies this offset before snapping the ink box, so each
    /// nonzero phase names a distinct cached image.
    pub subpixel_phase: u8,
}

impl GlyphKey {
    /// A key for a size-independent distance field.
    #[inline]
    pub fn mtsdf(font: FontId, glyph: GlyphId, variation_hash: u64) -> Self {
        Self {
            font,
            glyph,
            variation_hash,
            bitmap_size: 0,
            format: GlyphFormat::Mtsdf,
            subpixel_phase: 0,
        }
    }

    /// A key for a size-specific grayscale bitmap.
    #[inline]
    pub fn bitmap(font: FontId, glyph: GlyphId, variation_hash: u64, size_px: u16) -> Self {
        Self::bitmap_with_phase(font, glyph, variation_hash, size_px, 0)
    }

    /// A key for a size-specific grayscale bitmap at a quarter-pixel phase.
    #[inline]
    pub fn bitmap_with_phase(
        font: FontId,
        glyph: GlyphId,
        variation_hash: u64,
        size_px: u16,
        subpixel_phase: u8,
    ) -> Self {
        Self {
            font,
            glyph,
            variation_hash,
            bitmap_size: size_px.max(1),
            format: GlyphFormat::Grayscale,
            subpixel_phase: subpixel_phase.min(3),
        }
    }

    /// A key for size-specific RGB subpixel coverage at a quarter-pixel phase.
    #[inline]
    pub fn subpixel_bitmap(
        font: FontId,
        glyph: GlyphId,
        variation_hash: u64,
        size_px: u16,
        subpixel_phase: u8,
    ) -> Self {
        Self {
            font,
            glyph,
            variation_hash,
            bitmap_size: size_px.max(1),
            format: GlyphFormat::Subpixel,
            subpixel_phase: subpixel_phase.min(3),
        }
    }

    /// True when this key names a distance field rather than a bitmap.
    #[inline]
    pub fn is_mtsdf(self) -> bool {
        self.format == GlyphFormat::Mtsdf
    }
}

/// Pixel format of a rasterised glyph.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GlyphFormat {
    /// Four channels: RGB carry the multi-channel distance, A the true distance.
    Mtsdf,
    /// One channel of coverage.
    Grayscale,
    /// Three channels of independently filtered RGB-stripe coverage.
    Subpixel,
    /// Full color, for emoji.
    ColorBitmap,
}

impl GlyphFormat {
    /// Bytes per pixel in the rasterised image.
    #[inline]
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            GlyphFormat::Mtsdf | GlyphFormat::ColorBitmap => 4,
            GlyphFormat::Grayscale => 1,
            GlyphFormat::Subpixel => 3,
        }
    }
}

/// A rasterised glyph, before it reaches the atlas.
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphImage {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Pixel format.
    pub format: GlyphFormat,
    /// Tightly packed pixels, `width * height * format.bytes_per_pixel()` long.
    pub data: Vec<u8>,
    /// The glyph's bounding box in font-em units, relative to the pen position
    /// on the baseline, with y increasing downward.
    ///
    /// Em units rather than pixels is what makes an MTSDF glyph reusable at
    /// every size: the caller multiplies by the font size at draw time.
    pub bounds_em: Rect<f32>,
    /// How far the distance field extends beyond the outline, in em units.
    ///
    /// The shader converts this into a screen-space range to pick its smoothing
    /// width, so it has to travel with the glyph rather than being a constant.
    pub range_em: f32,
}

impl GlyphImage {
    /// An empty image, produced for glyphs with no outline such as a space.
    pub fn empty(format: GlyphFormat) -> Self {
        Self { width: 0, height: 0, format, data: Vec::new(), bounds_em: Rect::ZERO, range_em: 0.0 }
    }

    /// True when the glyph has no visible coverage.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Metrics for one glyph, in font-em units.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct GlyphMetrics {
    /// Horizontal advance.
    pub advance: f32,
    /// Offset from the pen to the left edge of the ink.
    pub left_bearing: f32,
    /// The ink bounding box, y-down, relative to the pen on the baseline.
    pub bounds: Rect<f32>,
}

/// Vertical metrics for a face, in font-em units.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct FontMetrics {
    /// Distance from the baseline to the top of the tallest glyph, positive up.
    pub ascent: f32,
    /// Distance from the baseline to the bottom of the lowest glyph, positive down.
    pub descent: f32,
    /// Recommended extra space between lines.
    pub line_gap: f32,
    /// Height of a lowercase `x`.
    pub x_height: f32,
    /// Height of an uppercase letter.
    pub cap_height: f32,
    /// Offset from the baseline to the underline, positive down.
    pub underline_offset: f32,
    /// Recommended underline thickness.
    pub underline_thickness: f32,
    /// Units per em in the source font, kept so raw font units can be recovered.
    pub units_per_em: u16,
}

impl FontMetrics {
    /// Default line height in em units: ascent plus descent plus line gap.
    #[inline]
    pub fn line_height(&self) -> f32 {
        self.ascent + self.descent + self.line_gap
    }

    /// Scales the metrics to a pixel size.
    #[inline]
    pub fn scaled(&self, font_size: Px) -> ScaledFontMetrics {
        let s = font_size.get();
        ScaledFontMetrics {
            ascent: Px(self.ascent * s),
            descent: Px(self.descent * s),
            line_gap: Px(self.line_gap * s),
            x_height: Px(self.x_height * s),
            cap_height: Px(self.cap_height * s),
            underline_offset: Px(self.underline_offset * s),
            underline_thickness: Px(self.underline_thickness * s),
        }
    }
}

/// Font metrics resolved to logical pixels at a specific size.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct ScaledFontMetrics {
    /// Baseline to top, positive up.
    pub ascent: Px,
    /// Baseline to bottom, positive down.
    pub descent: Px,
    /// Extra inter-line space.
    pub line_gap: Px,
    /// Lowercase height.
    pub x_height: Px,
    /// Uppercase height.
    pub cap_height: Px,
    /// Underline position, positive down.
    pub underline_offset: Px,
    /// Underline thickness.
    pub underline_thickness: Px,
}

impl ScaledFontMetrics {
    /// Default line height.
    #[inline]
    pub fn line_height(&self) -> Px {
        self.ascent + self.descent + self.line_gap
    }
}

/// Where a glyph ended up in the atlas.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AtlasPlacement {
    /// Which atlas page holds it.
    pub page: u32,
    /// Normalised `[u0, v0, u1, v1]` within that page.
    pub uv: [f32; 4],
    /// Texel rectangle within the page, for debugging and repacking.
    pub texels: Rect<u32>,
    /// The glyph's bounds in em units, carried through from the raster step.
    pub bounds_em: Rect<f32>,
    /// The distance-field range in em units, zero for bitmaps.
    pub range_em: f32,
    /// Pixel format stored in this page.
    pub format: GlyphFormat,
}

/// Horizontal text alignment.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextAlign {
    /// Against the paragraph's start edge, which follows the base direction.
    #[default]
    Start,
    /// Against the paragraph's end edge.
    End,
    /// Always the left edge, regardless of direction.
    Left,
    /// Always the right edge.
    Right,
    /// Centred.
    Center,
    /// Stretched to both edges, except on the last line.
    Justify,
}

/// Base writing direction for a paragraph.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextDirection {
    /// Infer from the first strong character, per the Unicode bidi algorithm.
    #[default]
    Auto,
    /// Left to right.
    Ltr,
    /// Right to left.
    Rtl,
}

/// How lines are broken.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum WrapMode {
    /// Break at permitted line-break opportunities.
    #[default]
    Word,
    /// Break anywhere a grapheme boundary allows.
    Grapheme,
    /// Never break; the paragraph is one line.
    None,
}

/// What to do when a line does not fit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Overflow {
    /// Let the text extend past the box; the caller clips.
    #[default]
    Visible,
    /// Cut at the box edge.
    Clip,
    /// Replace the tail with a horizontal ellipsis.
    Ellipsis,
}

/// Everything needed to lay out a paragraph.
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    /// Which face to use.
    pub font: FontRequest,
    /// Size in logical pixels.
    pub font_size: Px,
    /// Line height in logical pixels. `None` uses the font's own metrics.
    pub line_height: Option<Px>,
    /// Extra space inserted after every grapheme cluster.
    pub letter_spacing: Px,
    /// Extra space added to every space character.
    pub word_spacing: Px,
    /// Horizontal alignment.
    pub align: TextAlign,
    /// Base direction.
    pub direction: TextDirection,
    /// Line breaking behaviour.
    pub wrap: WrapMode,
    /// Overflow behaviour.
    pub overflow: Overflow,
    /// Variable-font axis settings.
    pub variations: Vec<VariationAxis>,
    /// OpenType features to enable or disable, as `(tag, value)`.
    pub features: Vec<([u8; 4], u32)>,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font: FontRequest::default(),
            font_size: Px(14.0),
            line_height: None,
            letter_spacing: Px::ZERO,
            word_spacing: Px::ZERO,
            align: TextAlign::default(),
            direction: TextDirection::default(),
            wrap: WrapMode::default(),
            overflow: Overflow::default(),
            variations: Vec::new(),
            features: Vec::new(),
        }
    }
}

/// One shaped glyph with its position and source mapping.
///
/// Not `Copy`: the `cluster` byte range is not, and making the glyph `Copy`
/// would mean giving up the range that hit testing and selection depend on.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedGlyph {
    /// Glyph index within `font`.
    pub glyph: GlyphId,
    /// The face this glyph came from, which may differ from the requested face
    /// when fallback kicked in.
    pub font: FontId,
    /// Position relative to the line's origin, on the baseline.
    pub position: Point<Px>,
    /// Horizontal advance in logical pixels.
    pub advance: Px,
    /// Byte range in the source string this glyph maps to.
    ///
    /// Needed for hit testing, caret placement and selection; without it a
    /// click can be turned into a pixel offset but not into a text index.
    pub cluster: core::ops::Range<usize>,
    /// True when the glyph is part of a right-to-left run.
    pub rtl: bool,
}

/// A run of glyphs sharing one face and direction.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedRun {
    /// The face for every glyph in this run.
    pub font: FontId,
    /// Font size in logical pixels.
    pub font_size: Px,
    /// Whether the run reads right to left.
    pub rtl: bool,
    /// The glyphs.
    pub glyphs: Vec<ShapedGlyph>,
    /// Byte range of the source string this run covers.
    pub source: core::ops::Range<usize>,
}

impl ShapedRun {
    /// Total advance of the run.
    pub fn advance(&self) -> Px {
        self.glyphs.iter().map(|g| g.advance).sum()
    }
}

/// One laid-out line.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLine {
    /// Runs in visual order, left to right on screen.
    pub runs: Vec<ShapedRun>,
    /// Distance from the paragraph's top to this line's baseline.
    pub baseline: Px,
    /// The line's own height.
    pub height: Px,
    /// Total advance width of the line.
    pub width: Px,
    /// Byte range of the source string this line covers.
    pub source: core::ops::Range<usize>,
}

/// A fully laid-out paragraph.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextLayout {
    /// Lines top to bottom.
    pub lines: Vec<TextLine>,
    /// The tight bounding size of the laid-out text.
    pub size: Size<Px>,
    /// The width the layout was constrained to, if any.
    pub max_width: Option<Px>,
}

impl TextLayout {
    /// True when nothing was laid out.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Total glyph count across every line, for diagnostics.
    pub fn glyph_count(&self) -> usize {
        self.lines.iter().flat_map(|l| l.runs.iter()).map(|r| r.glyphs.len()).sum()
    }

    /// Maps a point to the nearest byte index in the source string.
    ///
    /// Used for click-to-caret and drag selection.
    pub fn hit_test(&self, p: Point<Px>) -> Option<usize> {
        // Pick the line whose vertical band contains the point, clamping to the
        // first and last so a click above or below still lands somewhere sane.
        //
        // The band is `[top, top + height)`, accumulated from the top of the
        // paragraph. Testing against `baseline + height` instead would push
        // every band down by one ascent and hand clicks in line *n* to line
        // *n - 1*.
        let mut top = Px::ZERO;
        let mut found = None;
        for l in &self.lines {
            let bottom = top + l.height;
            if p.y < bottom {
                found = Some(l);
                break;
            }
            top = bottom;
        }
        let line = found.or_else(|| self.lines.last())?;

        let mut best: Option<(f32, usize)> = None;
        for run in &line.runs {
            for g in &run.glyphs {
                for (edge, idx) in
                    [(g.position.x, g.cluster.start), (g.position.x + g.advance, g.cluster.end)]
                {
                    let d = (edge.get() - p.x.get()).abs();
                    if best.is_none_or(|(bd, _)| d < bd) {
                        best = Some((d, idx));
                    }
                }
            }
        }
        best.map(|(_, i)| i).or(Some(line.source.start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::px;

    fn fid(n: u32) -> FontId {
        FontId::new(n, 1)
    }

    #[test]
    fn mtsdf_keys_ignore_size_but_bitmap_keys_do_not() {
        let a = GlyphKey::mtsdf(fid(0), GlyphId(5), 0);
        let b = GlyphKey::mtsdf(fid(0), GlyphId(5), 0);
        assert_eq!(a, b, "one distance field must serve every size");
        assert!(a.is_mtsdf());

        let s10 = GlyphKey::bitmap(fid(0), GlyphId(5), 0, 10);
        let s11 = GlyphKey::bitmap(fid(0), GlyphId(5), 0, 11);
        assert_ne!(s10, s11, "bitmaps are size-specific");
        assert!(!s10.is_mtsdf());
    }

    #[test]
    fn variation_hash_separates_variable_font_instances() {
        let light = GlyphKey::mtsdf(fid(0), GlyphId(5), 0xAAAA);
        let bold = GlyphKey::mtsdf(fid(0), GlyphId(5), 0xBBBB);
        assert_ne!(light, bold);
    }

    #[test]
    fn zero_bitmap_size_is_clamped_away() {
        // A zero size would silently alias a bitmap key onto an MTSDF key.
        assert_eq!(GlyphKey::bitmap(fid(0), GlyphId(1), 0, 0).bitmap_size, 1);
    }

    #[test]
    fn bitmap_phase_and_format_are_part_of_the_cache_key() {
        let gray0 = GlyphKey::bitmap_with_phase(fid(0), GlyphId(5), 0, 12, 0);
        let gray1 = GlyphKey::bitmap_with_phase(fid(0), GlyphId(5), 0, 12, 1);
        let rgb1 = GlyphKey::subpixel_bitmap(fid(0), GlyphId(5), 0, 12, 1);
        assert_ne!(gray0, gray1, "quarter-pixel positions need separate bitmaps");
        assert_ne!(gray1, rgb1, "grayscale and RGB coverage must not alias");
        assert_eq!(GlyphKey::subpixel_bitmap(fid(0), GlyphId(5), 0, 12, 9).subpixel_phase, 3);
        assert_eq!(GlyphKey::mtsdf(fid(0), GlyphId(5), 0).subpixel_phase, 0);
        assert!(GlyphKey::mtsdf(fid(0), GlyphId(5), 0).is_mtsdf());
        assert!(!rgb1.is_mtsdf());
    }

    #[test]
    fn metrics_scale_linearly() {
        let m = FontMetrics {
            ascent: 0.8,
            descent: 0.2,
            line_gap: 0.1,
            units_per_em: 1000,
            ..Default::default()
        };
        assert!((m.line_height() - 1.1).abs() < 1e-6);
        let s = m.scaled(px(20.0));
        assert_eq!(s.ascent, px(16.0));
        assert_eq!(s.descent, px(4.0));
        assert_eq!(s.line_height(), px(22.0));
    }

    #[test]
    fn glyph_format_pixel_sizes() {
        assert_eq!(GlyphFormat::Mtsdf.bytes_per_pixel(), 4);
        assert_eq!(GlyphFormat::Grayscale.bytes_per_pixel(), 1);
        assert_eq!(GlyphFormat::Subpixel.bytes_per_pixel(), 3);
    }

    #[test]
    fn empty_layout_reports_no_glyphs() {
        let l = TextLayout::default();
        assert!(l.is_empty());
        assert_eq!(l.glyph_count(), 0);
        assert_eq!(l.hit_test(Point::new(px(0.0), px(0.0))), None);
    }

    #[test]
    fn hit_test_snaps_to_the_nearest_cluster_edge() {
        let run = ShapedRun {
            font: fid(0),
            font_size: px(12.0),
            rtl: false,
            glyphs: vec![
                ShapedGlyph {
                    glyph: GlyphId(1),
                    font: fid(0),
                    position: Point::new(px(0.0), Px::ZERO),
                    advance: px(10.0),
                    cluster: 0..1,
                    rtl: false,
                },
                ShapedGlyph {
                    glyph: GlyphId(2),
                    font: fid(0),
                    position: Point::new(px(10.0), Px::ZERO),
                    advance: px(10.0),
                    cluster: 1..2,
                    rtl: false,
                },
            ],
            source: 0..2,
        };
        let layout = TextLayout {
            lines: vec![TextLine {
                runs: vec![run],
                baseline: px(10.0),
                height: px(14.0),
                width: px(20.0),
                source: 0..2,
            }],
            size: Size::new(px(20.0), px(14.0)),
            max_width: None,
        };
        assert_eq!(layout.hit_test(Point::new(px(1.0), px(5.0))), Some(0));
        assert_eq!(layout.hit_test(Point::new(px(11.0), px(5.0))), Some(1));
        assert_eq!(layout.hit_test(Point::new(px(19.0), px(5.0))), Some(2));
    }

    #[test]
    fn hit_test_picks_the_line_the_point_is_actually_in() {
        // Two stacked lines of height 20: line 0 owns y in [0, 20), line 1 owns
        // [20, 40). Testing against `baseline + height` instead of the line's
        // own band would give line 0 everything up to y = 35.
        let g = |x: f32, cluster: core::ops::Range<usize>| ShapedGlyph {
            glyph: GlyphId(1),
            font: fid(0),
            position: Point::new(px(x), Px::ZERO),
            advance: px(10.0),
            cluster,
            rtl: false,
        };
        let line = |baseline: f32, cluster: core::ops::Range<usize>| TextLine {
            runs: vec![ShapedRun {
                font: fid(0),
                font_size: px(12.0),
                rtl: false,
                glyphs: vec![g(0.0, cluster.clone())],
                source: cluster.clone(),
            }],
            baseline: px(baseline),
            height: px(20.0),
            width: px(10.0),
            source: cluster,
        };
        let layout = TextLayout {
            lines: vec![line(15.0, 0..1), line(35.0, 1..2)],
            size: Size::new(px(10.0), px(40.0)),
            max_width: None,
        };
        assert_eq!(layout.hit_test(Point::new(px(1.0), px(5.0))), Some(0));
        assert_eq!(layout.hit_test(Point::new(px(1.0), px(25.0))), Some(1), "click in line 2");
        // Above the first line and below the last still land somewhere sane.
        assert_eq!(layout.hit_test(Point::new(px(1.0), px(-50.0))), Some(0));
        assert_eq!(layout.hit_test(Point::new(px(1.0), px(500.0))), Some(1));
    }

    #[test]
    fn run_advance_sums_its_glyphs() {
        let g = |x: f32| ShapedGlyph {
            glyph: GlyphId(1),
            font: fid(0),
            position: Point::new(px(x), Px::ZERO),
            advance: px(7.0),
            cluster: 0..1,
            rtl: false,
        };
        let r = ShapedRun {
            font: fid(0),
            font_size: px(12.0),
            rtl: false,
            glyphs: vec![g(0.0), g(7.0), g(14.0)],
            source: 0..3,
        };
        assert_eq!(r.advance(), px(21.0));
    }
}
