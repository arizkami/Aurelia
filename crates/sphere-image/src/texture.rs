//! Texture description, resizing and mip generation.
//!
//! The engine draws a lot of small images from large sources: a 4000 px cover
//! art in a 40 px slot, a 512 px knob sprite at 28 px. Sampling a full-size
//! texture at that ratio skips most of the texels, so the result crawls and
//! shimmers whenever the value or the window moves by a fraction of a pixel.
//! Prefiltered levels are the fix, and this module produces them.
//!
//! ## Filtering and alpha
//!
//! Every resampling operation here runs on **premultiplied** data. Filtering
//! straight-alpha texels averages colours that are not actually visible, so the
//! fully transparent border texels of a sprite drag their nominal colour into
//! the visible edge: the halo. If the input is straight, it is premultiplied for
//! the duration of the operation and converted back afterwards, so the caller's
//! representation is preserved and the filtering is still correct.
//!
//! ## Filtering and sRGB
//!
//! Resampling happens in the stored encoding, which for decoded assets is sRGB
//! rather than linear light. A colorimetrically exact reduction would linearise,
//! average, and re-encode. Sphere does not, for the same reason it does not
//! linearise on decode: the intermediate would have to be wider than 8 bits to
//! avoid banding the darks, and every other 2D UI stack (Skia, Direct2D, the
//! browsers' `image-rendering` path) makes the same trade. The visible effect is
//! that a heavily minified checkerboard reads very slightly darker than its
//! mathematical average.

use image::RgbaImage;
use image::imageops::{self, FilterType};
use sphere_core::ImageError;

use crate::decode::{AlphaMode, ColorSpace, DecodeOptions, DecodedImage, premultiply_rgba8};

/// The GPU texel format a [`DecodedImage`] should be uploaded as.
///
/// This crate does not touch the GPU, but it is the only place that knows
/// whether a buffer is sRGB-encoded, so it is the right place to name the
/// format the backend must pick.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum TexelFormat {
    /// 8-bit RGBA with a hardware sRGB transfer on sample.
    ///
    /// The correct choice for anything a human looked at while authoring it:
    /// photographs, icons, skins. The sampler decodes to linear light before
    /// filtering and before the shader sees it, which is both free and more
    /// precise than doing it in software.
    Rgba8UnormSrgb,
    /// 8-bit RGBA with no transfer applied.
    ///
    /// For data that happens to be stored in an image: masks, coverage maps,
    /// lookup tables, normal maps. Applying an sRGB transfer to these corrupts
    /// them, usually in a way that looks almost right.
    Rgba8Unorm,
}

impl TexelFormat {
    /// True when the sampler applies the sRGB transfer.
    #[inline]
    pub const fn is_srgb(self) -> bool {
        matches!(self, TexelFormat::Rgba8UnormSrgb)
    }

    /// The format matching an image's declared colour space.
    #[inline]
    pub const fn for_image(image: &DecodedImage) -> Self {
        match image.color_space() {
            ColorSpace::Srgb => TexelFormat::Rgba8UnormSrgb,
            ColorSpace::Linear => TexelFormat::Rgba8Unorm,
        }
    }
}

/// Everything the backend needs to allocate a texture for a decoded image.
///
/// A plain description rather than a builder: it is copied into a wgpu
/// descriptor at the call site, and interposing a builder would only add a
/// second place for the mip count to disagree with the level list.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct TextureDesc {
    /// Width of mip level 0, in texels.
    pub width: u32,
    /// Height of mip level 0, in texels.
    pub height: u32,
    /// The texel format, which encodes whether the sampler applies sRGB.
    pub format: TexelFormat,
    /// Number of mip levels to allocate, at least one.
    pub mip_levels: u32,
    /// Whether the RGB channels are premultiplied, which decides the blend
    /// state the draw must use.
    pub alpha: AlphaMode,
}

impl TextureDesc {
    /// A single-level description for an image, with no mips.
    pub fn for_image(image: &DecodedImage) -> Self {
        Self {
            width: image.width(),
            height: image.height(),
            format: TexelFormat::for_image(image),
            mip_levels: 1,
            alpha: image.alpha_mode(),
        }
    }

    /// A description with a full mip chain down to 1x1.
    pub fn for_image_with_mips(image: &DecodedImage) -> Self {
        Self {
            mip_levels: mip_level_count(image.width(), image.height()),
            ..Self::for_image(image)
        }
    }

    /// Bytes of texel storage, including every mip level.
    ///
    /// A full chain adds about a third to the base level, which is what the
    /// cache must account for before deciding it is within budget.
    ///
    /// Every field of this struct is public, so this has to survive a descriptor
    /// a caller filled in by hand. Two things follow. The level count is clamped
    /// to the real chain length, because there is no level below 1x1 and a
    /// `mip_levels` of `u32::MAX` would otherwise spin for billions of
    /// iterations adding four bytes at a time. The arithmetic saturates, because
    /// `width * height * 4` overshoots even a 64-bit accumulator at extreme
    /// dimensions, and a wrapped total would understate the memory a texture
    /// costs — the exact number the cache uses to decide it is within budget.
    pub fn byte_len(&self) -> usize {
        let levels = self.mip_levels.max(1).min(mip_level_count(self.width, self.height));
        let mut total = 0u64;
        let (mut w, mut h) = (self.width, self.height);
        for _ in 0..levels {
            let level = u64::from(w).saturating_mul(u64::from(h)).saturating_mul(4);
            total = total.saturating_add(level);
            w = (w / 2).max(1);
            h = (h / 2).max(1);
        }
        usize::try_from(total).unwrap_or(usize::MAX)
    }
}

/// The number of mip levels in a full chain down to 1x1.
///
/// `floor(log2(max(w, h))) + 1`, the standard definition; a 1x1 image has one
/// level, a 40x30 image has six.
#[inline]
pub fn mip_level_count(width: u32, height: u32) -> u32 {
    32 - width.max(height).max(1).leading_zeros()
}

/// The resampling kernel used for downscaling.
///
/// Named by intent rather than by kernel so callers pick on the trade-off they
/// care about, which is quality against time.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub enum ResizeFilter {
    /// Point sampling. Only appropriate for pixel art and for exact integer
    /// upscales, where any interpolation is a defect.
    Nearest,
    /// Triangle (tent) filter. For an exact halving this degenerates to a 2x2
    /// box average, which is precisely what a mip level wants, and it is the
    /// cheapest filter that does not alias.
    #[default]
    Box,
    /// Catmull-Rom cubic. Slightly sharper than the tent filter for non-integer
    /// ratios, at a moderate cost.
    CatmullRom,
    /// Lanczos with a 3-lobe window. The sharpest option and the right one for a
    /// one-off thumbnail; too expensive to run over a whole mip chain every time
    /// an asset loads, and its ringing is visible on hard-edged UI art.
    Lanczos3,
}

impl ResizeFilter {
    const fn to_codec(self) -> FilterType {
        match self {
            ResizeFilter::Nearest => FilterType::Nearest,
            ResizeFilter::Box => FilterType::Triangle,
            ResizeFilter::CatmullRom => FilterType::CatmullRom,
            ResizeFilter::Lanczos3 => FilterType::Lanczos3,
        }
    }
}

/// Resamples an image to an exact size.
///
/// The result keeps the input's alpha representation and colour space; only the
/// dimensions change. Filtering itself always runs premultiplied, see the module
/// documentation.
///
/// Returns [`ImageError::Decode`] for a zero target size and
/// [`ImageError::TooLarge`] for a target beyond the default device limit; a
/// resize is engine-driven, so those represent a caller bug rather than
/// untrusted input, but they are still values rather than panics.
pub fn resize(
    image: &DecodedImage,
    width: u32,
    height: u32,
    filter: ResizeFilter,
) -> Result<DecodedImage, ImageError> {
    let options = DecodeOptions::default();
    if width == image.width() && height == image.height() {
        return Ok(image.clone());
    }
    // Resampling allocates a whole new buffer, so validate the target before
    // doing any work; from_rgba8 would catch it afterwards, too late to matter.
    if width == 0 || height == 0 {
        return Err(ImageError::Decode(format!(
            "cannot resize to {width}x{height}: a texture needs at least one texel"
        )));
    }
    if width > options.max_dimension || height > options.max_dimension {
        return Err(ImageError::TooLarge { width, height, max: options.max_dimension });
    }

    let mut working = image.data().to_vec();
    let was_straight = image.alpha_mode() == AlphaMode::Straight;
    if was_straight {
        premultiply_rgba8(&mut working);
    }
    let source = RgbaImage::from_raw(image.width(), image.height(), working).ok_or_else(|| {
        ImageError::Decode(format!(
            "internal: {}x{} buffer rejected by the resampler",
            image.width(),
            image.height()
        ))
    })?;

    let resized = imageops::resize(&source, width, height, filter.to_codec());
    let out = DecodedImage::from_rgba8(
        width,
        height,
        resized.into_raw(),
        AlphaMode::Premultiplied,
        image.color_space(),
        &options,
    )?;
    Ok(out.into_alpha_mode(image.alpha_mode()))
}

/// Resamples so that neither edge exceeds `max_edge`, preserving aspect ratio.
///
/// An image already within the limit is returned unchanged rather than being
/// pointlessly resampled, which also means a thumbnail request never *upscales*.
/// Rounding is to nearest with a floor of one texel, so a 4000x3 panorama
/// reduced to 64 comes back as 64x1 rather than 64x0.
pub fn thumbnail(
    image: &DecodedImage,
    max_edge: u32,
    filter: ResizeFilter,
) -> Result<DecodedImage, ImageError> {
    if max_edge == 0 {
        return Err(ImageError::Decode("thumbnail edge must be at least one texel".to_string()));
    }
    let longest = image.width().max(image.height());
    if longest <= max_edge {
        return Ok(image.clone());
    }
    let scale = f64::from(max_edge) / f64::from(longest);
    let width = ((f64::from(image.width()) * scale).round() as u32).max(1);
    let height = ((f64::from(image.height()) * scale).round() as u32).max(1);
    resize(image, width, height, filter)
}

/// A full mip pyramid: level 0 is the original, each level half the size of the
/// last, down to 1x1.
///
/// Levels are stored as complete [`DecodedImage`]s so they can be uploaded,
/// inspected and cached with exactly the same code as any other image.
#[derive(Clone, Debug)]
pub struct MipChain {
    levels: Vec<DecodedImage>,
}

impl MipChain {
    /// Builds the pyramid from a base image.
    ///
    /// Each level is reduced from the level above rather than from level 0.
    /// That is both cheaper (a quarter of the work at each step, geometric
    /// rather than repeated full-size resampling) and better: a chain of exact
    /// halvings is a clean box reduction, whereas resampling 4000 px straight
    /// down to 15 px in one step with a 3-lobe kernel misses most of the source.
    pub fn generate(base: &DecodedImage, filter: ResizeFilter) -> Result<Self, ImageError> {
        let count = mip_level_count(base.width(), base.height()) as usize;
        let mut levels = Vec::with_capacity(count);
        levels.push(base.clone());
        let (mut w, mut h) = base.dimensions();
        while w > 1 || h > 1 {
            w = (w / 2).max(1);
            h = (h / 2).max(1);
            let next = match levels.last() {
                Some(previous) => resize(previous, w, h, filter)?,
                // Unreachable: level 0 was pushed above and nothing removes it.
                None => break,
            };
            levels.push(next);
        }
        debug_assert_eq!(levels.len(), count);
        Ok(Self { levels })
    }

    /// A chain holding only level 0.
    ///
    /// Lets a consumer treat "no mips yet" and "a full pyramid" with the same
    /// type, which is what keeps the cache from storing the base image twice.
    #[inline]
    pub fn single(base: DecodedImage) -> Self {
        Self { levels: vec![base] }
    }

    /// True when the chain has levels beyond the base.
    #[inline]
    pub fn has_mips(&self) -> bool {
        self.levels.len() > 1
    }

    /// The levels, largest first. Never empty.
    #[inline]
    pub fn levels(&self) -> &[DecodedImage] {
        &self.levels
    }

    /// Number of levels, at least one.
    #[inline]
    pub fn len(&self) -> usize {
        self.levels.len()
    }

    /// Always false; a chain always contains at least level 0.
    ///
    /// Present because `len` without `is_empty` is a lint, and because a caller
    /// writing generic code over collections should get the right answer.
    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// One level, or `None` when the index is past the smallest.
    #[inline]
    pub fn level(&self, index: usize) -> Option<&DecodedImage> {
        self.levels.get(index)
    }

    /// The base level.
    #[inline]
    pub fn base(&self) -> &DecodedImage {
        &self.levels[0]
    }

    /// Consumes the chain, yielding level 0 and dropping the rest.
    ///
    /// Returns `Option` only because the compiler cannot see that a chain is
    /// always non-empty; every constructor guarantees it, so `None` is
    /// unreachable and returning it beats an `expect` in a library.
    #[inline]
    pub fn into_base(self) -> Option<DecodedImage> {
        self.levels.into_iter().next()
    }

    /// Total bytes across every level.
    pub fn byte_len(&self) -> usize {
        self.levels.iter().map(DecodedImage::byte_len).sum()
    }

    /// The level whose texel density best matches a draw scale.
    ///
    /// `scale` is destination extent divided by source extent, so `0.25` means
    /// the image is being drawn at a quarter size and level 2 is the match. The
    /// result is clamped into range, and any scale at or above 1 (magnification)
    /// selects level 0, because there is no level with *more* detail.
    pub fn level_for_scale(&self, scale: f32) -> usize {
        if !scale.is_finite() || scale >= 1.0 {
            return 0;
        }
        if scale <= 0.0 {
            return self.levels.len() - 1;
        }
        let level = (1.0 / scale).log2().floor();
        (level as usize).min(self.levels.len() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::ColorSpace;

    fn solid(w: u32, h: u32, texel: [u8; 4], alpha: AlphaMode) -> DecodedImage {
        let data: Vec<u8> = (0..w * h).flat_map(|_| texel).collect();
        DecodedImage::from_rgba8(w, h, data, alpha, ColorSpace::Srgb, &DecodeOptions::default())
            .expect("valid image")
    }

    #[test]
    fn mip_level_count_matches_the_standard_definition() {
        assert_eq!(mip_level_count(1, 1), 1);
        assert_eq!(mip_level_count(2, 1), 2);
        assert_eq!(mip_level_count(2, 2), 2);
        assert_eq!(mip_level_count(3, 1), 2);
        assert_eq!(mip_level_count(40, 30), 6);
        assert_eq!(mip_level_count(1024, 1024), 11);
        // Degenerate input must not underflow into a huge count.
        assert_eq!(mip_level_count(0, 0), 1);
    }

    #[test]
    fn a_chain_halves_down_to_one_by_one() {
        let base = solid(8, 5, [10, 20, 30, 255], AlphaMode::Premultiplied);
        let chain = MipChain::generate(&base, ResizeFilter::Box).expect("chain");
        let dims: Vec<_> = chain.levels().iter().map(DecodedImage::dimensions).collect();
        assert_eq!(dims, vec![(8, 5), (4, 2), (2, 1), (1, 1)]);
        assert_eq!(chain.len(), mip_level_count(8, 5) as usize);
        assert_eq!(chain.base().dimensions(), (8, 5));
        assert!(!chain.is_empty());
    }

    #[test]
    fn a_one_by_one_image_still_produces_a_valid_chain() {
        let base = solid(1, 1, [1, 2, 3, 4], AlphaMode::Premultiplied);
        let chain = MipChain::generate(&base, ResizeFilter::Box).expect("chain");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.level(0).map(DecodedImage::dimensions), Some((1, 1)));
        assert!(chain.level(1).is_none());
    }

    #[test]
    fn an_extremely_anisotropic_image_never_reaches_a_zero_extent() {
        // 1024x1 is the shape that makes a naive `w /= 2; h /= 2` produce a
        // zero-height level and then a zero-sized allocation.
        let base = solid(1024, 1, [255, 255, 255, 255], AlphaMode::Premultiplied);
        let chain = MipChain::generate(&base, ResizeFilter::Box).expect("chain");
        assert_eq!(chain.len(), 11);
        for level in chain.levels() {
            assert!(level.width() >= 1 && level.height() >= 1);
        }
        assert_eq!(chain.levels().last().map(DecodedImage::dimensions), Some((1, 1)));
    }

    #[test]
    fn chain_byte_length_is_about_four_thirds_of_the_base() {
        let base = solid(64, 64, [0, 0, 0, 255], AlphaMode::Premultiplied);
        let chain = MipChain::generate(&base, ResizeFilter::Box).expect("chain");
        let ratio = chain.byte_len() as f64 / base.byte_len() as f64;
        assert!((ratio - 4.0 / 3.0).abs() < 0.01, "ratio was {ratio}");
        // The descriptor must agree with the chain it describes.
        assert_eq!(TextureDesc::for_image_with_mips(&base).byte_len(), chain.byte_len());
    }

    #[test]
    fn level_for_scale_picks_by_octave() {
        let base = solid(64, 64, [0, 0, 0, 255], AlphaMode::Premultiplied);
        let chain = MipChain::generate(&base, ResizeFilter::Box).expect("chain");
        assert_eq!(chain.level_for_scale(2.0), 0, "magnification uses the base level");
        assert_eq!(chain.level_for_scale(1.0), 0);
        assert_eq!(chain.level_for_scale(0.6), 0);
        assert_eq!(chain.level_for_scale(0.5), 1);
        assert_eq!(chain.level_for_scale(0.25), 2);
        assert_eq!(chain.level_for_scale(0.2), 2);
        // Degenerate scales must clamp, not index out of range.
        assert_eq!(chain.level_for_scale(0.0), chain.len() - 1);
        assert_eq!(chain.level_for_scale(-1.0), chain.len() - 1);
        assert_eq!(chain.level_for_scale(1e-30), chain.len() - 1);
        assert_eq!(chain.level_for_scale(f32::NAN), 0);
        assert_eq!(chain.level_for_scale(f32::INFINITY), 0);
    }

    #[test]
    fn downscaling_a_flat_colour_preserves_it() {
        // A resampler that is broken in any interesting way (wrong stride, wrong
        // channel order, alpha leaking into colour) fails this.
        let base = solid(37, 21, [200, 100, 50, 255], AlphaMode::Premultiplied);
        let small = resize(&base, 8, 5, ResizeFilter::Lanczos3).expect("resize");
        assert_eq!(small.dimensions(), (8, 5));
        for texel in small.data().chunks_exact(4) {
            assert!((i32::from(texel[0]) - 200).abs() <= 1, "{texel:?}");
            assert!((i32::from(texel[1]) - 100).abs() <= 1, "{texel:?}");
            assert!((i32::from(texel[2]) - 50).abs() <= 1, "{texel:?}");
            assert_eq!(texel[3], 255);
        }
    }

    #[test]
    fn downscaling_straight_alpha_does_not_produce_a_halo() {
        // The regression this whole module exists for. Left half: opaque black.
        // Right half: transparent white. Naively averaging straight-alpha texels
        // drags the invisible white into the boundary and the edge turns grey.
        let (w, h) = (16u32, 2u32);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..h {
            for x in 0..w {
                if x < w / 2 {
                    data.extend_from_slice(&[0, 0, 0, 255]);
                } else {
                    data.extend_from_slice(&[255, 255, 255, 0]);
                }
            }
        }
        let base = DecodedImage::from_rgba8(
            w,
            h,
            data,
            AlphaMode::Straight,
            ColorSpace::Srgb,
            &DecodeOptions::default().with_alpha(AlphaMode::Straight),
        )
        .expect("image");
        let small = resize(&base, 4, 1, ResizeFilter::Box).expect("resize");
        assert_eq!(small.alpha_mode(), AlphaMode::Straight, "representation must survive");
        // Every texel that retains any opacity must still be black, not grey.
        for texel in small.data().chunks_exact(4) {
            if texel[3] > 8 {
                assert!(texel[0] < 24, "halo: colour {texel:?} leaked into a visible texel");
            }
        }
    }

    #[test]
    fn resize_to_the_same_size_is_an_exact_copy() {
        let base = solid(5, 7, [1, 2, 3, 200], AlphaMode::Premultiplied);
        let same = resize(&base, 5, 7, ResizeFilter::Lanczos3).expect("resize");
        assert_eq!(same.data(), base.data(), "a no-op resize must not perturb texels");
    }

    #[test]
    fn resize_rejects_degenerate_targets() {
        let base = solid(4, 4, [0, 0, 0, 255], AlphaMode::Premultiplied);
        assert!(matches!(resize(&base, 0, 4, ResizeFilter::Box), Err(ImageError::Decode(_))));
        assert!(matches!(resize(&base, 4, 0, ResizeFilter::Box), Err(ImageError::Decode(_))));
        match resize(&base, 100_000, 4, ResizeFilter::Box) {
            Err(ImageError::TooLarge { width, .. }) => assert_eq!(width, 100_000),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn thumbnail_preserves_aspect_ratio_and_never_upscales() {
        let base = solid(4000, 1000, [0, 0, 0, 255], AlphaMode::Premultiplied);
        let thumb = thumbnail(&base, 40, ResizeFilter::Lanczos3).expect("thumb");
        assert_eq!(thumb.dimensions(), (40, 10));

        // Already small enough: returned untouched, not resampled up.
        let small = solid(8, 4, [0, 0, 0, 255], AlphaMode::Premultiplied);
        assert_eq!(thumbnail(&small, 64, ResizeFilter::Box).unwrap().dimensions(), (8, 4));

        assert!(thumbnail(&base, 0, ResizeFilter::Box).is_err());
    }

    #[test]
    fn thumbnail_of_an_extreme_aspect_ratio_keeps_one_texel_of_height() {
        // 4000x3 down to 64: the height rounds to 0.048, which must floor to 1.
        let base = solid(4000, 3, [0, 0, 0, 255], AlphaMode::Premultiplied);
        let thumb = thumbnail(&base, 64, ResizeFilter::Box).expect("thumb");
        assert_eq!(thumb.dimensions(), (64, 1));
    }

    #[test]
    fn a_hand_filled_descriptor_cannot_overflow_or_spin() {
        // Every field is public, so byte_len has to survive nonsense without
        // panicking on overflow (debug), wrapping to a small total (release), or
        // looping four billion times.
        let absurd = TextureDesc {
            width: u32::MAX,
            height: u32::MAX,
            format: TexelFormat::Rgba8Unorm,
            mip_levels: u32::MAX,
            alpha: AlphaMode::Premultiplied,
        };
        // Saturated rather than wrapped: a wrapped total would understate the
        // cost of a texture, which is the number the cache budgets against.
        assert_eq!(absurd.byte_len(), usize::MAX);

        // Levels below 1x1 do not exist, so over-declaring them adds nothing.
        let over_declared = TextureDesc {
            width: 8,
            height: 8,
            format: TexelFormat::Rgba8Unorm,
            mip_levels: 1000,
            alpha: AlphaMode::Premultiplied,
        };
        let honest = TextureDesc { mip_levels: mip_level_count(8, 8), ..over_declared };
        assert_eq!(over_declared.byte_len(), honest.byte_len());
        assert_eq!(honest.byte_len(), (64 + 16 + 4 + 1) * 4);

        // A zero level count still describes level 0.
        let zero = TextureDesc { mip_levels: 0, ..over_declared };
        assert_eq!(zero.byte_len(), 8 * 8 * 4);
    }

    #[test]
    fn texel_format_follows_the_colour_space() {
        let srgb = solid(2, 2, [0, 0, 0, 255], AlphaMode::Premultiplied);
        assert_eq!(TexelFormat::for_image(&srgb), TexelFormat::Rgba8UnormSrgb);
        assert!(TexelFormat::for_image(&srgb).is_srgb());

        let linear = DecodedImage::from_rgba8(
            2,
            2,
            vec![0; 16],
            AlphaMode::Premultiplied,
            ColorSpace::Linear,
            &DecodeOptions::default(),
        )
        .unwrap();
        assert_eq!(TexelFormat::for_image(&linear), TexelFormat::Rgba8Unorm);
        assert!(!TexelFormat::for_image(&linear).is_srgb());
    }

    #[test]
    fn texture_desc_describes_the_image_it_came_from() {
        let img = solid(64, 32, [0, 0, 0, 128], AlphaMode::Premultiplied);
        let flat = TextureDesc::for_image(&img);
        assert_eq!((flat.width, flat.height, flat.mip_levels), (64, 32, 1));
        assert_eq!(flat.alpha, AlphaMode::Premultiplied);
        assert_eq!(flat.byte_len(), img.byte_len());

        let mipped = TextureDesc::for_image_with_mips(&img);
        assert_eq!(mipped.mip_levels, 7);
        assert!(mipped.byte_len() > flat.byte_len());
    }
}
