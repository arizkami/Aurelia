//! Decoding image data into a normalised, GPU-ready RGBA8 buffer.
//!
//! Everything that enters the engine leaves this module as a [`DecodedImage`]:
//! tightly packed, 8 bits per channel, four channels, row-major, top-left
//! origin. The renderer therefore has exactly one texel layout to upload and one
//! sampler configuration to reason about, and every format-specific quirk (16-bit
//! PNGs, greyscale JPEGs, palettes, CMYK) is resolved here rather than leaking
//! into the draw path.
//!
//! ## Alpha
//!
//! See the crate-level documentation for the full argument. In short: a
//! [`DecodedImage`] carries an explicit [`AlphaMode`] tag, decoding defaults to
//! [`AlphaMode::Premultiplied`] because that is what the renderer blends, and
//! converting between the two representations is always an explicit call.
//!
//! ## Failure is normal
//!
//! A user can point a DAW at a truncated PNG, a renamed `.jpg` that is really a
//! ZIP, or a 60000x60000 pixel bomb. None of those are bugs in the host, so none
//! of them panic: they all become [`ImageError`] values.

use std::io::Cursor;
use std::path::Path;

use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use sphere_core::{ImageError, Px, Size, px};

use image::ImageError as CodecError;

/// The largest texture edge accepted by default, in texels.
///
/// 16384 is the `maxTextureDimension2D` guaranteed by WebGPU's `core` limit tier
/// and matched by every desktop backend Sphere targets. Hosts that queried a
/// smaller device limit should lower it via [`DecodeOptions::max_dimension`] so
/// the failure surfaces at load time rather than as a driver error mid-frame.
pub const DEFAULT_MAX_DIMENSION: u32 = 16_384;

/// The largest decoded RGBA8 buffer accepted by default, in bytes.
///
/// A 512 MiB ceiling still admits a 11585x11585 image while refusing the
/// classic decompression bomb, whose header declares a plausible-looking
/// dimension pair whose *product* is not plausible at all.
pub const DEFAULT_MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

/// Whether the RGB channels of a buffer have already been multiplied by alpha.
///
/// This is a property of the *data*, not a preference, which is why it travels
/// with the pixels instead of living in a configuration struct. Uploading
/// straight-alpha texels to a pipeline that blends premultiplied is the bug that
/// produces a dark fringe around every soft edge, and it is invisible until
/// someone looks closely at a shadow.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum AlphaMode {
    /// RGB is independent of alpha, the representation every file format stores.
    ///
    /// Filtering straight-alpha texels is unsound: the transparent texels just
    /// outside a sprite still carry a colour, and bilinear interpolation drags it
    /// into the visible edge.
    Straight,
    /// RGB has been multiplied by alpha; this is what the renderer blends and
    /// what any filtering (mip generation, bilinear sampling) must operate on.
    Premultiplied,
}

/// The transfer function the stored bytes are encoded with.
///
/// Sphere never silently linearises image data. A PNG or JPEG is sRGB-encoded,
/// stays sRGB-encoded in memory, and is uploaded to an sRGB texture format so
/// that the *sampler* performs the transfer in hardware, for free, at full
/// precision. Decoding to linear 8-bit in software would throw away roughly two
/// bits of precision in the darks and band every gradient.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ColorSpace {
    /// Non-linear sRGB-encoded values. Upload as `Rgba8UnormSrgb`.
    Srgb,
    /// Linear-light values, for data textures: masks, normal maps, LUTs.
    /// Upload as `Rgba8Unorm` so no transfer is applied.
    Linear,
}

/// A container format `sphere-image` can decode.
///
/// Deliberately narrower than what the backing codec crate supports: the engine
/// ships the three formats a plug-in UI actually needs, and every additional
/// codec is attack surface plus binary size in a process the host owns.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ImageFormatKind {
    /// PNG. Lossless, alpha, the default for UI assets.
    Png,
    /// JPEG. Lossy, no alpha, used for artwork and photographic backdrops.
    Jpeg,
    /// WebP, lossy or lossless, with alpha.
    WebP,
}

impl ImageFormatKind {
    /// Identifies the format from the leading magic bytes.
    ///
    /// Content sniffing rather than extension matching: file extensions are
    /// user-supplied metadata and are routinely wrong, and a `.png` that is
    /// really a JPEG should just load.
    ///
    /// Returns `None` both for data that matches no known signature and for data
    /// whose signature names a format this crate does not decode; use
    /// [`ImageFormatKind::classify`] when the distinction matters.
    pub fn from_magic(bytes: &[u8]) -> Option<Self> {
        match Self::classify(bytes) {
            MagicVerdict::Supported(kind) => Some(kind),
            _ => None,
        }
    }

    /// Identifies the format from magic bytes, distinguishing "unknown data"
    /// from "a real image format that Sphere does not decode".
    pub fn classify(bytes: &[u8]) -> MagicVerdict {
        match image::guess_format(bytes) {
            Ok(ImageFormat::Png) => MagicVerdict::Supported(ImageFormatKind::Png),
            Ok(ImageFormat::Jpeg) => MagicVerdict::Supported(ImageFormatKind::Jpeg),
            Ok(ImageFormat::WebP) => MagicVerdict::Supported(ImageFormatKind::WebP),
            Ok(other) => MagicVerdict::Recognised(format!("{other:?}")),
            Err(_) => MagicVerdict::Unknown,
        }
    }

    /// Guesses from a file extension, as a fallback when content sniffing fails.
    ///
    /// Only consulted after the magic bytes come up empty, because a header that
    /// disagrees with the extension is the header telling the truth.
    pub fn from_extension(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "png" => Some(ImageFormatKind::Png),
            "jpg" | "jpeg" | "jpe" | "jfif" => Some(ImageFormatKind::Jpeg),
            "webp" => Some(ImageFormatKind::WebP),
            _ => None,
        }
    }

    /// The IANA media type, for host APIs and drag-and-drop payloads.
    pub const fn mime(self) -> &'static str {
        match self {
            ImageFormatKind::Png => "image/png",
            ImageFormatKind::Jpeg => "image/jpeg",
            ImageFormatKind::WebP => "image/webp",
        }
    }

    /// True when the format can carry an alpha channel at all.
    ///
    /// Callers can use this to skip the premultiply pass, and to pick an opaque
    /// blend state without scanning the decoded pixels.
    pub const fn supports_alpha(self) -> bool {
        matches!(self, ImageFormatKind::Png | ImageFormatKind::WebP)
    }

    /// Maps onto the backing codec crate's format enum.
    const fn to_codec_format(self) -> ImageFormat {
        match self {
            ImageFormatKind::Png => ImageFormat::Png,
            ImageFormatKind::Jpeg => ImageFormat::Jpeg,
            ImageFormatKind::WebP => ImageFormat::WebP,
        }
    }
}

/// The outcome of sniffing a buffer's magic bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MagicVerdict {
    /// A format this crate decodes.
    Supported(ImageFormatKind),
    /// A real image format that this crate deliberately does not decode; the
    /// string names it so the error message can say which.
    Recognised(String),
    /// No known image signature.
    Unknown,
}

/// Limits and target representation applied while decoding.
///
/// Cloned rather than borrowed at call sites because it is three words and
/// pinning a lifetime to it would infect every cache signature.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DecodeOptions {
    /// Largest accepted width or height, in texels. Exceeding it yields
    /// [`ImageError::TooLarge`] *before* any pixel buffer is allocated.
    pub max_dimension: u32,
    /// Largest accepted decoded buffer, in bytes. Guards against headers whose
    /// individual dimensions are legal but whose product is not.
    pub max_decoded_bytes: u64,
    /// The alpha representation the decoded image should end up in.
    ///
    /// Defaults to [`AlphaMode::Premultiplied`]: the renderer blends
    /// premultiplied, and doing the conversion once at load time rather than per
    /// frame is the whole point of a decode cache.
    pub alpha: AlphaMode,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            max_dimension: DEFAULT_MAX_DIMENSION,
            max_decoded_bytes: DEFAULT_MAX_DECODED_BYTES,
            alpha: AlphaMode::Premultiplied,
        }
    }
}

impl DecodeOptions {
    /// Options with the device's real texture limit substituted in.
    pub fn with_max_dimension(mut self, max_dimension: u32) -> Self {
        self.max_dimension = max_dimension;
        self
    }

    /// Options that produce straight-alpha output.
    ///
    /// Useful for tools that re-encode an image, where premultiplying and
    /// unpremultiplying again would lose precision for no benefit.
    pub fn with_alpha(mut self, alpha: AlphaMode) -> Self {
        self.alpha = alpha;
        self
    }
}

/// One entry point for every way an image can arrive.
///
/// Having a single sum type means the cache has one `load` method rather than
/// three, and adding a source (an embedded asset, a memory-mapped file) is a
/// variant rather than a new API surface.
#[derive(Copy, Clone, Debug)]
pub enum ImageSource<'a> {
    /// An encoded PNG, JPEG or WebP file already in memory.
    Bytes(&'a [u8]),
    /// A path to an encoded file on disk.
    Path(&'a Path),
    /// An already-decoded RGBA8 buffer, tightly packed, row-major.
    Rgba8 {
        /// Width in texels.
        width: u32,
        /// Height in texels.
        height: u32,
        /// Exactly `width * height * 4` bytes.
        data: &'a [u8],
        /// Whether `data` is already premultiplied.
        alpha: AlphaMode,
        /// The transfer function `data` is encoded with.
        color_space: ColorSpace,
    },
}

impl<'a> ImageSource<'a> {
    /// An encoded file already in memory.
    pub const fn bytes(bytes: &'a [u8]) -> Self {
        ImageSource::Bytes(bytes)
    }

    /// An encoded file on disk.
    pub const fn path(path: &'a Path) -> Self {
        ImageSource::Path(path)
    }

    /// A straight-alpha sRGB RGBA8 buffer, the common case for raw pixels
    /// produced by another library.
    pub const fn rgba8(width: u32, height: u32, data: &'a [u8]) -> Self {
        ImageSource::Rgba8 {
            width,
            height,
            data,
            alpha: AlphaMode::Straight,
            color_space: ColorSpace::Srgb,
        }
    }
}

/// A decoded image in the engine's canonical texel layout.
///
/// Invariants, all upheld by the constructors:
/// - `data.len() == width as usize * height as usize * 4`
/// - `width > 0 && height > 0`
/// - channel order is R, G, B, A; rows run top to bottom, tightly packed
#[derive(Clone)]
pub struct DecodedImage {
    width: u32,
    height: u32,
    alpha: AlphaMode,
    color_space: ColorSpace,
    opaque: bool,
    data: Vec<u8>,
}

impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the pixel buffer: a tracing span carrying 16 MiB of hex is
        // its own kind of production incident.
        f.debug_struct("DecodedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("alpha", &self.alpha)
            .field("color_space", &self.color_space)
            .field("opaque", &self.opaque)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl DecodedImage {
    /// Wraps an existing RGBA8 buffer, validating the layout invariants.
    ///
    /// Returns [`ImageError::Decode`] when the buffer length disagrees with the
    /// declared dimensions, which is the failure mode of every hand-rolled
    /// stride calculation, and [`ImageError::TooLarge`] when the dimensions
    /// exceed `max_dimension`.
    pub fn from_rgba8(
        width: u32,
        height: u32,
        data: Vec<u8>,
        alpha: AlphaMode,
        color_space: ColorSpace,
        options: &DecodeOptions,
    ) -> Result<Self, ImageError> {
        validate_dimensions(width, height, options)?;
        // Computed in u64 rather than usize: the product can exceed a 32-bit
        // usize outright, and even on 64-bit it overflows for extreme declared
        // dimensions, which would wrap the expected length to zero and let an
        // empty buffer masquerade as a valid image of billions of texels.
        let expected = rgba8_byte_len(width, height);
        if data.len() as u64 != expected {
            return Err(ImageError::Decode(format!(
                "RGBA8 buffer is {} bytes but {width}x{height} needs exactly {expected}",
                data.len()
            )));
        }
        let opaque = data.iter().skip(3).step_by(4).all(|&a| a == 255);
        Ok(Self { width, height, alpha, color_space, opaque, data })
    }

    /// Width in texels. Never zero.
    #[inline]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in texels. Never zero.
    #[inline]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Dimensions as a `(width, height)` pair, for GPU upload calls.
    #[inline]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The image's natural size in logical pixels.
    ///
    /// One texel maps to one logical pixel at 100 % scaling; a `HiDPI` asset is
    /// scaled by the layout that draws it, not by this crate, because only the
    /// layout knows the destination rectangle.
    #[inline]
    pub fn natural_size(&self) -> Size<Px> {
        Size::new(px(self.width as f32), px(self.height as f32))
    }

    /// Bytes per row, always `width * 4` because rows are tightly packed.
    ///
    /// GPU uploads usually need a row pitch aligned to 256 bytes; that padding
    /// belongs to the backend's staging buffer, not to the decoded image.
    #[inline]
    pub const fn row_bytes(&self) -> usize {
        self.width as usize * 4
    }

    /// Total size of the pixel buffer in bytes.
    #[inline]
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    /// The RGBA8 pixel buffer.
    #[inline]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the image, returning the pixel buffer.
    #[inline]
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Which alpha representation [`DecodedImage::data`] holds.
    #[inline]
    pub const fn alpha_mode(&self) -> AlphaMode {
        self.alpha
    }

    /// Which transfer function [`DecodedImage::data`] is encoded with.
    #[inline]
    pub const fn color_space(&self) -> ColorSpace {
        self.color_space
    }

    /// True when every texel is fully opaque.
    ///
    /// Computed once at construction. The renderer uses it to pick an opaque
    /// blend state and to skip sorting the draw into the transparent pass, which
    /// is worth a scan of the alpha channel at load time.
    #[inline]
    pub const fn is_opaque(&self) -> bool {
        self.opaque
    }

    /// Reads one texel, or `None` when the coordinate is outside the image.
    ///
    /// Bounds-checked rather than panicking: hit testing against an image mask
    /// legitimately asks about coordinates just off the edge.
    #[inline]
    pub fn texel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = (y as usize * self.width as usize + x as usize) * 4;
        self.data.get(offset..offset + 4).and_then(|s| s.first_chunk::<4>().copied())
    }

    /// Converts to premultiplied alpha in place. A no-op when already premultiplied.
    pub fn premultiply(&mut self) {
        if self.alpha == AlphaMode::Straight {
            premultiply_rgba8(&mut self.data);
            self.alpha = AlphaMode::Premultiplied;
        }
    }

    /// Converts to straight alpha in place. A no-op when already straight.
    ///
    /// Lossy for partially transparent texels: premultiplication quantises to 8
    /// bits, so the original colour cannot be recovered exactly. The error is
    /// bounded by roughly `255 / (2 * alpha)` per channel, which is invisible at
    /// high alpha and total at `alpha == 0`, where the colour is genuinely gone.
    pub fn unpremultiply(&mut self) {
        if self.alpha == AlphaMode::Premultiplied {
            unpremultiply_rgba8(&mut self.data);
            self.alpha = AlphaMode::Straight;
        }
    }

    /// Returns the image in the requested alpha representation.
    pub fn into_alpha_mode(mut self, target: AlphaMode) -> Self {
        match target {
            AlphaMode::Premultiplied => self.premultiply(),
            AlphaMode::Straight => self.unpremultiply(),
        }
        self
    }
}

/// Multiplies RGB by alpha in place, over a tightly packed RGBA8 buffer.
///
/// The multiply happens in the buffer's *storage* encoding, which for a decoded
/// PNG is sRGB rather than linear light. That is a deliberate choice, not an
/// oversight: it matches Skia, Direct2D and CoreGraphics, keeps 8-bit precision
/// intact, and lets the GPU's sRGB sampler do the transfer. Premultiplying in
/// linear space would be colorimetrically purer but requires a wider texture
/// format to avoid banding the darks. See the crate documentation.
///
/// Fully transparent texels have their RGB cleared to zero. This matters: a
/// transparent texel whose RGB is left at, say, white will bleed white into the
/// neighbouring visible texels under bilinear filtering and mip reduction.
///
/// Trailing bytes that do not form a whole texel are ignored rather than
/// panicking; [`DecodedImage`] cannot produce such a buffer, but a caller
/// slicing a subregion by hand can.
pub fn premultiply_rgba8(data: &mut [u8]) {
    for texel in data.chunks_exact_mut(4) {
        let a = u32::from(texel[3]);
        match a {
            255 => {}
            0 => {
                texel[0] = 0;
                texel[1] = 0;
                texel[2] = 0;
            }
            _ => {
                texel[0] = mul_255(texel[0], a);
                texel[1] = mul_255(texel[1], a);
                texel[2] = mul_255(texel[2], a);
            }
        }
    }
}

/// Divides RGB by alpha in place, the inverse of [`premultiply_rgba8`].
///
/// A zero-alpha texel yields zero rather than dividing by zero. Channels are
/// clamped to 255, so a malformed "premultiplied" buffer whose RGB exceeds its
/// alpha, which some encoders emit, saturates instead of wrapping.
pub fn unpremultiply_rgba8(data: &mut [u8]) {
    for texel in data.chunks_exact_mut(4) {
        let a = u32::from(texel[3]);
        match a {
            255 => {}
            0 => {
                texel[0] = 0;
                texel[1] = 0;
                texel[2] = 0;
            }
            _ => {
                texel[0] = div_255(texel[0], a);
                texel[1] = div_255(texel[1], a);
                texel[2] = div_255(texel[2], a);
            }
        }
    }
}

/// `round(c * a / 255)` without a divide.
///
/// The `t + (t >> 8)` trick is exact for all 8-bit inputs, unlike the naive
/// `c * a / 255` truncation, which loses a full level and makes premultiply
/// followed by unpremultiply drift downwards on every round trip.
#[inline(always)]
fn mul_255(c: u8, a: u32) -> u8 {
    let t = u32::from(c) * a + 128;
    ((t + (t >> 8)) >> 8) as u8
}

/// `round(c * 255 / a)`, saturating at 255. `a` must be non-zero.
#[inline(always)]
fn div_255(c: u8, a: u32) -> u8 {
    debug_assert!(a > 0);
    (((u32::from(c) * 255) + a / 2) / a).min(255) as u8
}

/// Decodes from any [`ImageSource`].
pub fn decode(
    source: ImageSource<'_>,
    options: &DecodeOptions,
) -> Result<DecodedImage, ImageError> {
    match source {
        ImageSource::Bytes(bytes) => decode_bytes(bytes, options),
        ImageSource::Path(path) => decode_path(path, options),
        ImageSource::Rgba8 { width, height, data, alpha, color_space } => {
            // Validate before copying: a caller that declares 100000x100000 and
            // hands over a four-byte slice must be rejected on the numbers.
            validate_dimensions(width, height, options)?;
            DecodedImage::from_rgba8(width, height, data.to_vec(), alpha, color_space, options)
                .map(|img| img.into_alpha_mode(options.alpha))
        }
    }
}

/// Decodes an encoded PNG, JPEG or WebP already in memory.
///
/// The format is taken from the magic bytes; the caller's file name, if any, is
/// never consulted here.
pub fn decode_bytes(bytes: &[u8], options: &DecodeOptions) -> Result<DecodedImage, ImageError> {
    decode_encoded(bytes, None, options)
}

/// Reads and decodes an encoded file.
///
/// The file is read whole and then sniffed, so a `.png` containing a JPEG loads
/// correctly. The extension is used only when the content matches no known
/// signature at all, which happens for formats whose magic bytes are not at
/// offset zero.
pub fn decode_path(path: &Path, options: &DecodeOptions) -> Result<DecodedImage, ImageError> {
    let bytes = std::fs::read(path)
        .map_err(|source| ImageError::Io { path: path.display().to_string(), source })?;
    decode_encoded(&bytes, ImageFormatKind::from_extension(path), options)
}

fn decode_encoded(
    bytes: &[u8],
    extension_hint: Option<ImageFormatKind>,
    options: &DecodeOptions,
) -> Result<DecodedImage, ImageError> {
    if bytes.is_empty() {
        return Err(ImageError::Decode("image data is empty".to_string()));
    }
    let kind = match ImageFormatKind::classify(bytes) {
        MagicVerdict::Supported(kind) => kind,
        MagicVerdict::Recognised(name) => {
            return Err(ImageError::UnsupportedFormat(format!(
                "{name} (sphere-image decodes PNG, JPEG and WebP)"
            )));
        }
        MagicVerdict::Unknown => {
            extension_hint.ok_or_else(|| ImageError::UnsupportedFormat(describe_unknown(bytes)))?
        }
    };
    let format = kind.to_codec_format();

    // Pass one parses headers only and allocates nothing proportional to the
    // declared size, so an image bomb is rejected on its metadata. Doing this
    // before the real decode is what turns a would-be multi-gigabyte allocation
    // into an ordinary Err.
    let mut probe = ImageReader::with_format(Cursor::new(bytes), format);
    probe.no_limits();
    let (width, height) = probe.into_dimensions().map_err(|e| map_codec_error(e, None, options))?;
    validate_dimensions(width, height, options)?;

    // Pass two decodes for real, with the codec's own allocation guard armed as
    // a second line of defence against a decoder that ignores its header.
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(codec_limits(options));
    let decoded: DynamicImage =
        reader.decode().map_err(|e| map_codec_error(e, Some((width, height)), options))?;

    let rgba = decoded.into_rgba8();
    let (w, h) = rgba.dimensions();
    let image = DecodedImage::from_rgba8(
        w,
        h,
        rgba.into_raw(),
        AlphaMode::Straight,
        ColorSpace::Srgb,
        options,
    )?;
    Ok(image.into_alpha_mode(options.alpha))
}

/// The codec-side resource guard, mirroring our own limits.
fn codec_limits(options: &DecodeOptions) -> Limits {
    let mut limits = Limits::no_limits();
    limits.max_image_width = Some(options.max_dimension);
    limits.max_image_height = Some(options.max_dimension);
    limits.max_alloc = Some(options.max_decoded_bytes);
    limits
}

/// Bytes a tightly packed RGBA8 buffer of these dimensions needs, saturating.
///
/// Saturation is the right failure mode here because every caller compares the
/// result against a budget: `u64::MAX` is over any budget, so an overflow is
/// rejected rather than wrapped into a plausible-looking small number.
#[inline]
fn rgba8_byte_len(width: u32, height: u32) -> u64 {
    u64::from(width).saturating_mul(u64::from(height)).saturating_mul(4)
}

/// Rejects degenerate and oversized dimensions before anything is allocated.
///
/// Zero area is reported as [`ImageError::Decode`] rather than
/// [`ImageError::TooLarge`] because the latter's message ("exceeding the maximum
/// texture dimension") would be actively misleading for a 0x0 image, and a wrong
/// error message costs more support time than a slightly less tidy taxonomy.
fn validate_dimensions(width: u32, height: u32, options: &DecodeOptions) -> Result<(), ImageError> {
    if width == 0 || height == 0 {
        return Err(ImageError::Decode(format!(
            "image has zero area ({width}x{height}); there are no texels to upload"
        )));
    }
    if width > options.max_dimension || height > options.max_dimension {
        return Err(ImageError::TooLarge { width, height, max: options.max_dimension });
    }
    // Saturating, not plain: `max_dimension` is caller-supplied, and a host that
    // wants the byte budget to be the only cap will set it to u32::MAX. The
    // product of two u32s times four overshoots u64, which panics under debug
    // overflow checks and — far worse — wraps to a *small* number in release,
    // waving the very pixel bomb this check exists to stop straight through.
    let needed = rgba8_byte_len(width, height);
    if needed > options.max_decoded_bytes {
        return Err(ImageError::Decode(format!(
            "image is {width}x{height}, needing {} MiB as RGBA8, over the {} MiB decode budget",
            needed / (1024 * 1024),
            options.max_decoded_bytes / (1024 * 1024)
        )));
    }
    Ok(())
}

fn map_codec_error(
    err: CodecError,
    dimensions: Option<(u32, u32)>,
    options: &DecodeOptions,
) -> ImageError {
    match err {
        CodecError::Unsupported(inner) => ImageError::UnsupportedFormat(inner.to_string()),
        CodecError::Limits(_) => {
            let (width, height) = dimensions.unwrap_or((0, 0));
            ImageError::TooLarge { width, height, max: options.max_dimension }
        }
        // A truncated file surfaces as an unexpected end of stream from the
        // in-memory cursor. That is a corrupt image, not an I/O failure of the
        // host, so it must not be reported as ImageError::Io.
        other => ImageError::Decode(other.to_string()),
    }
}

/// Builds a diagnostic for data that matches no signature.
///
/// Includes the first bytes in hex: "not an image" is a useless bug report,
/// whereas "starts with 3c 3f 78 6d" tells the reader they handed us XML.
fn describe_unknown(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("unrecognised image data, starts with");
    for b in bytes.iter().take(8) {
        let _ = write!(s, " {b:02x}");
    }
    if bytes.len() > 8 {
        s.push_str(" ...");
    }
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Encodes a checkerboard so tests exercise real codec output rather than a
    /// buffer we already control.
    pub(crate) fn rgba_checkerboard(w: u32, h: u32) -> Vec<u8> {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let on = (x + y) % 2 == 0;
                let a = if x == 0 { 0 } else { 255 };
                data.extend_from_slice(&[
                    if on { 200 } else { 20 },
                    if on { 100 } else { 220 },
                    if on { 40 } else { 60 },
                    a,
                ]);
            }
        }
        data
    }

    pub(crate) fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        use image::ImageEncoder;
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
            .expect("png encode");
        out
    }

    fn encode_jpeg(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 98);
        enc.encode(rgb, w, h, image::ExtendedColorType::Rgb8).expect("jpeg encode");
        out
    }

    fn encode_webp(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .encode(rgba, w, h, image::ExtendedColorType::Rgba8)
            .expect("webp encode");
        out
    }

    fn straight_opts() -> DecodeOptions {
        DecodeOptions::default().with_alpha(AlphaMode::Straight)
    }

    #[test]
    fn png_round_trips_exactly() {
        let (w, h) = (4u32, 3u32);
        let rgba = rgba_checkerboard(w, h);
        let png = encode_png(w, h, &rgba);
        let img = decode_bytes(&png, &straight_opts()).expect("decode");
        assert_eq!(img.dimensions(), (w, h));
        assert_eq!(img.alpha_mode(), AlphaMode::Straight);
        assert_eq!(img.color_space(), ColorSpace::Srgb);
        // PNG is lossless: every texel must survive bit for bit.
        assert_eq!(img.data(), rgba.as_slice());
        assert!(!img.is_opaque(), "column 0 is transparent");
    }

    #[test]
    fn png_defaults_to_premultiplied() {
        let rgba = vec![255u8, 128, 64, 128];
        let png = encode_png(1, 1, &rgba);
        let img = decode_bytes(&png, &DecodeOptions::default()).expect("decode");
        assert_eq!(img.alpha_mode(), AlphaMode::Premultiplied);
        assert_eq!(
            img.texel(0, 0),
            Some([mul_255(255, 128), mul_255(128, 128), mul_255(64, 128), 128])
        );
    }

    #[test]
    fn jpeg_round_trips_within_tolerance() {
        let (w, h) = (8u32, 8u32);
        // A flat colour keeps the DCT honest; a checkerboard at 8x8 would ring.
        let rgb: Vec<u8> = (0..w * h).flat_map(|_| [180u8, 90, 30]).collect();
        let jpeg = encode_jpeg(w, h, &rgb);
        let img = decode_bytes(&jpeg, &straight_opts()).expect("decode");
        assert_eq!(img.dimensions(), (w, h));
        assert!(img.is_opaque(), "JPEG has no alpha channel");
        for (i, texel) in img.data().chunks_exact(4).enumerate() {
            for c in 0..3 {
                let expected = i32::from(rgb[i * 3 + c]);
                let actual = i32::from(texel[c]);
                assert!(
                    (expected - actual).abs() <= 4,
                    "texel {i} channel {c}: {expected} vs {actual}"
                );
            }
            assert_eq!(texel[3], 255);
        }
    }

    #[test]
    fn webp_round_trips_losslessly() {
        let (w, h) = (4u32, 4u32);
        let rgba = rgba_checkerboard(w, h);
        let webp = encode_webp(w, h, &rgba);
        assert_eq!(ImageFormatKind::from_magic(&webp), Some(ImageFormatKind::WebP));
        let img = decode_bytes(&webp, &straight_opts()).expect("decode");
        assert_eq!(img.dimensions(), (w, h));
        // The encoder is VP8L, so this must be exact.
        assert_eq!(img.data(), rgba.as_slice());
    }

    #[test]
    fn format_is_detected_from_content_not_extension() {
        let png = encode_png(2, 2, &rgba_checkerboard(2, 2));
        assert_eq!(ImageFormatKind::from_magic(&png), Some(ImageFormatKind::Png));
        // A file whose extension lies still decodes, because the header wins.
        let dir = std::env::temp_dir().join("sphere-image-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("actually-a-png.jpg");
        std::fs::write(&path, &png).expect("write");
        let img = decode_path(&path, &straight_opts()).expect("decode");
        assert_eq!(img.dimensions(), (2, 2));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_and_short_input_is_an_error_not_a_panic() {
        assert!(matches!(decode_bytes(&[], &DecodeOptions::default()), Err(ImageError::Decode(_))));
        for len in 1..16usize {
            let err = decode_bytes(&vec![0u8; len], &DecodeOptions::default());
            assert!(err.is_err(), "len {len} decoded successfully");
        }
    }

    #[test]
    fn truncated_png_is_an_error_not_a_panic() {
        let rgba = rgba_checkerboard(16, 16);
        let png = encode_png(16, 16, &rgba);
        // The trailing IEND chunk is exactly 12 bytes and carries no pixels, so
        // a decoder is within its rights to accept a file missing only that.
        // Anything cut shorter has lost real image data and must fail.
        let last_pixel_byte = png.len() - 12;
        for cut in 1..png.len() {
            match decode_bytes(&png[..cut], &straight_opts()) {
                Err(_) => {}
                Ok(image) => {
                    assert!(
                        cut >= last_pixel_byte,
                        "truncating to {cut} of {} lost pixel data but still decoded",
                        png.len()
                    );
                    // If it does decode, it must be the whole, correct image; a
                    // half-filled buffer presented as success is the worst
                    // possible outcome here.
                    assert_eq!(image.dimensions(), (16, 16));
                    assert_eq!(image.data(), rgba.as_slice());
                }
            }
        }
    }

    #[test]
    fn corrupt_png_body_is_an_error_not_a_panic() {
        let mut png = encode_png(16, 16, &rgba_checkerboard(16, 16));
        // Scribble over the compressed data, leaving the signature intact so the
        // format is still identified as PNG and the zlib stream is what fails.
        for b in png.iter_mut().skip(40) {
            *b ^= 0x5A;
        }
        assert!(decode_bytes(&png, &DecodeOptions::default()).is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        // A cheap deterministic LCG: reproducible failures beat a real RNG here.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for round in 0..64 {
            let len = 1 + (round * 37) % 900;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                buf.push((state >> 33) as u8);
            }
            // Give a quarter of the rounds a valid PNG signature so the decoder
            // is forced past format detection into real parsing.
            if round % 4 == 0 && buf.len() >= 8 {
                buf[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
            }
            let _ = decode_bytes(&buf, &DecodeOptions::default());
        }
    }

    #[test]
    fn unsupported_but_recognised_format_says_which() {
        // A GIF signature: a real image format that this crate does not decode.
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0u8; 32]);
        match decode_bytes(&gif, &DecodeOptions::default()) {
            Err(ImageError::UnsupportedFormat(msg)) => assert!(msg.contains("Gif"), "{msg}"),
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }
    }

    #[test]
    fn unknown_data_reports_its_leading_bytes() {
        match decode_bytes(b"<?xml version=\"1.0\"?>", &DecodeOptions::default()) {
            Err(ImageError::UnsupportedFormat(msg)) => {
                assert!(msg.contains("3c 3f 78 6d"), "{msg}")
            }
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_an_io_error_carrying_the_path() {
        let path = Path::new("W:/definitely/not/here/sphere-image-missing.png");
        match decode_path(path, &DecodeOptions::default()) {
            Err(ImageError::Io { path: p, .. }) => assert!(p.contains("sphere-image-missing")),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn oversized_declared_dimensions_are_rejected_before_allocating() {
        // 16x16 is fine by default; with a device that only supports 8, it is not.
        let png = encode_png(16, 16, &rgba_checkerboard(16, 16));
        let opts = DecodeOptions::default().with_max_dimension(8);
        match decode_bytes(&png, &opts) {
            Err(ImageError::TooLarge { width, height, max }) => {
                assert_eq!((width, height, max), (16, 16, 8));
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn a_declared_pixel_bomb_is_rejected_on_its_header() {
        // A PNG whose IHDR claims 65535x65535 (17 GB as RGBA8) but whose body is
        // a few bytes. Decoding it must fail on the header, without allocating.
        let png = png_with_forged_dimensions(65535, 65535);
        match decode_bytes(&png, &DecodeOptions::default()) {
            Err(ImageError::TooLarge { width, height, max }) => {
                assert_eq!((width, height), (65535, 65535));
                assert_eq!(max, DEFAULT_MAX_DIMENSION);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn dimensions_that_are_individually_legal_but_jointly_absurd_are_rejected() {
        // 12000x12000 is under the 16384 edge limit but is 549 MiB of RGBA8.
        let png = png_with_forged_dimensions(12000, 12000);
        match decode_bytes(&png, &DecodeOptions::default()) {
            Err(ImageError::Decode(msg)) => {
                assert!(msg.contains("decode budget"), "{msg}");
            }
            other => panic!("expected a budget rejection, got {other:?}"),
        }
    }

    /// Rewrites a real PNG's IHDR width and height, fixing the chunk CRC so the
    /// decoder accepts the header and reaches the dimension check.
    fn png_with_forged_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut png = encode_png(1, 1, &[255, 0, 0, 255]);
        // Layout: 8-byte signature, then length(4) + "IHDR"(4) + width(4) +
        // height(4) + 5 more bytes + CRC(4).
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&png[12..29]); // type + data, per the PNG spec
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        png
    }

    /// Bitwise CRC-32 (IEEE), enough for one 17-byte chunk in a test.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn zero_sized_raw_buffers_are_rejected() {
        let opts = DecodeOptions::default();
        let err = DecodedImage::from_rgba8(
            0,
            0,
            Vec::new(),
            AlphaMode::Straight,
            ColorSpace::Srgb,
            &opts,
        );
        assert!(matches!(err, Err(ImageError::Decode(_))));
        let err = DecodedImage::from_rgba8(
            4,
            0,
            Vec::new(),
            AlphaMode::Straight,
            ColorSpace::Srgb,
            &opts,
        );
        assert!(matches!(err, Err(ImageError::Decode(_))));
    }

    #[test]
    fn an_unbounded_dimension_limit_does_not_overflow_the_budget_check() {
        // A host that wants the byte budget to be the only cap sets max_dimension
        // to u32::MAX. width * height * 4 then overshoots u64: under debug
        // overflow checks that panics, and in release it wraps to a small number
        // and waves the pixel bomb through. Both must be impossible.
        let opts = DecodeOptions::default().with_max_dimension(u32::MAX);
        for (w, h) in [
            (u32::MAX, u32::MAX),
            // Chosen so the naive product times four is exactly 2^64, i.e. wraps
            // to zero and sails under every budget.
            (1u32 << 31, 1u32 << 31),
            (1u32 << 16, 1u32 << 16),
        ] {
            match validate_dimensions(w, h, &opts) {
                Err(ImageError::Decode(msg)) => assert!(msg.contains("decode budget"), "{msg}"),
                other => panic!("{w}x{h} should be over budget, got {other:?}"),
            }
            // ...and the same numbers must not build an image out of no bytes.
            let built = DecodedImage::from_rgba8(
                w,
                h,
                Vec::new(),
                AlphaMode::Straight,
                ColorSpace::Srgb,
                &opts,
            );
            assert!(built.is_err(), "{w}x{h} built a DecodedImage from an empty buffer");
        }
        // The guard must still let an ordinary image through unchanged.
        assert!(validate_dimensions(4096, 4096, &opts).is_ok());
    }

    #[test]
    fn raw_buffer_length_must_match_declared_dimensions() {
        let opts = DecodeOptions::default();
        // One byte short: the classic off-by-one in a hand-rolled stride.
        let err = DecodedImage::from_rgba8(
            2,
            2,
            vec![0; 15],
            AlphaMode::Straight,
            ColorSpace::Srgb,
            &opts,
        );
        match err {
            Err(ImageError::Decode(msg)) => assert!(msg.contains("16"), "{msg}"),
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn raw_source_declaring_an_absurd_size_does_not_allocate() {
        // The buffer is tiny; only the declared size is absurd. This must be
        // rejected on the numbers, before anything is copied.
        let opts = DecodeOptions::default();
        let src = ImageSource::rgba8(100_000, 100_000, &[0u8; 4]);
        match decode(src, &opts) {
            Err(ImageError::TooLarge { width, height, .. }) => {
                assert_eq!((width, height), (100_000, 100_000));
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn premultiply_clears_rgb_of_transparent_texels() {
        // The halo bug: a transparent white texel next to a visible one bleeds
        // white into the edge under any filtering unless RGB is cleared.
        let mut data = vec![255u8, 255, 255, 0, 10, 20, 30, 255];
        premultiply_rgba8(&mut data);
        assert_eq!(&data[..4], &[0, 0, 0, 0]);
        assert_eq!(&data[4..], &[10, 20, 30, 255], "opaque texels must be untouched");
    }

    #[test]
    fn unpremultiply_does_not_divide_by_zero() {
        let mut data = vec![0u8, 0, 0, 0];
        unpremultiply_rgba8(&mut data);
        assert_eq!(data, vec![0, 0, 0, 0]);
    }

    #[test]
    fn premultiply_round_trips_within_the_quantisation_bound() {
        for a in 1..=255u32 {
            for c in [0u8, 1, 37, 128, 200, 254, 255] {
                let mut data = vec![c, c, c, a as u8];
                premultiply_rgba8(&mut data);
                assert!(data[0] <= data[3], "premultiplied {} exceeds alpha {}", data[0], data[3]);
                unpremultiply_rgba8(&mut data);
                // Quantising c*a/255 to 8 bits costs at most half a level, which
                // becomes 255/(2a) levels once divided back out.
                let tolerance = (255.0 / (2.0 * a as f32)).ceil() as i32 + 1;
                let drift = i32::from(data[0]) - i32::from(c);
                assert!(
                    drift.abs() <= tolerance,
                    "a={a} c={c}: drifted {drift}, bound {tolerance}"
                );
            }
        }
    }

    #[test]
    fn opaque_texels_round_trip_exactly() {
        // The overwhelmingly common case must be lossless, or every opaque icon
        // shifts colour slightly the first time it passes through the cache.
        for c in 0..=255u8 {
            let mut data = vec![c, c, c, 255];
            premultiply_rgba8(&mut data);
            assert_eq!(data[0], c);
            unpremultiply_rgba8(&mut data);
            assert_eq!(data[0], c);
        }
    }

    #[test]
    fn alpha_mode_conversions_are_idempotent() {
        let rgba = rgba_checkerboard(4, 4);
        let opts = straight_opts();
        let img =
            DecodedImage::from_rgba8(4, 4, rgba, AlphaMode::Straight, ColorSpace::Srgb, &opts)
                .unwrap();
        let once = img.clone().into_alpha_mode(AlphaMode::Premultiplied);
        let twice = once.clone().into_alpha_mode(AlphaMode::Premultiplied);
        assert_eq!(once.data(), twice.data());
        assert_eq!(twice.alpha_mode(), AlphaMode::Premultiplied);
    }

    #[test]
    fn texel_access_is_bounds_checked() {
        let opts = straight_opts();
        let img = DecodedImage::from_rgba8(
            2,
            2,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            AlphaMode::Straight,
            ColorSpace::Srgb,
            &opts,
        )
        .unwrap();
        assert_eq!(img.texel(0, 0), Some([1, 2, 3, 4]));
        assert_eq!(img.texel(1, 1), Some([13, 14, 15, 16]));
        assert_eq!(img.texel(2, 0), None);
        assert_eq!(img.texel(0, 2), None);
        assert_eq!(img.texel(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn natural_size_and_row_bytes_describe_the_layout() {
        let opts = straight_opts();
        let img = DecodedImage::from_rgba8(
            5,
            3,
            vec![0; 60],
            AlphaMode::Straight,
            ColorSpace::Srgb,
            &opts,
        )
        .unwrap();
        assert_eq!(img.row_bytes(), 20);
        assert_eq!(img.byte_len(), 60);
        assert_eq!(img.natural_size(), Size::new(px(5.0), px(3.0)));
    }

    #[test]
    fn format_metadata_is_consistent() {
        assert_eq!(ImageFormatKind::Png.mime(), "image/png");
        assert!(ImageFormatKind::Png.supports_alpha());
        assert!(!ImageFormatKind::Jpeg.supports_alpha());
        assert_eq!(
            ImageFormatKind::from_extension(Path::new("a/b.JPEG")),
            Some(ImageFormatKind::Jpeg)
        );
        assert_eq!(ImageFormatKind::from_extension(Path::new("a/b.tga")), None);
        assert_eq!(ImageFormatKind::from_extension(Path::new("noext")), None);
    }
}
