//! Paged glyph atlas allocation and packing.
//!
//! # Why paged
//!
//! A single unbounded atlas texture is the obvious design and the wrong one. It
//! forces a reallocate-and-recopy every time it grows, it cannot be partially
//! released when a font is dropped, and it eventually walks into the backend's
//! maximum texture dimension with no recovery path. [`GlyphAtlas`] instead owns
//! a set of fixed-size *pages*, opens a new one when the current ones are full,
//! and can release an individual page back to the allocator.
//!
//! # Why one page set per format
//!
//! An MTSDF glyph is RGBA8 and a small-size bitmap fallback glyph is R8. Those
//! are different GPU texture formats, so they cannot share a page no matter how
//! much free space the other one has. [`GlyphAtlas`] therefore keeps an
//! independent page list per [`GlyphFormat`]; `max_pages_per_format` is a
//! per-format limit, and [`AtlasStats::pages`] counts them all.
//!
//! # Packing
//!
//! Packing is shelf (skyline) packing, implemented here rather than pulled in
//! from a crate because glyph packing is core engine behaviour that has to stay
//! tunable: the slack threshold below is the single knob that trades wasted
//! texels against shelf count, and it is worth owning.
//!
//! Each page holds a list of horizontal shelves. A glyph is placed in the first
//! shelf that is tall enough and not *too* tall — `shelf.height - cell_height`
//! must be within [`AtlasConfig::shelf_slack`] — otherwise a new shelf is opened
//! below the last one. Insertion is online and sort-free, which matters because
//! glyphs arrive one at a time as text is laid out, not in a batch that could be
//! sorted by height first.
//!
//! First fit rather than best fit is deliberate: the slack bound already caps
//! how much a mismatched shelf can waste, so scanning for the *tightest* shelf
//! buys at most `shelf_slack` rows per glyph while making every insertion walk
//! the whole shelf list. [`GlyphAtlas::compact`] is the offline counterpart, and
//! it is what reclaims space that online insertion cannot.
//!
//! # Padding
//!
//! Every glyph reserves [`AtlasConfig::padding`] extra texels on its right and
//! bottom edges, and those texels are explicitly cleared to zero. Without them a
//! bilinear tap near a glyph's edge reaches into whatever glyph the packer put
//! next door, which shows up as a stray sliver of an unrelated letter — a real,
//! shipped-bug-class artifact, not a theoretical one. One texel is enough for
//! bilinear; a wider filter would need more, hence the knob.
//!
//! # UV convention
//!
//! [`AtlasPlacement::uv`] holds *edge-aligned* coordinates: `u0 = x / page`,
//! `u1 = (x + w) / page`, i.e. the outer boundary of the glyph's texel block,
//! not the centre of its first and last texels. This is the convention that
//! makes a quad covering exactly `w` device pixels sample texel centres: pixel
//! `i` of that quad interpolates to `(x + i + 0.5) / page`, which is exactly
//! [`texel_center_uv`]. Half-texel-inset UVs would instead squeeze the field by
//! one texel, which for a distance field also shifts the distance-to-em mapping
//! and thins every stem slightly. See [`uv_rect`].
//!
//! # Uploads
//!
//! A 2048x2048 RGBA8 page is 16 MB. Re-uploading one every frame is a
//! non-starter, so each page tracks which rows changed and
//! [`GlyphAtlas::take_dirty_regions`] hands the renderer only those. Regions are
//! full-width row *bands* because rows are the only contiguous run in a tightly
//! packed page mirror: a band can be handed to `write_texture` as a borrowed
//! slice with a constant row stride and no staging copy.

use core::fmt;

use rustc_hash::FxHashMap;
use sphere_core::{FontError, Rect, rect};

use crate::types::{AtlasPlacement, GlyphFormat, GlyphImage, GlyphKey};

/// Page index used by a placement that occupies no atlas area.
///
/// A space, a zero-width joiner and any other glyph with no ink rasterise to an
/// empty [`GlyphImage`]. Those still get a placement so the caller's own cache
/// records "already handled", but the placement must never be drawn: its `uv`
/// and `texels` are zero and its `page` is this sentinel rather than `0`, so a
/// renderer that forgets to check cannot silently bind page zero and sample a
/// real glyph's corner.
pub const NO_PAGE: u32 = u32::MAX;

/// Smallest page edge the atlas will accept, after clamping to a device limit.
const MIN_PAGE_SIZE: u32 = 16;

/// Largest page edge the atlas will accept.
///
/// 16384 is the maximum 2D texture dimension in D3D12, Vulkan and Metal, so no
/// backend can create a page larger than this. The clamp matters because the
/// page mirror is `page_size^2 * bytes_per_pixel` bytes: without a ceiling a
/// caller who passes a bogus device limit gets an arithmetic overflow in
/// [`AtlasConfig::page_bytes`] and a multi-exabyte `Vec` allocation instead of a
/// usable atlas.
const MAX_PAGE_SIZE: u32 = 16384;

/// Maximum dirty row bands tracked per page before the closest pair is merged.
///
/// Each band becomes one texture upload, so an unbounded list would trade a
/// bandwidth problem for a draw-call problem. Sixteen covers the realistic case
/// (a handful of shelves gain glyphs in any one frame) and merging past that
/// costs only the rows between two bands.
const MAX_DIRTY_BANDS: usize = 16;

/// Number of distinct [`GlyphFormat`] page sets.
const FORMAT_COUNT: usize = 4;

#[inline]
fn format_index(f: GlyphFormat) -> usize {
    match f {
        GlyphFormat::Mtsdf => 0,
        GlyphFormat::Grayscale => 1,
        GlyphFormat::Subpixel => 2,
        GlyphFormat::ColorBitmap => 3,
    }
}

/// Tuning for a [`GlyphAtlas`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AtlasConfig {
    /// Edge length of every page, in texels. Pages are square.
    ///
    /// Square pages keep the shelf allocator's arithmetic and the renderer's
    /// texture management uniform, and a non-square page buys nothing when the
    /// contents are thousands of small rectangles.
    pub page_size: u32,
    /// Hard limit on pages *per format*, not in total.
    ///
    /// This is the bound that turns "the atlas grew until the driver died" into
    /// a recoverable [`FontError::AtlasFull`].
    pub max_pages_per_format: u32,
    /// Texels of clear gutter reserved on each glyph's right and bottom edges.
    ///
    /// One is the correct value for bilinear sampling. Zero is only safe with
    /// nearest sampling and is supported so a test can prove the artifact.
    pub padding: u32,
    /// How much taller than a glyph a shelf may be and still accept it.
    ///
    /// Zero degenerates to one shelf per distinct height, which fragments a page
    /// badly; a large value packs 6-texel glyphs into 60-texel shelves and wastes
    /// 90 % of the row. Four texels keeps the UI-size cluster (glyph heights at
    /// one font size differ by a few texels) on a shared shelf.
    pub shelf_slack: u32,
    /// Soft memory ceiling in bytes, enforced by [`GlyphAtlas::enforce_budget`].
    pub memory_budget_bytes: usize,
    /// Whether a full atlas may evict a page that was not used this frame in
    /// order to make room, instead of failing.
    ///
    /// Only pages untouched since the last [`GlyphAtlas::begin_frame`] are
    /// eligible, so an insert can never pull the rug out from under a glyph the
    /// frame being built already references.
    pub evict_when_full: bool,
}

impl Default for AtlasConfig {
    fn default() -> Self {
        Self {
            page_size: 2048,
            max_pages_per_format: 8,
            padding: 1,
            shelf_slack: 4,
            memory_budget_bytes: 64 * 1024 * 1024,
            evict_when_full: true,
        }
    }
}

impl AtlasConfig {
    /// Clamps [`AtlasConfig::page_size`] to what the backend can actually create.
    ///
    /// Every backend has a maximum texture dimension (commonly 8192 or 16384,
    /// but as low as 2048 on old mobile-class hardware). Requesting more is a
    /// hard device error at texture creation, so the clamp belongs here, next to
    /// the default, rather than in each renderer.
    #[must_use]
    pub fn clamped_to_device(mut self, max_texture_size: u32) -> Self {
        self.page_size = self.page_size.min(max_texture_size);
        self.sanitized()
    }

    /// Forces the configuration into a range the allocator can honour.
    fn sanitized(mut self) -> Self {
        self.page_size = self.page_size.clamp(MIN_PAGE_SIZE, MAX_PAGE_SIZE);
        // Padding wider than a quarter page would make a page unable to hold
        // even four glyphs, which is never what a caller means.
        self.padding = self.padding.min(self.page_size / 4);
        self.max_pages_per_format = self.max_pages_per_format.max(1);
        self
    }

    /// Bytes one page of `format` occupies at this page size.
    ///
    /// Saturating rather than wrapping: this is callable on a raw, unsanitised
    /// [`AtlasConfig`] whose `page_size` field is public, and `u32::MAX` squared
    /// times four bytes per texel overflows `usize` on every target.
    #[inline]
    pub fn page_bytes(&self, format: GlyphFormat) -> usize {
        let texels = u64::from(self.page_size) * u64::from(self.page_size);
        let bytes = texels.saturating_mul(format.bytes_per_pixel() as u64);
        usize::try_from(bytes).unwrap_or(usize::MAX)
    }
}

/// Normalised UV rectangle for a texel rectangle inside a page.
///
/// Edge-aligned, as documented on the module: the returned `[u0, v0, u1, v1]`
/// bounds the *outer* edge of the texel block. Interpolating across a quad that
/// covers exactly `texels.size` device pixels therefore lands on texel centres.
#[inline]
#[must_use]
pub fn uv_rect(page_size: u32, texels: Rect<u32>) -> [f32; 4] {
    let s = page_size.max(1) as f32;
    [
        texels.origin.x as f32 / s,
        texels.origin.y as f32 / s,
        (texels.origin.x + texels.size.width) as f32 / s,
        (texels.origin.y + texels.size.height) as f32 / s,
    ]
}

/// UV of the centre of one texel, the value a correct sample must land on.
///
/// Exposed because it is the definition the [`uv_rect`] convention is chosen to
/// satisfy, and because shader-side debugging needs it.
#[inline]
#[must_use]
pub fn texel_center_uv(page_size: u32, x: u32, y: u32) -> [f32; 2] {
    let s = page_size.max(1) as f32;
    [(x as f32 + 0.5) / s, (y as f32 + 0.5) / s]
}

/// One horizontal row of a page.
#[derive(Copy, Clone, Debug)]
struct Shelf {
    /// Top edge of the shelf within the page.
    y: u32,
    /// Fixed height, set by the first glyph the shelf accepted.
    height: u32,
    /// Left edge of the unallocated remainder.
    cursor_x: u32,
}

/// One atlas page: a square texture mirror plus its shelf allocator state.
struct Page {
    format: GlyphFormat,
    size: u32,
    /// CPU mirror of the texture. Empty while the page holds nothing, so an
    /// evicted page costs no memory until it is reused.
    pixels: Vec<u8>,
    shelves: Vec<Shelf>,
    /// Top of the region no shelf has claimed yet.
    next_shelf_y: u32,
    /// Sum of `w * h` over the glyphs actually stored, excluding padding.
    used_area: u64,
    /// Sum of `cell_w * shelf_height` over every allocation, including padding
    /// and shelf slack. The difference from `used_area` is the honest waste.
    allocated_area: u64,
    glyphs: u32,
    /// Half-open `[y0, y1)` row bands changed since the last upload, sorted and
    /// disjoint.
    dirty: Vec<(u32, u32)>,
    last_used_frame: u64,
    /// Set when the mirror was released while it still held glyphs.
    ///
    /// Eviction deliberately keeps the page index alive so the renderer does not
    /// churn texture handles, which means the renderer's texture for this page
    /// still holds the *evicted* glyphs' texels. The next mirror is a fresh
    /// zeroed buffer, so uploading only the rows the new glyphs touch would
    /// leave that dead ink on the GPU — and a bilinear tap at the edge of a
    /// newly packed glyph would sample it. The flag forces one full-page upload
    /// when the mirror comes back, restoring "texture equals mirror".
    stale_texture: bool,
}

impl Page {
    fn new(format: GlyphFormat, size: u32, frame: u64) -> Self {
        Self {
            format,
            size,
            pixels: Vec::new(),
            shelves: Vec::new(),
            next_shelf_y: 0,
            used_area: 0,
            allocated_area: 0,
            glyphs: 0,
            dirty: Vec::new(),
            last_used_frame: frame,
            stale_texture: false,
        }
    }

    #[inline]
    fn byte_len(&self) -> usize {
        self.size as usize * self.size as usize * self.format.bytes_per_pixel()
    }

    /// Allocates the backing mirror on first use.
    ///
    /// A page whose mirror was released while it held glyphs comes back fully
    /// dirty; see [`Page::stale_texture`]. A page that never held anything does
    /// not, because the renderer creates its texture zero-initialised and a
    /// gratuitous full-page upload of a 2048x2048 RGBA8 page is 16 MB.
    fn ensure_pixels(&mut self) {
        if self.pixels.is_empty() {
            self.pixels = vec![0u8; self.byte_len()];
            if self.stale_texture {
                self.stale_texture = false;
                let size = self.size;
                self.mark_dirty(0, size);
            }
        }
    }

    /// Resets the allocator without touching the mirror, so the page can be
    /// refilled in place. `release` additionally frees the mirror.
    fn reset(&mut self, frame: u64, release: bool) {
        self.shelves.clear();
        self.next_shelf_y = 0;
        self.used_area = 0;
        self.allocated_area = 0;
        self.glyphs = 0;
        self.dirty.clear();
        self.last_used_frame = frame;
        if release {
            // Only content that reached the renderer can go stale on it.
            self.stale_texture |= !self.pixels.is_empty();
            self.pixels = Vec::new();
            self.pixels.shrink_to_fit();
        }
    }

    /// Shelf-packs a `cell_w x cell_h` block (glyph plus its padding gutter).
    ///
    /// First fit with a slack bound, per the module documentation. Returns the
    /// top-left texel of the block.
    fn try_alloc(&mut self, cell_w: u32, cell_h: u32, slack: u32) -> Option<(u32, u32)> {
        if cell_w > self.size || cell_h > self.size {
            return None;
        }
        for i in 0..self.shelves.len() {
            let s = self.shelves[i];
            let fits_height = s.height >= cell_h && s.height - cell_h <= slack;
            // Subtract rather than add so a huge cell_w cannot overflow.
            let fits_width = self.size - s.cursor_x >= cell_w;
            if fits_height && fits_width {
                self.shelves[i].cursor_x = s.cursor_x + cell_w;
                self.allocated_area += cell_w as u64 * s.height as u64;
                return Some((s.cursor_x, s.y));
            }
        }
        if self.size - self.next_shelf_y >= cell_h {
            let y = self.next_shelf_y;
            self.shelves.push(Shelf { y, height: cell_h, cursor_x: cell_w });
            self.next_shelf_y = y + cell_h;
            self.allocated_area += cell_w as u64 * cell_h as u64;
            return Some((0, y));
        }
        None
    }

    /// Copies `src` in at `(x, y)` and clears the padding gutter beside it.
    ///
    /// The gutter is cleared rather than merely reserved because a page reused
    /// after [`GlyphAtlas::clear`] still holds the previous session's texels; a
    /// reserved-but-stale gutter bleeds exactly the same way an unreserved one
    /// does.
    fn write(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8], pad: u32) {
        let bpp = self.format.bytes_per_pixel();
        let stride = self.size as usize * bpp;
        let row_bytes = w as usize * bpp;
        let gutter_w = pad.min(self.size - x - w) as usize * bpp;
        for row in 0..h as usize {
            let dst = (y as usize + row) * stride + x as usize * bpp;
            self.pixels[dst..dst + row_bytes]
                .copy_from_slice(&src[row * row_bytes..(row + 1) * row_bytes]);
            self.pixels[dst + row_bytes..dst + row_bytes + gutter_w].fill(0);
        }
        let gutter_h = pad.min(self.size - y - h);
        let span = row_bytes + gutter_w;
        for row in 0..gutter_h as usize {
            let dst = (y as usize + h as usize + row) * stride + x as usize * bpp;
            self.pixels[dst..dst + span].fill(0);
        }
        self.mark_dirty(y, y + h + gutter_h);
    }

    /// Adds a half-open row band to the dirty set, coalescing in place.
    fn mark_dirty(&mut self, y0: u32, y1: u32) {
        let y0 = y0.min(self.size);
        let y1 = y1.min(self.size);
        if y0 >= y1 {
            return;
        }
        self.dirty.push((y0, y1));
        self.dirty.sort_unstable_by_key(|b| b.0);
        // In-place coalesce: the hot path must not allocate a second vector.
        let mut w = 0;
        for r in 1..self.dirty.len() {
            let (a, b) = self.dirty[r];
            if a <= self.dirty[w].1 {
                self.dirty[w].1 = self.dirty[w].1.max(b);
            } else {
                w += 1;
                self.dirty[w] = (a, b);
            }
        }
        self.dirty.truncate(w + 1);
        while self.dirty.len() > MAX_DIRTY_BANDS {
            // Merge the pair separated by the fewest clean rows: that is the
            // merge that uploads the least redundant data.
            let mut best = 0;
            let mut best_gap = u32::MAX;
            for i in 0..self.dirty.len() - 1 {
                let gap = self.dirty[i + 1].0 - self.dirty[i].1;
                if gap < best_gap {
                    best_gap = gap;
                    best = i;
                }
            }
            self.dirty[best].1 = self.dirty[best + 1].1;
            self.dirty.remove(best + 1);
        }
    }
}

/// A glyph's residency record: where it lives and when it was last drawn.
#[derive(Copy, Clone, Debug)]
struct Resident {
    placement: AtlasPlacement,
    last_used_frame: u64,
}

/// A contiguous run of page rows that changed since the last upload.
///
/// `data` is a borrow into the atlas's own mirror, so uploading costs no copy.
/// The borrow keeps the atlas exclusively borrowed for the lifetime of the
/// returned regions, which also enforces the obvious rule that a page must not
/// be repacked while an upload is reading it.
pub struct DirtyRegion<'a> {
    /// Index of the page, matching [`AtlasPlacement::page`].
    pub page: u32,
    /// Pixel format of the page, and hence of `data`.
    pub format: GlyphFormat,
    /// Texel region to upload. Always spans the page's full width; only the
    /// vertical extent is tight.
    pub rect: Rect<u32>,
    /// Byte stride between rows of `data`, equal to the page's full row size.
    pub bytes_per_row: u32,
    /// `rect.size.height * bytes_per_row` bytes, tightly packed.
    pub data: &'a [u8],
}

impl fmt::Debug for DirtyRegion<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirtyRegion")
            .field("page", &self.page)
            .field("format", &self.format)
            .field("rect", &self.rect)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// A snapshot of atlas occupancy, for the diagnostics overlay.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct AtlasStats {
    /// Pages that exist, across every format.
    pub pages: usize,
    /// Pages currently holding a texture mirror. Evicted pages keep their index
    /// but release their memory, so this is the number that costs anything.
    pub live_pages: usize,
    /// Resident glyphs, including zero-area ones.
    pub glyphs: usize,
    /// Texels covered by actual glyph pixels.
    pub used_texels: u64,
    /// Texels consumed by the packer, including padding and shelf slack.
    pub allocated_texels: u64,
    /// Total texels across live pages.
    pub page_texels: u64,
    /// Bytes held by page mirrors and the residency map.
    pub memory_bytes: usize,
    /// Pages evicted over the atlas's lifetime.
    pub evicted_pages: u64,
}

impl AtlasStats {
    /// Fraction of live page area covered by glyph pixels, in `0.0..=1.0`.
    ///
    /// Honest by construction: the denominator is every texel of every live
    /// page, so shelf slack, padding gutters, unfilled shelf tails and the
    /// unopened bottom of a page all count against it.
    #[inline]
    #[must_use]
    pub fn utilization(&self) -> f32 {
        if self.page_texels == 0 { 0.0 } else { self.used_texels as f32 / self.page_texels as f32 }
    }

    /// Fraction of *allocated* area that is glyph pixels, in `0.0..=1.0`.
    ///
    /// Unlike [`AtlasStats::utilization`] this ignores space the packer has not
    /// handed out yet, so it measures the packer rather than how full the atlas
    /// happens to be. A number well under 1 is the signal to [`GlyphAtlas::compact`].
    #[inline]
    #[must_use]
    pub fn packing_efficiency(&self) -> f32 {
        if self.allocated_texels == 0 {
            0.0
        } else {
            self.used_texels as f32 / self.allocated_texels as f32
        }
    }

    /// Texels lost to padding and shelf slack.
    #[inline]
    #[must_use]
    pub fn wasted_texels(&self) -> u64 {
        self.allocated_texels.saturating_sub(self.used_texels)
    }
}

/// What [`GlyphAtlas::compact`] achieved.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// Glyphs successfully re-packed.
    pub moved: usize,
    /// Glyphs that no longer fit and were dropped from residency.
    ///
    /// Height-descending re-packing is never worse than the online order that
    /// produced the original layout, so this is expected to be zero; it is
    /// reported rather than asserted because a caller that shrank the page limit
    /// between packs can legitimately hit it.
    pub dropped: usize,
    /// Pages that ended up empty and released their mirror.
    pub freed_pages: usize,
    /// [`GlyphAtlas::memory_bytes`] before compaction.
    pub bytes_before: usize,
    /// [`GlyphAtlas::memory_bytes`] after compaction.
    pub bytes_after: usize,
}

/// A paged, shelf-packed glyph atlas with per-page dirty tracking and
/// frame-based eviction.
pub struct GlyphAtlas {
    config: AtlasConfig,
    /// All pages, in stable index order. `AtlasPlacement::page` indexes this.
    pages: Vec<Page>,
    /// Page indices belonging to each [`GlyphFormat`]; see the module docs for
    /// why formats cannot share storage.
    sets: [Vec<u32>; FORMAT_COUNT],
    residency: FxHashMap<GlyphKey, Resident>,
    frame: u64,
    generation: u64,
    evicted_pages: u64,
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for GlyphAtlas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately not deriving: a page mirror is megabytes of texels and
        // formatting them would hang any debugger that touched the atlas.
        let s = self.stats();
        f.debug_struct("GlyphAtlas")
            .field("page_size", &self.config.page_size)
            .field("pages", &s.pages)
            .field("glyphs", &s.glyphs)
            .field("memory_bytes", &s.memory_bytes)
            .field("utilization", &s.utilization())
            .field("generation", &self.generation)
            .finish()
    }
}

impl GlyphAtlas {
    /// An atlas with the default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(AtlasConfig::default())
    }

    /// An atlas with an explicit configuration, sanitised into a workable range.
    #[must_use]
    pub fn with_config(config: AtlasConfig) -> Self {
        Self {
            config: config.sanitized(),
            pages: Vec::new(),
            sets: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            residency: FxHashMap::default(),
            frame: 0,
            generation: 0,
            evicted_pages: 0,
        }
    }

    /// The active configuration, after sanitisation.
    #[inline]
    #[must_use]
    pub fn config(&self) -> AtlasConfig {
        self.config
    }

    /// Edge length of every page, in texels.
    #[inline]
    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.config.page_size
    }

    /// Bumped whenever placements are invalidated: [`GlyphAtlas::clear`],
    /// [`GlyphAtlas::compact`] and every page eviction.
    ///
    /// Anything holding an [`AtlasPlacement`] past a frame boundary — notably
    /// [`crate::cache::GlyphCache`] — must compare this and drop what it cached
    /// when it changes. Without it, an evicted page's texels are re-used by
    /// different glyphs and stale placements draw garbage.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The current frame counter.
    #[inline]
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Advances the frame counter and returns the new value.
    ///
    /// Everything touched after this call is "used this frame" and is protected
    /// from eviction until the next call.
    pub fn begin_frame(&mut self) -> u64 {
        self.frame += 1;
        self.frame
    }

    /// Number of pages that exist, across every format.
    #[inline]
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Number of resident glyphs, including zero-area ones.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.residency.len()
    }

    /// True when no glyph is resident.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.residency.is_empty()
    }

    /// Format of a page, or `None` if the index does not exist.
    #[inline]
    #[must_use]
    pub fn page_format(&self, page: u32) -> Option<GlyphFormat> {
        self.pages.get(page as usize).map(|p| p.format)
    }

    /// The CPU mirror of a page, or `None` if the page has no mirror allocated.
    #[must_use]
    pub fn page_pixels(&self, page: u32) -> Option<&[u8]> {
        let p = self.pages.get(page as usize)?;
        if p.pixels.is_empty() { None } else { Some(&p.pixels) }
    }

    /// Looks a glyph up without recording use. Suitable from a `&self` context
    /// such as a diagnostics pass.
    #[inline]
    #[must_use]
    pub fn get(&self, key: &GlyphKey) -> Option<AtlasPlacement> {
        self.residency.get(key).map(|r| r.placement)
    }

    /// True when the glyph is resident.
    #[inline]
    #[must_use]
    pub fn contains(&self, key: &GlyphKey) -> bool {
        self.residency.contains_key(key)
    }

    /// Looks a glyph up and marks it, and its page, as used this frame.
    ///
    /// This is the eviction hook: a page nothing touched since the last
    /// [`GlyphAtlas::begin_frame`] is the page eviction picks.
    pub fn touch(&mut self, key: &GlyphKey) -> Option<AtlasPlacement> {
        let frame = self.frame;
        let r = self.residency.get_mut(key)?;
        r.last_used_frame = frame;
        let placement = r.placement;
        if placement.page != NO_PAGE {
            self.pages[placement.page as usize].last_used_frame = frame;
        }
        Some(placement)
    }

    /// Places a rasterised glyph, returning where it landed.
    ///
    /// Re-inserting a resident key with a matching format and size is a cheap
    /// no-op that just records use, so a caller does not have to check first.
    ///
    /// # Errors
    ///
    /// - [`FontError::Parse`] if `image.data` is shorter than
    ///   `width * height * bytes_per_pixel`, which means the rasteriser produced
    ///   a malformed image and blitting it would read out of bounds.
    /// - [`FontError::AtlasFull`] if the glyph plus its padding cannot fit in a
    ///   page at all, or if every page of its format is full and no more may be
    ///   opened or evicted. Never panics in either case.
    pub fn insert(
        &mut self,
        key: GlyphKey,
        image: &GlyphImage,
    ) -> Result<AtlasPlacement, FontError> {
        let bpp = image.format.bytes_per_pixel() as u64;
        let expected = image.width as u64 * image.height as u64 * bpp;
        if (image.data.len() as u64) < expected {
            return Err(FontError::Parse {
                name: "glyph image".to_string(),
                reason: format!(
                    "{}x{} {:?} needs {expected} bytes but only {} were supplied",
                    image.width,
                    image.height,
                    image.format,
                    image.data.len()
                ),
            });
        }

        // A glyph with no ink consumes no atlas area at all: allocating a 0x0
        // rectangle would still burn a shelf slot and a padding gutter, and a
        // paragraph is mostly spaces.
        if image.is_empty() {
            // If this key previously held ink, its page must give the accounting
            // back before the zero-area placement replaces it, or utilization()
            // keeps charging for a glyph that no longer exists.
            self.forget(&key);
            let placement = AtlasPlacement {
                page: NO_PAGE,
                uv: [0.0; 4],
                texels: rect(0, 0, 0, 0),
                bounds_em: image.bounds_em,
                range_em: image.range_em,
                format: image.format,
            };
            self.residency.insert(key, Resident { placement, last_used_frame: self.frame });
            return Ok(placement);
        }

        if let Some(r) = self.residency.get(&key).copied() {
            let same = r.placement.format == image.format
                && r.placement.texels.size.width == image.width
                && r.placement.texels.size.height == image.height;
            if same {
                return Ok(self.touch(&key).unwrap_or(r.placement));
            }
            // The glyph changed shape under a stable key. A shelf packer cannot
            // reclaim an interior rectangle, so the old one becomes waste that
            // only compact() recovers; the accounting is corrected here so
            // utilization() does not over-report.
            self.forget(&key);
        }

        let pad = self.config.padding;
        let cell_w = image.width.saturating_add(pad);
        let cell_h = image.height.saturating_add(pad);
        if cell_w > self.config.page_size || cell_h > self.config.page_size {
            return Err(FontError::AtlasFull(format!(
                "glyph {}x{} plus {pad} texels of padding does not fit a {}x{} page",
                image.width, image.height, self.config.page_size, self.config.page_size
            )));
        }

        let Some((page, x, y)) = self.allocate(image.format, cell_w, cell_h) else {
            return Err(FontError::AtlasFull(format!(
                "all {} {:?} page(s) are full and the limit of {} is reached",
                self.sets[format_index(image.format)].len(),
                image.format,
                self.config.max_pages_per_format
            )));
        };

        let frame = self.frame;
        let p = &mut self.pages[page as usize];
        p.ensure_pixels();
        p.write(x, y, image.width, image.height, &image.data, pad);
        p.used_area += image.width as u64 * image.height as u64;
        p.glyphs += 1;
        p.last_used_frame = frame;

        let texels = rect(x, y, image.width, image.height);
        let placement = AtlasPlacement {
            page,
            uv: uv_rect(self.config.page_size, texels),
            texels,
            bounds_em: image.bounds_em,
            range_em: image.range_em,
            format: image.format,
        };
        self.residency.insert(key, Resident { placement, last_used_frame: frame });
        Ok(placement)
    }

    /// Drops a glyph from residency, correcting the owning page's accounting.
    ///
    /// The texels themselves are not reclaimed: shelf packing has no free list,
    /// and pretending otherwise is how atlases end up handing the same rectangle
    /// to two glyphs. [`GlyphAtlas::compact`] is what actually recovers it.
    pub fn forget(&mut self, key: &GlyphKey) -> bool {
        let Some(r) = self.residency.remove(key) else { return false };
        if r.placement.page != NO_PAGE {
            let p = &mut self.pages[r.placement.page as usize];
            let area = r.placement.texels.size.width as u64 * r.placement.texels.size.height as u64;
            p.used_area = p.used_area.saturating_sub(area);
            p.glyphs = p.glyphs.saturating_sub(1);
        }
        true
    }

    /// Finds room for a `cell_w x cell_h` block of `format`.
    fn allocate(
        &mut self,
        format: GlyphFormat,
        cell_w: u32,
        cell_h: u32,
    ) -> Option<(u32, u32, u32)> {
        if let Some(v) = self.try_existing(format, cell_w, cell_h) {
            return Some(v);
        }
        if let Some(v) = self.try_new_page(format, cell_w, cell_h) {
            return Some(v);
        }
        if self.config.evict_when_full && self.evict_lru_page(Some(format)).is_some() {
            return self.try_existing(format, cell_w, cell_h);
        }
        None
    }

    fn try_existing(
        &mut self,
        format: GlyphFormat,
        cell_w: u32,
        cell_h: u32,
    ) -> Option<(u32, u32, u32)> {
        let slack = self.config.shelf_slack;
        let fi = format_index(format);
        for i in 0..self.sets[fi].len() {
            let page = self.sets[fi][i];
            if let Some((x, y)) = self.pages[page as usize].try_alloc(cell_w, cell_h, slack) {
                return Some((page, x, y));
            }
        }
        None
    }

    fn try_new_page(
        &mut self,
        format: GlyphFormat,
        cell_w: u32,
        cell_h: u32,
    ) -> Option<(u32, u32, u32)> {
        let fi = format_index(format);
        if self.sets[fi].len() as u32 >= self.config.max_pages_per_format {
            return None;
        }
        let mut page = Page::new(format, self.config.page_size, self.frame);
        // `?` before the push: a cell that does not fit an empty page never
        // will, and pushing a page for it would leak an empty page per attempt.
        let (x, y) = page.try_alloc(cell_w, cell_h, self.config.shelf_slack)?;
        let index = self.pages.len() as u32;
        self.pages.push(page);
        self.sets[fi].push(index);
        Some((index, x, y))
    }

    /// Evicts the least recently used page that was not touched this frame.
    ///
    /// `format` restricts the search to one page set; `None` searches all of
    /// them, which is what a memory-budget sweep wants.
    pub fn evict_lru_page(&mut self, format: Option<GlyphFormat>) -> Option<u32> {
        let frame = self.frame;
        let mut best: Option<(u64, u32)> = None;
        let consider = |p: &Page, i: u32, best: &mut Option<(u64, u32)>| {
            // A page used this frame is referenced by the frame currently being
            // built; evicting it would invalidate placements already handed out.
            if p.last_used_frame >= frame || p.pixels.is_empty() {
                return;
            }
            if best.is_none_or(|(f, _)| p.last_used_frame < f) {
                *best = Some((p.last_used_frame, i));
            }
        };
        match format {
            Some(f) => {
                for i in 0..self.sets[format_index(f)].len() {
                    let idx = self.sets[format_index(f)][i];
                    consider(&self.pages[idx as usize], idx, &mut best);
                }
            }
            None => {
                for (i, p) in self.pages.iter().enumerate() {
                    consider(p, i as u32, &mut best);
                }
            }
        }
        let (_, index) = best?;
        self.evict_page(index);
        Some(index)
    }

    /// Evicts one page: every glyph on it leaves residency and its mirror is
    /// released. The page index stays valid and the page is reused for new
    /// glyphs, which avoids churning texture handles on the renderer side.
    pub fn evict_page(&mut self, page: u32) -> bool {
        let Some(p) = self.pages.get_mut(page as usize) else { return false };
        let frame = self.frame;
        p.reset(frame, true);
        self.residency.retain(|_, r| r.placement.page != page);
        self.generation += 1;
        self.evicted_pages += 1;
        true
    }

    /// Evicts every page untouched for at least `max_idle_frames` frames.
    ///
    /// Returns how many pages were freed. Call it once per frame with a value
    /// like 120: a page nobody has drawn from in two seconds is holding memory
    /// for text that scrolled off screen.
    pub fn evict_unused(&mut self, max_idle_frames: u64) -> usize {
        let cutoff = self.frame.saturating_sub(max_idle_frames);
        let stale: Vec<u32> = self
            .pages
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.pixels.is_empty() && p.last_used_frame < cutoff)
            .map(|(i, _)| i as u32)
            .collect();
        for page in &stale {
            self.evict_page(*page);
        }
        stale.len()
    }

    /// Evicts least-recently-used pages until [`GlyphAtlas::memory_bytes`] is
    /// within [`AtlasConfig::memory_budget_bytes`].
    ///
    /// Stops early rather than evicting a page in use this frame, so the budget
    /// is a target, not a guarantee: a single frame that genuinely needs more
    /// than the budget gets to render correctly instead of losing its glyphs.
    pub fn enforce_budget(&mut self) -> usize {
        let mut freed = 0;
        while self.memory_bytes() > self.config.memory_budget_bytes {
            if self.evict_lru_page(None).is_none() {
                break;
            }
            freed += 1;
        }
        freed
    }

    /// Drops every glyph and resets every page's allocator, keeping the page
    /// mirrors allocated for immediate refill.
    ///
    /// This is the invalidation path (a font was reloaded, a scale factor
    /// changed), not the memory-reclaim path — the mirrors are the expensive
    /// part and a clear is almost always followed by a refill. Use
    /// [`GlyphAtlas::release_memory`] to actually give the memory back.
    pub fn clear(&mut self) {
        let frame = self.frame;
        for p in &mut self.pages {
            p.reset(frame, false);
        }
        self.residency.clear();
        self.generation += 1;
    }

    /// Clears the atlas and releases every page mirror.
    pub fn release_memory(&mut self) {
        let frame = self.frame;
        for p in &mut self.pages {
            p.reset(frame, true);
        }
        self.residency.clear();
        self.generation += 1;
    }

    /// Re-packs every live glyph in height-descending order.
    ///
    /// This is the only way space comes back. A shelf packer has no free list,
    /// so a glyph dropped by [`GlyphAtlas::forget`], or replaced because it was
    /// re-rasterised at a new size, leaves a rectangle nothing can ever reuse. A
    /// long session churns through font sizes and documents and gradually fills
    /// its pages with those; compaction is what turns them back into free rows,
    /// usually collapsing several pages into one.
    ///
    /// The height-descending order is a secondary win: it groups near-equal
    /// heights so shelves come out full rather than ragged. It is not a strict
    /// improvement on the online order for every input — first fit with slack
    /// sometimes stumbles into a tighter match by accident — so this reports what
    /// it achieved rather than promising a ratio.
    ///
    /// Glyph texels are copied out to a temporary buffer first — only
    /// `used_texels` worth, not a second copy of every page — so peak memory
    /// grows by the atlas's *contents*, not by its capacity.
    ///
    /// Bumps [`GlyphAtlas::generation`]: every previously returned placement is
    /// invalid afterwards.
    pub fn compact(&mut self) -> CompactReport {
        let bytes_before = self.memory_bytes();

        struct Extracted {
            key: GlyphKey,
            format: GlyphFormat,
            w: u32,
            h: u32,
            bounds_em: Rect<f32>,
            range_em: f32,
            last_used_frame: u64,
            data: Vec<u8>,
        }

        let mut live: Vec<Extracted> = Vec::with_capacity(self.residency.len());
        let mut empties: Vec<(GlyphKey, Resident)> = Vec::new();
        for (key, r) in &self.residency {
            if r.placement.page == NO_PAGE {
                empties.push((*key, *r));
                continue;
            }
            let p = &self.pages[r.placement.page as usize];
            if p.pixels.is_empty() {
                continue;
            }
            let bpp = p.format.bytes_per_pixel();
            let stride = p.size as usize * bpp;
            let (x, y) =
                (r.placement.texels.origin.x as usize, r.placement.texels.origin.y as usize);
            let (w, h) =
                (r.placement.texels.size.width as usize, r.placement.texels.size.height as usize);
            let mut data = Vec::with_capacity(w * h * bpp);
            for row in 0..h {
                let s = (y + row) * stride + x * bpp;
                data.extend_from_slice(&p.pixels[s..s + w * bpp]);
            }
            live.push(Extracted {
                key: *key,
                format: p.format,
                w: w as u32,
                h: h as u32,
                bounds_em: r.placement.bounds_em,
                range_em: r.placement.range_em,
                last_used_frame: r.last_used_frame,
                data,
            });
        }

        // Tallest first, widest first within a height: this is the ordering that
        // makes shelves come out uniform instead of ragged.
        live.sort_unstable_by(|a, b| {
            b.h.cmp(&a.h).then(b.w.cmp(&a.w)).then(a.key.glyph.0.cmp(&b.key.glyph.0))
        });

        let frame = self.frame;
        for p in &mut self.pages {
            // Keep the mirrors: they are about to be rewritten, and marking the
            // pages as used this frame stops the allocator evicting one mid-pack.
            p.reset(frame, false);
        }
        self.residency.clear();
        for (key, r) in empties {
            self.residency.insert(key, r);
        }

        let pad = self.config.padding;
        let mut moved = 0;
        let mut dropped = 0;
        for g in &live {
            let cell_w = g.w.saturating_add(pad);
            let cell_h = g.h.saturating_add(pad);
            let Some((page, x, y)) = self.allocate(g.format, cell_w, cell_h) else {
                dropped += 1;
                continue;
            };
            let p = &mut self.pages[page as usize];
            p.ensure_pixels();
            p.write(x, y, g.w, g.h, &g.data, pad);
            p.used_area += g.w as u64 * g.h as u64;
            p.glyphs += 1;
            let texels = rect(x, y, g.w, g.h);
            self.residency.insert(
                g.key,
                Resident {
                    placement: AtlasPlacement {
                        page,
                        uv: uv_rect(self.config.page_size, texels),
                        texels,
                        bounds_em: g.bounds_em,
                        range_em: g.range_em,
                        format: g.format,
                    },
                    last_used_frame: g.last_used_frame,
                },
            );
            moved += 1;
        }

        let mut freed_pages = 0;
        for p in &mut self.pages {
            if p.glyphs == 0 && !p.pixels.is_empty() {
                p.reset(frame, true);
                freed_pages += 1;
            }
        }

        self.generation += 1;
        CompactReport {
            moved,
            dropped,
            freed_pages,
            bytes_before,
            bytes_after: self.memory_bytes(),
        }
    }

    /// Marks every live page entirely dirty.
    ///
    /// The recovery path after a device loss, where the GPU-side textures are
    /// gone but the CPU mirrors are intact and re-rasterising every glyph would
    /// be pure waste.
    pub fn mark_all_dirty(&mut self) {
        for p in &mut self.pages {
            if p.pixels.is_empty() {
                continue;
            }
            p.dirty.clear();
            let size = p.size;
            p.mark_dirty(0, size);
        }
    }

    /// Takes the pending uploads, leaving every page clean.
    ///
    /// Regions borrow the atlas, so the atlas cannot be modified until the
    /// returned slice of regions is dropped — which is exactly the invariant an
    /// upload needs.
    pub fn take_dirty_regions(&mut self) -> Vec<DirtyRegion<'_>> {
        let mut bands: Vec<(u32, u32, u32)> = Vec::new();
        for (i, p) in self.pages.iter_mut().enumerate() {
            if p.pixels.is_empty() {
                p.dirty.clear();
                continue;
            }
            for (y0, y1) in p.dirty.drain(..) {
                bands.push((i as u32, y0, y1));
            }
        }
        let pages = &self.pages;
        bands
            .into_iter()
            .map(|(i, y0, y1)| {
                let p = &pages[i as usize];
                let stride = p.size as usize * p.format.bytes_per_pixel();
                DirtyRegion {
                    page: i,
                    format: p.format,
                    rect: rect(0, y0, p.size, y1 - y0),
                    bytes_per_row: stride as u32,
                    data: &p.pixels[y0 as usize * stride..y1 as usize * stride],
                }
            })
            .collect()
    }

    /// True when at least one page has pending uploads.
    #[must_use]
    pub fn has_dirty_regions(&self) -> bool {
        self.pages.iter().any(|p| !p.dirty.is_empty() && !p.pixels.is_empty())
    }

    /// Bytes held by page mirrors plus the residency map.
    ///
    /// The residency term is included because a million-glyph CJK session's map
    /// is not free, and a budget that ignored it would under-report by tens of
    /// megabytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        let pixels: usize = self.pages.iter().map(|p| p.pixels.capacity()).sum();
        let map = self.residency.len()
            * (size_of::<GlyphKey>() + size_of::<Resident>() + size_of::<u64>());
        pixels + map
    }

    /// Fraction of live page area covered by glyph pixels; see
    /// [`AtlasStats::utilization`].
    #[must_use]
    pub fn utilization(&self) -> f32 {
        self.stats().utilization()
    }

    /// Fraction of allocated area that is glyph pixels; see
    /// [`AtlasStats::packing_efficiency`].
    #[must_use]
    pub fn packing_efficiency(&self) -> f32 {
        self.stats().packing_efficiency()
    }

    /// A snapshot of occupancy for the diagnostics overlay.
    #[must_use]
    pub fn stats(&self) -> AtlasStats {
        let mut s = AtlasStats {
            pages: self.pages.len(),
            glyphs: self.residency.len(),
            memory_bytes: self.memory_bytes(),
            evicted_pages: self.evicted_pages,
            ..Default::default()
        };
        for p in &self.pages {
            s.used_texels += p.used_area;
            s.allocated_texels += p.allocated_area;
            if !p.pixels.is_empty() {
                s.live_pages += 1;
                s.page_texels += p.size as u64 * p.size as u64;
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_core::{FontId, GlyphId};

    fn key(n: u16) -> GlyphKey {
        GlyphKey::mtsdf(FontId::new(0, 1), GlyphId(n), 0)
    }

    /// A glyph image of `w x h` filled with `fill`, in the given format.
    fn img(w: u32, h: u32, format: GlyphFormat, fill: u8) -> GlyphImage {
        GlyphImage {
            width: w,
            height: h,
            format,
            data: vec![fill; (w * h) as usize * format.bytes_per_pixel()],
            bounds_em: rect(0.0, -0.7, 0.5, 0.7),
            range_em: 0.05,
        }
    }

    fn gray(w: u32, h: u32) -> GlyphImage {
        img(w, h, GlyphFormat::Grayscale, 0xAB)
    }

    fn small_atlas(page: u32, max_pages: u32) -> GlyphAtlas {
        GlyphAtlas::with_config(AtlasConfig {
            page_size: page,
            max_pages_per_format: max_pages,
            padding: 1,
            shelf_slack: 4,
            memory_budget_bytes: usize::MAX,
            evict_when_full: true,
        })
    }

    /// `Rect<u32>` has no `Scalar` impl (u32 cannot be negated), so overlap is
    /// tested here rather than through `Rect::intersects`.
    fn overlaps(a: Rect<u32>, b: Rect<u32>) -> bool {
        let (ax1, ay1) = (a.origin.x + a.size.width, a.origin.y + a.size.height);
        let (bx1, by1) = (b.origin.x + b.size.width, b.origin.y + b.size.height);
        a.origin.x < bx1 && b.origin.x < ax1 && a.origin.y < by1 && b.origin.y < ay1
    }

    /// Grows a rect by `n` texels on every side, saturating at zero.
    fn inflate(r: Rect<u32>, n: u32) -> Rect<u32> {
        let x = r.origin.x.saturating_sub(n);
        let y = r.origin.y.saturating_sub(n);
        rect(x, y, r.size.width + (r.origin.x - x) + n, r.size.height + (r.origin.y - y) + n)
    }

    #[test]
    fn mixed_size_glyphs_never_overlap() {
        // The core invariant. If this ever fails, two letters share texels and
        // the whole text stack renders garbage.
        let mut atlas = small_atlas(128, 4);
        let sizes = [
            (3, 3),
            (17, 9),
            (9, 17),
            (1, 1),
            (40, 12),
            (12, 40),
            (5, 6),
            (6, 5),
            (31, 31),
            (2, 29),
            (29, 2),
            (11, 11),
            (13, 7),
            (7, 13),
            (23, 19),
            (19, 23),
        ];
        let mut placed: Vec<AtlasPlacement> = Vec::new();
        for (i, (w, h)) in sizes.iter().enumerate() {
            let p = atlas.insert(key(i as u16), &gray(*w, *h)).expect("should fit");
            placed.push(p);
        }
        for i in 0..placed.len() {
            for j in (i + 1)..placed.len() {
                if placed[i].page != placed[j].page {
                    continue;
                }
                assert!(
                    !overlaps(placed[i].texels, placed[j].texels),
                    "glyph {i} at {:?} overlaps glyph {j} at {:?}",
                    placed[i].texels,
                    placed[j].texels
                );
            }
        }
    }

    #[test]
    fn neighbours_are_separated_by_at_least_one_texel() {
        // Bilinear taps at a glyph's edge must not reach a neighbour's texels.
        let mut atlas = small_atlas(64, 2);
        let mut placed = Vec::new();
        for i in 0..20u16 {
            let w = 3 + u32::from(i % 5) * 2;
            let h = 4 + u32::from(i % 3) * 3;
            placed.push(atlas.insert(key(i), &gray(w, h)).expect("should fit"));
        }
        for i in 0..placed.len() {
            for j in (i + 1)..placed.len() {
                if placed[i].page != placed[j].page {
                    continue;
                }
                assert!(
                    !overlaps(inflate(placed[i].texels, 1), placed[j].texels),
                    "glyph {i} {:?} has no padding gutter against glyph {j} {:?}",
                    placed[i].texels,
                    placed[j].texels
                );
            }
        }
    }

    #[test]
    fn padding_gutter_is_cleared_even_in_a_recycled_page() {
        // clear() keeps the mirror, so a stale neighbour would survive into the
        // gutter if write() only *reserved* it instead of zeroing it.
        let mut atlas = small_atlas(32, 1);
        for i in 0..6u16 {
            atlas.insert(key(i), &gray(6, 6)).expect("should fit");
        }
        atlas.clear();
        let p = atlas.insert(key(100), &gray(4, 4)).expect("should fit");
        let px = atlas.page_pixels(p.page).expect("page has a mirror");
        let stride = atlas.page_size() as usize;
        let (x, y) = (p.texels.origin.x as usize, p.texels.origin.y as usize);
        // Right gutter column and bottom gutter row must both be zero.
        for row in 0..4 {
            assert_eq!(px[(y + row) * stride + x + 4], 0, "right gutter row {row} is stale");
        }
        for col in 0..5 {
            assert_eq!(px[(y + 4) * stride + x + col], 0, "bottom gutter col {col} is stale");
        }
        // ...while the glyph itself survived.
        assert_eq!(px[y * stride + x], 0xAB);
    }

    #[test]
    fn glyph_larger_than_a_page_is_rejected_not_panicked_on() {
        let mut atlas = small_atlas(64, 4);
        let err = atlas.insert(key(0), &gray(100, 8)).unwrap_err();
        assert!(matches!(err, FontError::AtlasFull(_)), "{err:?}");
        assert!(err.to_string().contains("64"), "{err}");
        // A page-sized glyph fails too, because the padding gutter must fit.
        assert!(atlas.insert(key(1), &gray(64, 64)).is_err());
        // One texel smaller does fit.
        assert!(atlas.insert(key(2), &gray(63, 63)).is_ok());
        assert_eq!(atlas.page_count(), 1, "the rejections must not leak pages");
    }

    #[test]
    fn a_glyph_bigger_than_u32_area_does_not_overflow() {
        // Guards the `saturating_add` on the padded cell size: `u32::MAX + 1`
        // would wrap to zero and the packer would happily "fit" it.
        let mut atlas = small_atlas(64, 1);
        let bogus = GlyphImage {
            width: u32::MAX,
            height: 1,
            format: GlyphFormat::Grayscale,
            data: vec![0; 4],
            bounds_em: rect(0.0, 0.0, 0.0, 0.0),
            range_em: 0.0,
        };
        // Reported as malformed data rather than accepted: the declared size and
        // the buffer disagree, which is the first thing worth catching.
        assert!(matches!(atlas.insert(key(0), &bogus), Err(FontError::Parse { .. })));
    }

    #[test]
    fn pages_open_in_order_and_the_limit_returns_atlas_full() {
        // 30x30 plus 1 padding is 31x31; a 64 page holds two per shelf and two
        // shelves, so exactly four per page.
        let mut atlas = small_atlas(64, 2);
        let mut pages = Vec::new();
        for i in 0..8u16 {
            pages.push(atlas.insert(key(i), &gray(30, 30)).expect("should fit").page);
        }
        assert_eq!(pages[..4], [0, 0, 0, 0], "first four share page 0");
        assert_eq!(pages[4..], [1, 1, 1, 1], "next four open page 1");
        assert_eq!(atlas.page_count(), 2);

        // Frame 0 means every page counts as used this frame, so nothing may be
        // evicted and the ninth glyph must fail cleanly.
        let err = atlas.insert(key(8), &gray(30, 30)).unwrap_err();
        assert!(matches!(err, FontError::AtlasFull(_)), "{err:?}");
        assert_eq!(atlas.len(), 8, "the failed insert must not half-register");
    }

    #[test]
    fn a_full_atlas_recovers_by_evicting_a_page_nobody_touched_this_frame() {
        let mut atlas = small_atlas(64, 2);
        for i in 0..8u16 {
            atlas.insert(key(i), &gray(30, 30)).expect("should fit");
        }
        let gen_before = atlas.generation();

        atlas.begin_frame();
        // Page 1 is referenced by this frame; page 0 is not, so page 0 goes.
        atlas.touch(&key(4)).expect("resident");
        let p = atlas.insert(key(8), &gray(30, 30)).expect("eviction should make room");
        assert_eq!(p.page, 0);
        assert!(atlas.generation() > gen_before, "eviction must invalidate placements");
        assert!(!atlas.contains(&key(0)), "page 0's glyphs left residency");
        assert!(atlas.contains(&key(4)), "the touched page survived");
    }

    #[test]
    fn separate_formats_never_share_a_page() {
        let mut atlas = small_atlas(64, 4);
        let a = atlas.insert(key(0), &gray(8, 8)).expect("fits");
        let b = atlas.insert(key(1), &img(8, 8, GlyphFormat::Mtsdf, 0x11)).expect("fits");
        let c = atlas.insert(key(2), &img(8, 8, GlyphFormat::Subpixel, 0x22)).expect("fits");
        assert_ne!(a.page, b.page, "an R8 and an RGBA8 glyph cannot share storage");
        assert_ne!(a.page, c.page, "an R8 and an RGB8 glyph cannot share storage");
        assert_ne!(b.page, c.page, "an RGBA8 and an RGB8 glyph cannot share storage");
        assert_eq!(atlas.page_format(a.page), Some(GlyphFormat::Grayscale));
        assert_eq!(atlas.page_format(b.page), Some(GlyphFormat::Mtsdf));
        assert_eq!(atlas.page_format(c.page), Some(GlyphFormat::Subpixel));
        assert_eq!(atlas.page_pixels(c.page).unwrap().len(), 64 * 64 * 3);
        // The per-format limit is per-format, so both sets can be full at once.
        assert_eq!(atlas.page_count(), 3);
    }

    #[test]
    fn uv_rects_are_normalised_and_map_back_to_texel_centres() {
        let mut atlas = small_atlas(64, 1);
        let p = atlas.insert(key(0), &gray(5, 7)).expect("fits");
        for c in p.uv {
            assert!((0.0..=1.0).contains(&c), "uv {c} escaped the unit square");
        }
        assert!(p.uv[0] < p.uv[2] && p.uv[1] < p.uv[3], "uv rect must not be inverted");

        // The convention: edge-aligned UVs, so interpolating across a quad that
        // covers exactly the glyph's texels lands on texel centres.
        let size = atlas.page_size();
        let (w, h) = (p.texels.size.width, p.texels.size.height);
        for i in 0..w {
            for j in 0..h {
                let u = p.uv[0] + (p.uv[2] - p.uv[0]) * ((i as f32 + 0.5) / w as f32);
                let v = p.uv[1] + (p.uv[3] - p.uv[1]) * ((j as f32 + 0.5) / h as f32);
                let expect = texel_center_uv(size, p.texels.origin.x + i, p.texels.origin.y + j);
                assert!((u - expect[0]).abs() < 1e-6, "u {u} != {}", expect[0]);
                assert!((v - expect[1]).abs() < 1e-6, "v {v} != {}", expect[1]);
                // ...and that centre must round back to the texel we stored.
                assert_eq!((u * size as f32).floor() as u32, p.texels.origin.x + i);
                assert_eq!((v * size as f32).floor() as u32, p.texels.origin.y + j);
            }
        }
    }

    #[test]
    fn glyph_pixels_land_at_the_placement_the_uv_points_at() {
        let mut atlas = small_atlas(32, 1);
        atlas.insert(key(0), &gray(6, 6)).expect("fits");
        // A distinguishable second glyph: a 2x2 with four different values.
        let probe = GlyphImage {
            width: 2,
            height: 2,
            format: GlyphFormat::Grayscale,
            data: vec![1, 2, 3, 4],
            bounds_em: rect(0.0, 0.0, 0.1, 0.1),
            range_em: 0.0,
        };
        let p = atlas.insert(key(1), &probe).expect("fits");
        let px = atlas.page_pixels(p.page).expect("mirror exists");
        let stride = atlas.page_size() as usize;
        let (x, y) = (p.texels.origin.x as usize, p.texels.origin.y as usize);
        assert_eq!(px[y * stride + x], 1);
        assert_eq!(px[y * stride + x + 1], 2);
        assert_eq!(px[(y + 1) * stride + x], 3);
        assert_eq!(px[(y + 1) * stride + x + 1], 4);
    }

    #[test]
    fn dirty_regions_cover_every_glyph_and_are_taken_once() {
        let mut atlas = small_atlas(64, 2);
        let mut placed = Vec::new();
        for i in 0..10u16 {
            placed.push(atlas.insert(key(i), &gray(9, 9)).expect("fits"));
        }
        assert!(atlas.has_dirty_regions());
        let regions = atlas.take_dirty_regions();
        assert!(!regions.is_empty());

        // Every glyph's rows are inside some region of its own page, and every
        // region's slice really is `height * bytes_per_row` long.
        for (i, p) in placed.iter().enumerate() {
            let covered = regions.iter().any(|r| {
                r.page == p.page
                    && r.rect.origin.y <= p.texels.origin.y
                    && r.rect.origin.y + r.rect.size.height
                        >= p.texels.origin.y + p.texels.size.height
            });
            assert!(covered, "glyph {i} at {:?} was never uploaded", p.texels);
        }
        for r in &regions {
            assert_eq!(r.data.len(), r.rect.size.height as usize * r.bytes_per_row as usize);
            assert_eq!(r.rect.origin.x, 0, "regions are full-width row bands");
            assert_eq!(r.rect.size.width, 64);
        }
        drop(regions);

        assert!(!atlas.has_dirty_regions(), "taking the regions must clear them");
        assert!(atlas.take_dirty_regions().is_empty());

        // A further insert dirties again, and only the new rows.
        atlas.insert(key(50), &gray(9, 9)).expect("fits");
        let again = atlas.take_dirty_regions();
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn adjacent_dirty_bands_coalesce_into_one_upload() {
        // Filling consecutive shelves must not produce one upload per shelf.
        let mut atlas = small_atlas(256, 1);
        for i in 0..100u16 {
            atlas.insert(key(i), &gray(20, 20)).expect("fits");
        }
        let regions = atlas.take_dirty_regions();
        assert_eq!(regions.len(), 1, "touching shelves are one contiguous band");
        assert!(regions[0].rect.size.height >= 9 * 21);
    }

    #[test]
    fn scattered_dirty_bands_are_capped_without_losing_coverage() {
        // Zero slack gives every distinct glyph height its own shelf, so a glyph
        // can be aimed at a specific shelf by choosing its height.
        let mut atlas = GlyphAtlas::with_config(AtlasConfig {
            page_size: 1024,
            max_pages_per_format: 1,
            padding: 1,
            shelf_slack: 0,
            memory_budget_bytes: usize::MAX,
            evict_when_full: false,
        });
        // 40 shelves of heights 5..=44, totalling 980 of the 1024 rows.
        for i in 0..40u16 {
            atlas.insert(key(i), &gray(4, 4 + u32::from(i))).expect("fits");
        }
        drop(atlas.take_dirty_regions());
        assert!(!atlas.has_dirty_regions());

        // Touch every other shelf: 20 bands separated by clean rows, past the cap.
        let mut written = Vec::new();
        for i in (0..40u16).step_by(2) {
            written.push(atlas.insert(key(100 + i), &gray(4, 4 + u32::from(i))).expect("fits"));
        }
        let regions = atlas.take_dirty_regions();
        assert_eq!(regions.len(), MAX_DIRTY_BANDS, "bands past the cap must merge");
        // Merging trades a few redundant rows for fewer uploads; it must never
        // drop coverage, or a glyph reaches the GPU as garbage.
        for p in &written {
            let covered = regions.iter().any(|r| {
                r.rect.origin.y <= p.texels.origin.y
                    && r.rect.origin.y + r.rect.size.height
                        >= p.texels.origin.y + p.texels.size.height
            });
            assert!(covered, "glyph at {:?} was dropped by band merging", p.texels);
        }
    }

    #[test]
    fn mark_all_dirty_reuploads_whole_live_pages() {
        let mut atlas = small_atlas(64, 2);
        atlas.insert(key(0), &gray(9, 9)).expect("fits");
        drop(atlas.take_dirty_regions());
        atlas.mark_all_dirty();
        let regions = atlas.take_dirty_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].rect.size.height, 64);
        assert_eq!(regions[0].data.len(), 64 * 64);
    }

    #[test]
    fn an_empty_glyph_costs_no_atlas_area() {
        let mut atlas = small_atlas(64, 1);
        let space = GlyphImage::empty(GlyphFormat::Grayscale);
        let p = atlas.insert(key(0), &space).expect("a space is not an error");
        assert_eq!(p.page, NO_PAGE, "an empty glyph must not name a real page");
        assert_eq!(p.uv, [0.0; 4]);
        assert_eq!(p.texels.size.width, 0);
        assert_eq!(atlas.page_count(), 0, "no page may be opened for a space");
        assert_eq!(atlas.stats().used_texels, 0);
        assert_eq!(atlas.stats().allocated_texels, 0);
        assert_eq!(atlas.utilization(), 0.0);
        // It is still resident, so the caller's cache records "handled".
        assert!(atlas.contains(&key(0)));
        assert!(!atlas.has_dirty_regions());
    }

    #[test]
    fn malformed_glyph_data_is_reported_not_blitted() {
        let mut atlas = small_atlas(64, 1);
        let bad = GlyphImage {
            width: 8,
            height: 8,
            format: GlyphFormat::Grayscale,
            data: vec![0; 10], // needs 64
            bounds_em: rect(0.0, 0.0, 0.1, 0.1),
            range_em: 0.0,
        };
        let err = atlas.insert(key(0), &bad).unwrap_err();
        assert!(matches!(err, FontError::Parse { .. }), "{err:?}");
        assert_eq!(atlas.len(), 0);
        assert_eq!(atlas.page_count(), 0);
    }

    #[test]
    fn reinserting_a_resident_glyph_is_a_no_op() {
        let mut atlas = small_atlas(64, 1);
        let a = atlas.insert(key(0), &gray(8, 8)).expect("fits");
        let used = atlas.stats().used_texels;
        let b = atlas.insert(key(0), &gray(8, 8)).expect("fits");
        assert_eq!(a, b, "the same key must not be packed twice");
        assert_eq!(atlas.stats().used_texels, used);
        assert_eq!(atlas.len(), 1);
    }

    #[test]
    fn a_glyph_that_changes_shape_under_one_key_is_re_accounted() {
        // Happens when a bitmap-fallback glyph is re-rasterised at a new size:
        // the key is stable but the image is not. The old rectangle is
        // unreclaimable, but it must stop being counted as live glyph area.
        let mut atlas = small_atlas(64, 1);
        let first = atlas.insert(key(0), &gray(8, 8)).expect("fits");
        let second = atlas.insert(key(0), &gray(12, 6)).expect("fits");
        assert_ne!(first.texels, second.texels, "a different shape needs a new rectangle");
        assert_eq!(atlas.len(), 1);
        assert_eq!(atlas.stats().used_texels, 12 * 6, "the old 8x8 must not still count");
        assert_eq!(atlas.get(&key(0)), Some(second));

        // ...and the same holds when the glyph loses its ink entirely.
        atlas.insert(key(0), &GlyphImage::empty(GlyphFormat::Grayscale)).expect("a space");
        assert_eq!(atlas.stats().used_texels, 0);
        assert_eq!(atlas.get(&key(0)).map(|p| p.page), Some(NO_PAGE));
        assert_eq!(atlas.len(), 1);
    }

    #[test]
    fn utilization_stays_in_range_and_climbs_as_glyphs_are_added() {
        let mut atlas = small_atlas(128, 1);
        assert_eq!(atlas.utilization(), 0.0, "an empty atlas is 0 % utilised");
        let mut previous = 0.0f32;
        for i in 0..40u16 {
            atlas.insert(key(i), &gray(10, 10)).expect("fits in one 128 page");
            let u = atlas.utilization();
            assert!((0.0..=1.0).contains(&u), "utilization {u} out of range");
            assert!(u > previous, "utilization did not grow: {previous} -> {u}");
            previous = u;
        }
        // 40 glyphs of 100 texels in a 16384-texel page.
        assert!((previous - 4000.0 / 16384.0).abs() < 1e-6, "{previous}");
        // Packing efficiency accounts for the padding gutters, so it is under 1
        // but well above utilization: the page is mostly still empty.
        let e = atlas.packing_efficiency();
        assert!(e > previous && e < 1.0, "efficiency {e} vs utilization {previous}");
        assert!(atlas.stats().wasted_texels() > 0, "padding is real waste and must be counted");
    }

    #[test]
    fn forgetting_a_glyph_gives_its_area_back_to_the_accounting() {
        let mut atlas = small_atlas(64, 1);
        atlas.insert(key(0), &gray(10, 10)).expect("fits");
        atlas.insert(key(1), &gray(10, 10)).expect("fits");
        assert_eq!(atlas.stats().used_texels, 200);
        assert!(atlas.forget(&key(0)));
        assert!(!atlas.forget(&key(0)), "forgetting twice is not an error but is not a hit");
        assert_eq!(atlas.stats().used_texels, 100);
        // The texels are *not* reclaimed: only compact() can do that.
        assert!(atlas.stats().allocated_texels >= 242);
    }

    #[test]
    fn compaction_reclaims_forgotten_texels_and_preserves_the_survivors() {
        // The motivating case: a session churned through glyphs (a font size
        // changed, a document scrolled) and most of the atlas is now rectangles
        // that nothing references. Shelf packing has no free list, so only a
        // repack gets those texels back.
        let mut atlas = small_atlas(64, 4);
        for i in 0..24u16 {
            atlas
                .insert(key(i), &img(10, 28, GlyphFormat::Grayscale, 0x10 + i as u8))
                .expect("fits");
        }
        // Four glyphs per shelf, two shelves per 64-texel page: three pages.
        assert_eq!(atlas.stats().live_pages, 3);
        let kept: Vec<(GlyphKey, u8)> =
            [1u16, 7, 15, 23].iter().map(|i| (key(*i), 0x10 + *i as u8)).collect();
        for i in 0..24u16 {
            if !kept.iter().any(|(k, _)| *k == key(i)) {
                assert!(atlas.forget(&key(i)));
            }
        }

        let before = atlas.stats();
        assert_eq!(before.used_texels, 4 * 280);
        assert!(before.packing_efficiency() < 0.2, "{}", before.packing_efficiency());

        let report = atlas.compact();
        assert_eq!(report.moved, 4);
        assert_eq!(report.dropped, 0);
        assert_eq!(report.freed_pages, 2, "two pages held nothing but dead rectangles");
        assert!(report.bytes_after < report.bytes_before);

        let after = atlas.stats();
        assert_eq!(after.used_texels, before.used_texels, "no live glyph area may be lost");
        assert!(after.allocated_texels < before.allocated_texels / 4);
        assert!(
            after.packing_efficiency() > 0.8,
            "{} -> {}",
            before.packing_efficiency(),
            after.packing_efficiency()
        );
        assert_eq!(after.live_pages, 1);
        assert!(after.utilization() > before.utilization());

        // Every survivor is still resident and its texels still hold its own fill.
        let size = atlas.page_size() as usize;
        for (k, fill) in kept {
            let p = atlas.get(&k).expect("still resident");
            let px = atlas.page_pixels(p.page).expect("mirror");
            let (x, y) = (p.texels.origin.x as usize, p.texels.origin.y as usize);
            assert_eq!(px[y * size + x], fill, "glyph {k:?} lost its pixels");
            let last_row = y + p.texels.size.height as usize - 1;
            let last_col = x + p.texels.size.width as usize - 1;
            assert_eq!(px[last_row * size + last_col], fill, "glyph {k:?} was truncated");
        }
        // ...and the repacked page is genuinely re-uploaded.
        assert!(atlas.has_dirty_regions());
    }

    #[test]
    fn compacting_a_mixed_height_atlas_loses_nothing() {
        // A repack of a fully live atlas is a no-op as far as the caller is
        // concerned: every glyph must come back, with the same pixels, in no
        // more pages than it took before.
        let mut atlas = GlyphAtlas::with_config(AtlasConfig {
            page_size: 256,
            max_pages_per_format: 4,
            padding: 1,
            shelf_slack: 4,
            memory_budget_bytes: usize::MAX,
            evict_when_full: false,
        });
        // A deterministic scatter of heights in 6..=30, in a jumbled order.
        for i in 0..90u16 {
            let h = 6 + (u32::from(i) * 7 + 3) % 25;
            atlas.insert(key(i), &img(6, h, GlyphFormat::Grayscale, 0x40 + i as u8)).expect("fits");
        }
        let before = atlas.stats();
        let report = atlas.compact();
        let after = atlas.stats();
        assert_eq!(report.moved, 90);
        assert_eq!(report.dropped, 0);
        assert_eq!(after.glyphs, 90);
        assert_eq!(after.used_texels, before.used_texels);
        assert!(after.live_pages <= before.live_pages);

        let size = atlas.page_size() as usize;
        for i in 0..90u16 {
            let p = atlas.get(&key(i)).expect("still resident");
            assert_eq!(p.texels.size.width, 6);
            let px = atlas.page_pixels(p.page).expect("mirror");
            let (x, y) = (p.texels.origin.x as usize, p.texels.origin.y as usize);
            let last = (y + p.texels.size.height as usize - 1) * size + x + 5;
            assert_eq!(px[y * size + x], 0x40 + i as u8, "glyph {i} lost its pixels");
            assert_eq!(px[last], 0x40 + i as u8, "glyph {i} was truncated");
        }
    }

    #[test]
    fn compaction_keeps_empty_glyph_placements() {
        let mut atlas = small_atlas(64, 1);
        atlas.insert(key(0), &GlyphImage::empty(GlyphFormat::Grayscale)).expect("space");
        atlas.insert(key(1), &gray(8, 8)).expect("fits");
        let report = atlas.compact();
        assert_eq!(report.moved, 1, "the space is not packed");
        assert_eq!(atlas.len(), 2, "but it is still resident");
        assert_eq!(atlas.get(&key(0)).map(|p| p.page), Some(NO_PAGE));
    }

    #[test]
    fn eviction_frees_memory_and_pages_are_reused_not_reallocated() {
        let mut atlas = small_atlas(64, 2);
        atlas.insert(key(0), &gray(30, 30)).expect("fits");
        let with_page = atlas.memory_bytes();
        assert!(with_page >= 64 * 64);

        atlas.begin_frame();
        assert_eq!(atlas.evict_unused(0), 1);
        assert!(atlas.memory_bytes() < with_page, "an evicted page must release its mirror");
        assert_eq!(atlas.len(), 0);
        assert_eq!(atlas.page_count(), 1, "the page index is kept so it can be reused");

        let p = atlas.insert(key(1), &gray(30, 30)).expect("fits");
        assert_eq!(p.page, 0, "the recycled page is reused rather than a new one opened");
        assert_eq!(atlas.page_count(), 1);
    }

    #[test]
    fn evict_unused_spares_pages_used_recently_enough() {
        let mut atlas = small_atlas(64, 2);
        atlas.insert(key(0), &gray(30, 30)).expect("fits");
        for _ in 0..5 {
            atlas.begin_frame();
        }
        assert_eq!(atlas.evict_unused(10), 0, "idle for 5 frames, budget is 10");
        assert_eq!(atlas.evict_unused(3), 1, "idle for 5 frames, budget is 3");
    }

    #[test]
    fn the_memory_budget_evicts_until_it_is_met() {
        let mut atlas = GlyphAtlas::with_config(AtlasConfig {
            page_size: 64,
            max_pages_per_format: 4,
            padding: 1,
            shelf_slack: 4,
            // Room for one 64x64 R8 page plus map overhead, not two.
            memory_budget_bytes: 64 * 64 + 1024,
            evict_when_full: true,
        });
        for i in 0..8u16 {
            atlas.begin_frame();
            atlas.insert(key(i), &gray(30, 30)).expect("fits");
        }
        assert_eq!(atlas.stats().live_pages, 2);
        atlas.begin_frame();
        let freed = atlas.enforce_budget();
        assert_eq!(freed, 1);
        assert_eq!(atlas.stats().live_pages, 1);
        assert!(atlas.memory_bytes() <= 64 * 64 + 1024);
    }

    #[test]
    fn clear_keeps_page_mirrors_but_release_memory_does_not() {
        let mut atlas = small_atlas(64, 2);
        atlas.insert(key(0), &gray(20, 20)).expect("fits");
        atlas.clear();
        assert_eq!(atlas.len(), 0);
        assert_eq!(atlas.stats().live_pages, 1, "clear() keeps the mirror for refill");
        assert!(atlas.memory_bytes() >= 64 * 64);
        atlas.release_memory();
        assert_eq!(atlas.memory_bytes(), 0);
        assert_eq!(atlas.stats().live_pages, 0);
    }

    #[test]
    fn clear_and_compact_invalidate_cached_placements() {
        let mut atlas = small_atlas(64, 1);
        atlas.insert(key(0), &gray(8, 8)).expect("fits");
        let g0 = atlas.generation();
        atlas.clear();
        let g1 = atlas.generation();
        assert!(g1 > g0);
        atlas.insert(key(0), &gray(8, 8)).expect("fits");
        atlas.compact();
        assert!(atlas.generation() > g1);
    }

    #[test]
    fn shelf_slack_controls_how_aggressively_rows_are_shared() {
        // With zero slack a 4-tall glyph cannot join a 20-tall shelf, so each
        // distinct height opens its own row.
        let strict = AtlasConfig {
            page_size: 128,
            max_pages_per_format: 1,
            padding: 1,
            shelf_slack: 0,
            memory_budget_bytes: usize::MAX,
            evict_when_full: false,
        };
        let mut a = GlyphAtlas::with_config(strict);
        let p0 = a.insert(key(0), &gray(10, 20)).expect("fits");
        let p1 = a.insert(key(1), &gray(10, 4)).expect("fits");
        assert_ne!(p0.texels.origin.y, p1.texels.origin.y, "0 slack must not share the shelf");

        let mut b = GlyphAtlas::with_config(AtlasConfig { shelf_slack: 32, ..strict });
        let q0 = b.insert(key(0), &gray(10, 20)).expect("fits");
        let q1 = b.insert(key(1), &gray(10, 4)).expect("fits");
        assert_eq!(q0.texels.origin.y, q1.texels.origin.y, "32 slack shares the shelf");
        // ...and pays for it: the short glyph allocated a 21-tall cell.
        assert!(b.packing_efficiency() < a.packing_efficiency());
    }

    #[test]
    fn a_recycled_page_re_uploads_everything_it_zeroed() {
        // Eviction keeps the page index so the renderer keeps its texture, which
        // means that texture still holds the evicted glyphs. The replacement
        // mirror is all zeros, so unless the whole page is re-uploaded the GPU
        // keeps dead ink one texel away from a freshly packed glyph and a
        // bilinear tap pulls it in.
        let mut atlas = small_atlas(64, 1);
        for i in 0..8u16 {
            atlas.insert(key(i), &gray(30, 14)).expect("fits");
        }
        drop(atlas.take_dirty_regions());

        atlas.begin_frame();
        assert_eq!(atlas.evict_unused(0), 1, "the idle page is evicted");
        let p = atlas.insert(key(100), &gray(4, 4)).expect("the recycled page has room");
        assert_eq!(p.page, 0, "the page index is reused, so its texture is too");

        let regions = atlas.take_dirty_regions();
        let rows: u32 = regions.iter().map(|r| r.rect.size.height).sum();
        assert_eq!(rows, 64, "a recycled page must re-upload every row it zeroed");
        // ...and the upload really carries the zeroed mirror, not stale texels.
        assert!(regions.iter().all(|r| r.data.iter().all(|b| *b == 0 || *b == 0xAB)));

        // A page that never went away is not re-uploaded wholesale: the second
        // insert must cost only its own rows.
        drop(regions);
        atlas.insert(key(101), &gray(4, 4)).expect("fits");
        let rows: u32 = atlas.take_dirty_regions().iter().map(|r| r.rect.size.height).sum();
        assert!(rows < 64, "a live page must not re-upload itself, {rows} rows");
    }

    #[test]
    fn a_cleared_page_is_not_re_uploaded_because_its_texture_is_still_correct() {
        // clear() keeps the mirror, so mirror and texture still agree and the
        // stale-texture path must not fire.
        let mut atlas = small_atlas(64, 1);
        atlas.insert(key(0), &gray(20, 20)).expect("fits");
        drop(atlas.take_dirty_regions());
        atlas.clear();
        atlas.insert(key(1), &gray(4, 4)).expect("fits");
        let rows: u32 = atlas.take_dirty_regions().iter().map(|r| r.rect.size.height).sum();
        assert!(rows < 64, "clear() keeps the texture valid, {rows} rows uploaded");

        // release_memory() does drop the mirror, so it does force a full upload.
        atlas.release_memory();
        atlas.insert(key(2), &gray(4, 4)).expect("fits");
        let rows: u32 = atlas.take_dirty_regions().iter().map(|r| r.rect.size.height).sum();
        assert_eq!(rows, 64, "release_memory() invalidates the texture");
    }

    #[test]
    fn config_is_clamped_to_the_device_limit() {
        let c = AtlasConfig::default().clamped_to_device(1024);
        assert_eq!(c.page_size, 1024);
        // A backend reporting something absurdly small still yields a usable page.
        let c = AtlasConfig::default().clamped_to_device(1);
        assert_eq!(c.page_size, MIN_PAGE_SIZE);
        assert!(c.padding <= MIN_PAGE_SIZE / 4);
        // A larger limit does not inflate the requested size.
        assert_eq!(AtlasConfig::default().clamped_to_device(16384).page_size, 2048);
        assert_eq!(AtlasConfig::default().page_bytes(GlyphFormat::Mtsdf), 2048 * 2048 * 4);
        assert_eq!(AtlasConfig::default().page_bytes(GlyphFormat::Grayscale), 2048 * 2048);
        assert_eq!(AtlasConfig::default().page_bytes(GlyphFormat::Subpixel), 2048 * 2048 * 3);
    }

    #[test]
    fn an_absurd_page_size_is_clamped_instead_of_overflowing() {
        // `page_size` is a public field, so `page_bytes` can be called on a
        // config nothing sanitised: u32::MAX squared times four bytes overflows
        // usize on every target and used to panic in debug.
        let raw = AtlasConfig { page_size: u32::MAX, ..AtlasConfig::default() };
        assert_eq!(raw.page_bytes(GlyphFormat::Mtsdf), usize::MAX);
        assert_eq!(raw.page_bytes(GlyphFormat::Grayscale), u32::MAX as usize * u32::MAX as usize);

        // ...and an atlas built from it clamps to something a backend can make,
        // rather than trying to allocate an exabyte of mirror.
        let atlas = GlyphAtlas::with_config(raw);
        assert_eq!(atlas.page_size(), MAX_PAGE_SIZE);
        assert_eq!(raw.clamped_to_device(u32::MAX).page_size, MAX_PAGE_SIZE);
    }

    #[test]
    fn zero_padding_packs_flush_but_is_still_correct() {
        // Supported so a nearest-sampled bitmap page can opt out; the packer must
        // not depend on padding being nonzero.
        let mut atlas = GlyphAtlas::with_config(AtlasConfig {
            page_size: 32,
            max_pages_per_format: 1,
            padding: 0,
            shelf_slack: 0,
            memory_budget_bytes: usize::MAX,
            evict_when_full: false,
        });
        let a = atlas.insert(key(0), &gray(16, 16)).expect("fits");
        let b = atlas.insert(key(1), &gray(16, 16)).expect("fits");
        assert_eq!(a.texels.origin.x, 0);
        assert_eq!(b.texels.origin.x, 16, "no gutter with zero padding");
        assert!(!overlaps(a.texels, b.texels));
        assert!((atlas.packing_efficiency() - 1.0).abs() < 1e-6, "no padding, no waste");
        // A full-page glyph now fits exactly.
        atlas.clear();
        assert!(atlas.insert(key(2), &gray(32, 32)).is_ok());
    }

    #[test]
    fn stats_are_internally_consistent() {
        let mut atlas = small_atlas(64, 4);
        for i in 0..12u16 {
            atlas.insert(key(i), &gray(7, 11)).expect("fits");
        }
        let s = atlas.stats();
        assert_eq!(s.glyphs, 12);
        assert_eq!(s.used_texels, 12 * 7 * 11);
        assert_eq!(s.allocated_texels, 12 * 8 * 12);
        assert_eq!(s.wasted_texels(), 12 * (8 * 12 - 7 * 11));
        assert_eq!(s.page_texels, s.live_pages as u64 * 64 * 64);
        assert!(s.used_texels <= s.allocated_texels);
        assert!(s.allocated_texels <= s.page_texels);
        assert_eq!(s.memory_bytes, atlas.memory_bytes());
        assert_eq!(s.evicted_pages, 0);
    }
}
