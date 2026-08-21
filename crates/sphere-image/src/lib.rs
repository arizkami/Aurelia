//! # sphere-image
//!
//! Image decoding, caching and texture preparation for `SphereGraphicEngine`.
//!
//! Everything an application hands the engine — a PNG on disk, a JPEG embedded
//! in a preset, a buffer produced by another library — arrives here and leaves
//! as a [`DecodedImage`]: tightly packed RGBA8, one byte per channel, top-left
//! origin. The renderer therefore has exactly one texel layout to upload, and
//! every format quirk is resolved once, at load time, instead of per frame.
//!
//! ```no_run
//! use sphere_core::{px, rect};
//! use sphere_image::{ImageCache, ImageFit, resolve_fit};
//!
//! # fn asset_bytes() -> Vec<u8> { Vec::new() }
//! let mut cache = ImageCache::new(64 * 1024 * 1024);
//! let bytes = asset_bytes();
//!
//! // Loading the same bytes again returns this id without decoding again.
//! let id = cache.load_bytes(&bytes)?;
//! let natural = cache.get(id).expect("just loaded").natural_size();
//!
//! // Where the texels land in a 200x120 slot, letterboxed.
//! let rects = resolve_fit(
//!     ImageFit::Contain,
//!     natural,
//!     rect(px(0.0), px(0.0), px(200.0), px(120.0)),
//! );
//! assert!(!rects.is_empty());
//! # Ok::<(), sphere_core::ImageError>(())
//! ```
//!
//! ## Alpha: `DecodedImage` is premultiplied by default
//!
//! The renderer blends premultiplied alpha, so decoding produces premultiplied
//! data and every [`DecodedImage`] carries an explicit [`AlphaMode`] tag saying
//! so. This is the single most consequential decision in the crate, and it is
//! tagged rather than assumed because getting it wrong is invisible in review
//! and obvious on screen: uploading straight-alpha texels to a premultiplied
//! pipeline puts a dark fringe around every soft edge, and premultiplying twice
//! makes semi-transparent art wash out.
//!
//! Two properties follow, both of which this crate relies on:
//!
//! - **Filtering is only correct on premultiplied data.** The transparent texels
//!   just outside a sprite still carry a colour in a straight-alpha buffer, and
//!   bilinear sampling or mip reduction averages that invisible colour into the
//!   visible edge. That is the halo. Premultiplying first, with the RGB of
//!   fully transparent texels cleared to zero, makes the average correct.
//! - **The conversion is lossy in one direction.** Premultiplying quantises to 8
//!   bits, so recovering straight alpha drifts by up to about `255 / (2 * alpha)`
//!   per channel. Opaque texels round-trip exactly; a texel at `alpha == 0` has
//!   genuinely lost its colour. [`DecodeOptions::with_alpha`] asks for straight
//!   output when a caller is re-encoding rather than drawing.
//!
//! ## sRGB: the data stays encoded, the sampler does the transfer
//!
//! Decoded pixels are **sRGB-encoded, non-linear** values with a linear alpha
//! channel, exactly as the file stored them. Nothing here linearises them. The
//! image is uploaded as [`TexelFormat::Rgba8UnormSrgb`] and the GPU's sampler
//! performs the transfer in hardware, before filtering and before the shader
//! sees a value. Decoding to linear 8-bit in software instead would cost about
//! two bits of precision in the shadows and band every gradient; doing it in the
//! shader after the sampler would apply it after filtering, which is wrong.
//!
//! One consequence, documented rather than hidden: premultiplication and
//! resampling both operate on those encoded values rather than on linear light.
//! That is the same trade Skia, Direct2D and the browsers make for 8-bit
//! surfaces. Data textures that are not colour — masks, LUTs, normal maps —
//! should declare [`ColorSpace::Linear`] so they get [`TexelFormat::Rgba8Unorm`]
//! and no transfer at all.
//!
//! ## Caching
//!
//! [`ImageCache`] is content addressed: the same bytes, or the same file,
//! resolve to the same [`ImageId`] and decode exactly once. It holds a byte
//! budget, evicts least-recently-used entries, and will not invalidate a handle
//! that is pinned or that was touched during the current frame. See the
//! [`cache`] module documentation for the eviction contract.
//!
//! ## Errors
//!
//! Malformed input is a normal runtime condition, not a bug: a user can point a
//! host at a truncated PNG or a 60000x60000 pixel bomb. Nothing in this crate
//! panics on input, and an image whose header declares more than the configured
//! limits is rejected before a buffer is allocated for it.

#![deny(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod cache;
pub mod decode;
pub mod fit;
pub mod texture;

mod hash;

pub use cache::{CacheStats, ContentKey, DEFAULT_BUDGET_BYTES, Evicted, ImageCache};
pub use decode::{
    AlphaMode, ColorSpace, DEFAULT_MAX_DECODED_BYTES, DEFAULT_MAX_DIMENSION, DecodeOptions,
    DecodedImage, ImageFormatKind, ImageSource, MagicVerdict, decode, decode_bytes, decode_path,
    premultiply_rgba8, unpremultiply_rgba8,
};
pub use fit::{FitRects, resolve as resolve_fit};
pub use texture::{
    MipChain, ResizeFilter, TexelFormat, TextureDesc, mip_level_count, resize, thumbnail,
};

// Re-exported so consumers do not need a direct `sphere-core` dependency just to
// name the types this crate's signatures speak in.
pub use sphere_core::{ImageError, ImageFit, ImageId, TextureId};

#[cfg(test)]
mod integration_tests {
    use super::*;
    use sphere_core::{px, rect};

    /// A 2x2 PNG with a fully transparent white texel in the corner, which is
    /// the shape that exposes both the halo bug and a premultiply mix-up.
    fn tricky_png() -> Vec<u8> {
        use image::ImageEncoder;
        let rgba: Vec<u8> = vec![
            255, 255, 255, 0, // transparent white: must not bleed
            255, 0, 0, 255, // opaque red
            0, 255, 0, 128, // half-transparent green
            0, 0, 255, 255, // opaque blue
        ];
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&rgba, 2, 2, image::ExtendedColorType::Rgba8)
            .expect("encode");
        out
    }

    #[test]
    fn the_whole_pipeline_agrees_on_one_representation() {
        let png = tricky_png();
        let mut cache = ImageCache::new(1024 * 1024);

        let id = cache.load_bytes(&png).expect("decode");
        let image = cache.get(id).expect("live");

        // Premultiplied by default, sRGB, and the transparent texel is cleared.
        assert_eq!(image.alpha_mode(), AlphaMode::Premultiplied);
        assert_eq!(image.color_space(), ColorSpace::Srgb);
        assert_eq!(image.texel(0, 0), Some([0, 0, 0, 0]));
        assert_eq!(image.texel(1, 0), Some([255, 0, 0, 255]));
        // 255 * 128 / 255 rounds to 128 in the green channel.
        assert_eq!(image.texel(0, 1), Some([0, 128, 0, 128]));
        assert!(!image.is_opaque());

        // The texel format follows from the colour space, not from a guess.
        assert_eq!(TexelFormat::for_image(image), TexelFormat::Rgba8UnormSrgb);
        assert!(TextureDesc::for_image(image).format.is_srgb());

        // ...and the same bytes never decode twice.
        assert_eq!(cache.load_bytes(&png).expect("cached"), id);
        assert_eq!(cache.stats().decodes, 1);
    }

    #[test]
    fn a_thumbnail_of_a_cached_image_keeps_its_representation() {
        let png = tricky_png();
        let mut cache = ImageCache::new(1024 * 1024);
        let id = cache.load_bytes(&png).expect("decode");
        let image = cache.get(id).expect("live").clone();

        let small = thumbnail(&image, 1, ResizeFilter::Box).expect("thumbnail");
        assert_eq!(small.dimensions(), (1, 1));
        assert_eq!(small.alpha_mode(), AlphaMode::Premultiplied);
        assert_eq!(small.color_space(), ColorSpace::Srgb);
    }

    #[test]
    fn fit_resolution_composes_with_a_cached_image() {
        let png = tricky_png();
        let mut cache = ImageCache::new(1024 * 1024);
        let id = cache.load_bytes(&png).expect("decode");
        let natural = cache.get(id).expect("live").natural_size();

        let dest = rect(px(0.0), px(0.0), px(100.0), px(50.0));
        let rects = resolve_fit(ImageFit::Contain, natural, dest);
        // A square image in a 2:1 box: letterboxed to 50x50, centred.
        assert_eq!(rects.dest, rect(px(25.0), px(0.0), px(50.0), px(50.0)));
        assert_eq!(rects.source_uv(natural), rect(0.0, 0.0, 1.0, 1.0));
        assert!(!rects.is_empty());
    }

    #[test]
    fn the_public_surface_names_every_error_case() {
        // A compile-level check that the re-exports line up, plus a reminder
        // that every one of these is reachable from ordinary user input.
        let opts = DecodeOptions::default().with_max_dimension(1);
        assert!(matches!(
            decode_bytes(&tricky_png(), &opts),
            Err(ImageError::TooLarge { width: 2, height: 2, max: 1 })
        ));
        assert!(matches!(decode_bytes(b"", &DecodeOptions::default()), Err(ImageError::Decode(_))));
        assert!(matches!(
            decode_path(std::path::Path::new("W:/nope/x.png"), &DecodeOptions::default()),
            Err(ImageError::Io { .. })
        ));
    }
}
