//! The content-addressed decode cache.
//!
//! Decoding is expensive and images are drawn every frame, so the engine's
//! standing rule is that an image is decoded once and then referred to by
//! [`ImageId`]. This cache enforces that rule structurally: it is keyed by the
//! *content*, not by the call site, so two unrelated widgets that both load the
//! same 300 KB PNG share one decode, one buffer and one GPU texture.
//!
//! ## Identity
//!
//! Every entry point resolves to a [`ContentKey`], a 128-bit digest of what the
//! caller actually supplied. Loading the same bytes twice, or two different
//! paths that happen to hold the same file, both collapse onto one [`ImageId`].
//! Path lookups additionally short-circuit on the path itself (plus its size and
//! modification time) so a repeat load costs one `stat` rather than a full file
//! read: editing an asset on disk changes its modification time and therefore
//! its key, which is what makes live skin reloading work.
//!
//! ## Eviction safety
//!
//! An entry can be evicted only when **both** of these hold:
//!
//! 1. its pin count is zero, and
//! 2. it was not touched during the current frame.
//!
//! The second condition is a frame-generation guard, and it is what makes the
//! cache safe to use from a render loop without any explicit lifetime
//! management: an [`ImageId`] obtained or looked up during frame *n* cannot be
//! invalidated until [`ImageCache::begin_frame`] moves the cache to frame *n+1*.
//! Scene building, layout and submission all happen inside one frame, so a
//! handle can never go stale between the moment a widget asks for an image and
//! the moment the renderer samples it. Pinning ([`ImageCache::pin`]) is the
//! escape hatch for assets that must outlive a frame, such as an icon atlas
//! uploaded once at startup.
//!
//! A consequence worth stating plainly: loading more than the byte budget within
//! a single frame will exceed the budget rather than evict something still in
//! use. Correctness first; the budget is a target, not a hard cap.

use std::path::Path;
use std::time::UNIX_EPOCH;

use rustc_hash::FxHashMap;
use sphere_core::{GenerationalStore, ImageError, ImageId, TextureId};

use crate::decode::{self, AlphaMode, ColorSpace, DecodeOptions, DecodedImage, ImageSource};
use crate::hash;
use crate::texture::{MipChain, ResizeFilter};

/// Default byte budget: 128 MiB of decoded pixels.
///
/// Enough for a few hundred UI assets or a handful of large backdrops, and small
/// enough that a plug-in instance is not the reason a session runs out of RAM.
pub const DEFAULT_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// Domain separators, so a path key and a pixel buffer that happen to share
/// bytes cannot produce the same digest.
const SEED_ENCODED: u64 = 0x5350_4845_5245_5F45;
const SEED_RAW: u64 = 0x5350_4845_5245_5F52;
const SEED_PATH: u64 = 0x5350_4845_5245_5F50;

/// A 128-bit digest identifying what a cache entry was built from.
///
/// 128 bits is chosen so that a collision is never a plausible explanation for
/// a bug: a cache holding a million distinct images has a collision probability
/// on the order of 10^-27. The digest is fast and non-cryptographic; see the
/// `hash` module for why that trade is the right one here.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct ContentKey(u128);

impl std::fmt::Debug for ContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContentKey({:032x})", self.0)
    }
}

impl ContentKey {
    /// The key for an encoded file already in memory.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(hash::digest(SEED_ENCODED, bytes))
    }

    /// The key for a raw pixel buffer.
    ///
    /// The dimensions, alpha representation and colour space are folded into the
    /// seed, because the same bytes read as 4x4 and as 16x1 are different
    /// images and must not share an entry.
    pub fn of_rgba8(
        width: u32,
        height: u32,
        alpha: AlphaMode,
        color_space: ColorSpace,
        data: &[u8],
    ) -> Self {
        let mut header = [0u8; 10];
        header[0..4].copy_from_slice(&width.to_le_bytes());
        header[4..8].copy_from_slice(&height.to_le_bytes());
        header[8] = alpha as u8;
        header[9] = color_space as u8;
        let seed = hash::digest(SEED_RAW, &header) as u64;
        Self(hash::digest(seed, data))
    }

    /// The key for a path, including the file's size and modification time.
    ///
    /// Performs a `stat`, which is orders of magnitude cheaper than reading and
    /// decoding, and is what lets a repeat load of the same asset avoid touching
    /// the file's contents at all. Metadata that cannot be read is simply
    /// omitted, so a path on an exotic filesystem still keys deterministically,
    /// just without change detection.
    pub fn of_path(path: &Path) -> Self {
        // Canonicalising folds `./skin/knob.png` and an absolute path to the
        // same file onto one key. It fails for a path that does not exist, which
        // is fine: the load is about to fail anyway, and the raw path still
        // gives a stable key.
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut buf = canonical.as_os_str().as_encoded_bytes().to_vec();
        if let Ok(meta) = std::fs::metadata(&canonical) {
            buf.extend_from_slice(&meta.len().to_le_bytes());
            if let Ok(modified) = meta.modified() {
                if let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH) {
                    buf.extend_from_slice(&since_epoch.as_nanos().to_le_bytes());
                }
            }
        }
        Self(hash::digest(SEED_PATH, &buf))
    }

    /// The raw digest, for logging and for callers keeping their own side table.
    #[inline]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

/// Counters describing how well the cache is doing its job.
///
/// `hits + misses` is exactly the number of `load` calls: a hit is a load served
/// without decoding, a miss is a load that had to decode. That invariant is what
/// makes [`CacheStats::hit_rate`] meaningful, so lookups that are not loads
/// ([`ImageCache::get`], [`ImageCache::peek`]) deliberately do not count.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct CacheStats {
    /// Loads served from memory.
    pub hits: u64,
    /// Loads that required a decode.
    pub misses: u64,
    /// Images actually decoded. Equal to `misses` unless a decode failed.
    pub decodes: u64,
    /// Entries removed to stay within budget.
    pub evictions: u64,
    /// Bytes reclaimed by eviction, cumulative.
    pub evicted_bytes: u64,
    /// Bytes of decoded pixels admitted, cumulative.
    pub decoded_bytes: u64,
}

impl CacheStats {
    /// Fraction of loads served without decoding, in `0..=1`.
    ///
    /// Returns 1.0 when nothing has been loaded, because "no misses" is the
    /// honest reading of an empty history and a zero would look like a problem.
    pub fn hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 { 1.0 } else { self.hits as f32 / total as f32 }
    }
}

/// An entry the cache dropped, reported so the renderer can free its texture.
///
/// The cache never touches the GPU, so it cannot free a texture itself; it can
/// only tell the owner that nothing refers to it any more.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Evicted {
    /// The handle that is now stale. Every copy of it will fail to resolve.
    pub id: ImageId,
    /// The GPU texture that was associated with it, if any.
    pub texture: Option<TextureId>,
    /// Bytes of decoded pixels reclaimed, including mip levels.
    pub bytes: usize,
}

struct CacheEntry {
    /// Level 0 plus any generated mips. Storing the base inside the chain is
    /// what stops the cache from holding two copies of it once mips are built.
    levels: MipChain,
    texture: Option<TextureId>,
    content: ContentKey,
    /// Path keys aliased onto this entry. Usually zero or one; more when the
    /// same file is reached through several paths.
    path_keys: Vec<ContentKey>,
    pins: u32,
    last_frame: u64,
    last_used: u64,
    bytes: usize,
}

/// A byte-budgeted, content-addressed cache of decoded images.
///
/// See the module documentation for the identity and eviction rules.
pub struct ImageCache {
    entries: GenerationalStore<ImageId, CacheEntry>,
    by_content: FxHashMap<ContentKey, ImageId>,
    by_path: FxHashMap<ContentKey, ImageId>,
    options: DecodeOptions,
    budget_bytes: usize,
    bytes: usize,
    frame: u64,
    /// Monotonic access counter giving an exact LRU order. A counter rather than
    /// a timestamp because two accesses in the same microsecond still need a
    /// defined order, and because it cannot go backwards when the clock does.
    clock: u64,
    stats: CacheStats,
    evicted: Vec<Evicted>,
    over_budget_warned: bool,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

impl std::fmt::Debug for ImageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCache")
            .field("entries", &self.entries.len())
            .field("bytes", &self.bytes)
            .field("budget_bytes", &self.budget_bytes)
            .field("frame", &self.frame)
            .field("stats", &self.stats)
            .finish()
    }
}

impl ImageCache {
    /// An empty cache with the given byte budget.
    pub fn new(budget_bytes: usize) -> Self {
        Self::with_options(budget_bytes, DecodeOptions::default())
    }

    /// An empty cache with an explicit decode configuration.
    ///
    /// Hosts should pass the device's real `max_texture_dimension_2d` here so an
    /// oversized asset fails at load with a clear error instead of at upload
    /// with a driver message.
    pub fn with_options(budget_bytes: usize, options: DecodeOptions) -> Self {
        Self {
            entries: GenerationalStore::new(),
            by_content: FxHashMap::default(),
            by_path: FxHashMap::default(),
            options,
            budget_bytes,
            bytes: 0,
            frame: 0,
            clock: 0,
            stats: CacheStats::default(),
            evicted: Vec::new(),
            over_budget_warned: false,
        }
    }

    /// The decode configuration applied to every load.
    #[inline]
    pub fn options(&self) -> &DecodeOptions {
        &self.options
    }

    /// Replaces the decode configuration. Already-cached entries are unaffected.
    pub fn set_options(&mut self, options: DecodeOptions) {
        self.options = options;
    }

    /// The byte budget.
    #[inline]
    pub const fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// Sets the byte budget and immediately evicts down to it.
    ///
    /// Returns the number of entries evicted, so a host lowering the budget
    /// under memory pressure can log what it cost.
    pub fn set_budget_bytes(&mut self, budget_bytes: usize) -> usize {
        self.budget_bytes = budget_bytes;
        self.evict_to_budget()
    }

    /// Bytes of decoded pixels currently held, including generated mip levels.
    ///
    /// Excludes the cache's own bookkeeping, which is a few dozen bytes per
    /// entry and irrelevant next to the pixel buffers.
    #[inline]
    pub const fn memory_bytes(&self) -> usize {
        self.bytes
    }

    /// Number of live entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is cached.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The current frame number.
    #[inline]
    pub const fn frame(&self) -> u64 {
        self.frame
    }

    /// Advances to the next frame, releasing the frame-generation guard.
    ///
    /// Call once per frame, before building the scene. Everything touched during
    /// the frame that just ended becomes evictable again.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Accumulated counters.
    #[inline]
    pub const fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// Zeroes the counters, leaving the cached images alone.
    pub fn reset_stats(&mut self) {
        self.stats = CacheStats::default();
    }

    /// Loads from any source, decoding only if the content is not already held.
    pub fn load(&mut self, source: ImageSource<'_>) -> Result<ImageId, ImageError> {
        match source {
            ImageSource::Bytes(bytes) => self.load_bytes(bytes),
            ImageSource::Path(path) => self.load_path(path),
            ImageSource::Rgba8 { width, height, data, alpha, color_space } => {
                self.load_rgba8_in(width, height, data, alpha, color_space)
            }
        }
    }

    /// Loads an encoded PNG, JPEG or WebP already in memory.
    pub fn load_bytes(&mut self, bytes: &[u8]) -> Result<ImageId, ImageError> {
        let key = ContentKey::of_bytes(bytes);
        if let Some(id) = self.resolve_content(key) {
            self.stats.hits += 1;
            self.touch_entry(id);
            return Ok(id);
        }
        let image = self.decode_counted(|options| decode::decode_bytes(bytes, options))?;
        Ok(self.admit(key, Vec::new(), image))
    }

    /// Loads an encoded file from disk.
    ///
    /// Three levels of short-circuit, cheapest first: the path key (a `stat`),
    /// the content key (a file read), and only then a decode.
    pub fn load_path(&mut self, path: &Path) -> Result<ImageId, ImageError> {
        let path_key = ContentKey::of_path(path);
        if let Some(id) = self.resolve_path(path_key) {
            self.stats.hits += 1;
            self.touch_entry(id);
            return Ok(id);
        }
        let bytes = std::fs::read(path)
            .map_err(|source| ImageError::Io { path: path.display().to_string(), source })?;
        let content_key = ContentKey::of_bytes(&bytes);
        if let Some(id) = self.resolve_content(content_key) {
            // A second path onto content we already hold: alias it so the next
            // load stops at the `stat`, and do not decode again.
            self.stats.hits += 1;
            self.by_path.insert(path_key, id);
            if let Some(entry) = self.entries.get_mut(id) {
                entry.path_keys.push(path_key);
            }
            self.touch_entry(id);
            return Ok(id);
        }
        let image = self.decode_counted(|options| decode::decode_bytes(&bytes, options))?;
        Ok(self.admit(content_key, vec![path_key], image))
    }

    /// Loads a straight-alpha sRGB RGBA8 buffer.
    pub fn load_rgba8(
        &mut self,
        width: u32,
        height: u32,
        data: &[u8],
        alpha: AlphaMode,
    ) -> Result<ImageId, ImageError> {
        self.load_rgba8_in(width, height, data, alpha, ColorSpace::Srgb)
    }

    fn load_rgba8_in(
        &mut self,
        width: u32,
        height: u32,
        data: &[u8],
        alpha: AlphaMode,
        color_space: ColorSpace,
    ) -> Result<ImageId, ImageError> {
        let key = ContentKey::of_rgba8(width, height, alpha, color_space, data);
        if let Some(id) = self.resolve_content(key) {
            self.stats.hits += 1;
            self.touch_entry(id);
            return Ok(id);
        }
        let source = ImageSource::Rgba8 { width, height, data, alpha, color_space };
        let image = self.decode_counted(|options| decode::decode(source, options))?;
        Ok(self.admit(key, Vec::new(), image))
    }

    /// Inserts an image the caller decoded or synthesised itself.
    ///
    /// Keyed by the resulting pixels, so inserting an identical image twice
    /// still yields one entry. Does not move the hit and miss counters: nothing
    /// was loaded, so counting it would break the invariant that
    /// `hits + misses` is the number of `load` calls.
    pub fn insert(&mut self, image: DecodedImage) -> ImageId {
        let key = ContentKey::of_rgba8(
            image.width(),
            image.height(),
            image.alpha_mode(),
            image.color_space(),
            image.data(),
        );
        if let Some(id) = self.resolve_content(key) {
            self.touch_entry(id);
            return id;
        }
        self.admit(key, Vec::new(), image)
    }

    /// Looks up the id for a key without touching or loading anything.
    pub fn lookup(&self, key: ContentKey) -> Option<ImageId> {
        self.by_content.get(&key).copied().filter(|id| self.entries.contains(*id))
    }

    /// Borrows an image and marks it used this frame.
    ///
    /// Takes `&mut self` on purpose: touching is what protects the entry from
    /// eviction for the rest of the frame, and a `&self` accessor would make
    /// forgetting to touch the default. Use [`ImageCache::peek`] for genuinely
    /// read-only inspection.
    pub fn get(&mut self, id: ImageId) -> Option<&DecodedImage> {
        self.touch_entry(id);
        self.entries.get(id).map(|e| e.levels.base())
    }

    /// Borrows an image without touching it or affecting eviction order.
    pub fn peek(&self, id: ImageId) -> Option<&DecodedImage> {
        self.entries.get(id).map(|e| e.levels.base())
    }

    /// Marks an entry used this frame. Returns false for a stale id.
    pub fn touch(&mut self, id: ImageId) -> bool {
        self.touch_entry(id);
        self.entries.contains(id)
    }

    /// True when the handle still resolves.
    #[inline]
    pub fn contains(&self, id: ImageId) -> bool {
        self.entries.contains(id)
    }

    /// The GPU texture associated with an entry, if the renderer uploaded one.
    pub fn texture(&self, id: ImageId) -> Option<TextureId> {
        self.entries.get(id).and_then(|e| e.texture)
    }

    /// Associates a GPU texture with an entry. Returns false for a stale id.
    ///
    /// The cache stores the handle and hands it back on eviction; it never
    /// creates or destroys the texture itself.
    pub fn set_texture(&mut self, id: ImageId, texture: TextureId) -> bool {
        match self.entries.get_mut(id) {
            Some(entry) => {
                entry.texture = Some(texture);
                true
            }
            None => false,
        }
    }

    /// Detaches the GPU texture from an entry, returning it.
    pub fn clear_texture(&mut self, id: ImageId) -> Option<TextureId> {
        self.entries.get_mut(id).and_then(|e| e.texture.take())
    }

    /// The mip chain for an entry. Always at least level 0 for a live entry.
    pub fn mips(&self, id: ImageId) -> Option<&MipChain> {
        self.entries.get(id).map(|e| &e.levels)
    }

    /// Generates a full mip chain for an entry, in place.
    ///
    /// Idempotent: an entry that already has mips is left alone. The extra
    /// levels count towards the budget, which is why this is opt-in rather than
    /// automatic — a UI that only ever draws images at their natural size would
    /// be paying a third more memory for nothing.
    ///
    /// Returns [`ImageError::Decode`] for an id that has been evicted, since a
    /// stale handle is a caller error rather than an image problem.
    pub fn build_mips(&mut self, id: ImageId, filter: ResizeFilter) -> Result<(), ImageError> {
        let Some(entry) = self.entries.get(id) else {
            return Err(ImageError::Decode(format!("{id:?} is not in the cache")));
        };
        if entry.levels.has_mips() {
            return Ok(());
        }
        let old_bytes = entry.bytes;
        // Build before taking the mutable borrow, so a failure leaves the entry
        // exactly as it was.
        let chain = MipChain::generate(entry.levels.base(), filter)?;
        let new_bytes = chain.byte_len();
        if let Some(entry) = self.entries.get_mut(id) {
            entry.levels = chain;
            entry.bytes = new_bytes;
        }
        self.bytes = self.bytes.saturating_sub(old_bytes) + new_bytes;
        self.evict_to_budget();
        Ok(())
    }

    /// Pins an entry so it cannot be evicted. Returns false for a stale id.
    ///
    /// Pins nest: two calls need two [`ImageCache::unpin`] calls. Use for assets
    /// that must survive across frames, such as anything uploaded once at
    /// startup and referenced by a long-lived scene node.
    pub fn pin(&mut self, id: ImageId) -> bool {
        match self.entries.get_mut(id) {
            Some(entry) => {
                entry.pins = entry.pins.saturating_add(1);
                true
            }
            None => false,
        }
    }

    /// Releases one pin. Returns false for a stale id or an unpinned entry.
    ///
    /// Saturates at zero rather than wrapping: an unbalanced unpin is a bug, but
    /// silently turning the pin count into four billion would turn it into a
    /// permanent leak instead of a recoverable one.
    pub fn unpin(&mut self, id: ImageId) -> bool {
        match self.entries.get_mut(id) {
            Some(entry) if entry.pins > 0 => {
                entry.pins -= 1;
                true
            }
            _ => false,
        }
    }

    /// The pin count, or zero for a stale id.
    pub fn pin_count(&self, id: ImageId) -> u32 {
        self.entries.get(id).map_or(0, |e| e.pins)
    }

    /// Removes an entry regardless of pins, returning its base image.
    ///
    /// Does not report through [`ImageCache::take_evicted`]: the caller asked
    /// for this and already knows. Any associated texture is dropped from the
    /// cache's bookkeeping, so read it with [`ImageCache::texture`] first if the
    /// renderer still needs to free it.
    pub fn remove(&mut self, id: ImageId) -> Option<DecodedImage> {
        let entry = self.entries.remove(id)?;
        self.unindex(id, &entry);
        self.bytes = self.bytes.saturating_sub(entry.bytes);
        entry.levels.into_base()
    }

    /// Drops every entry and resets memory accounting. Counters are kept.
    ///
    /// Evicted entries are *not* reported: a clear is usually part of a teardown
    /// where the renderer is dropping every texture anyway.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.by_content.clear();
        self.by_path.clear();
        self.bytes = 0;
        self.over_budget_warned = false;
    }

    /// Takes the list of entries evicted since the last call.
    ///
    /// The renderer should drain this once per frame and free the textures it
    /// names; nothing else will.
    pub fn take_evicted(&mut self) -> Vec<Evicted> {
        std::mem::take(&mut self.evicted)
    }

    /// Evicts until the budget is met, returning how many entries went.
    ///
    /// Stops early when everything left is pinned or in use this frame, which is
    /// the one case where the cache deliberately runs over budget.
    pub fn evict_to_budget(&mut self) -> usize {
        let mut count = 0;
        while self.bytes > self.budget_bytes {
            let Some(victim) = self.pick_victim() else {
                if !self.over_budget_warned {
                    self.over_budget_warned = true;
                    tracing::warn!(
                        bytes = self.bytes,
                        budget = self.budget_bytes,
                        entries = self.entries.len(),
                        "image cache is over budget but every entry is pinned or in use this \
                         frame; holding them rather than invalidating a live handle"
                    );
                }
                return count;
            };
            self.evict(victim);
            count += 1;
        }
        self.over_budget_warned = false;
        count
    }

    /// The least recently used entry that is neither pinned nor used this frame.
    ///
    /// A linear scan. Entry counts are in the hundreds and eviction happens on
    /// load rather than per draw, so a heap would add invalidation complexity to
    /// save microseconds nobody can measure.
    fn pick_victim(&self) -> Option<ImageId> {
        self.entries
            .iter()
            .filter(|(_, e)| e.pins == 0 && e.last_frame != self.frame)
            .min_by_key(|(_, e)| e.last_used)
            .map(|(id, _)| id)
    }

    fn evict(&mut self, id: ImageId) {
        let Some(entry) = self.entries.remove(id) else { return };
        self.unindex(id, &entry);
        self.bytes = self.bytes.saturating_sub(entry.bytes);
        self.stats.evictions += 1;
        self.stats.evicted_bytes += entry.bytes as u64;
        tracing::trace!(?id, bytes = entry.bytes, "evicted image");
        self.evicted.push(Evicted { id, texture: entry.texture, bytes: entry.bytes });
    }

    /// Removes an entry's keys from the lookup maps.
    ///
    /// A key is only dropped when it still points at *this* entry. That matters
    /// for path keys: if a file changed and a newer entry has taken over one of
    /// the aliases, evicting the stale entry must not unhook the live one.
    fn unindex(&mut self, id: ImageId, entry: &CacheEntry) {
        if self.by_content.get(&entry.content) == Some(&id) {
            self.by_content.remove(&entry.content);
        }
        for key in &entry.path_keys {
            if self.by_path.get(key) == Some(&id) {
                self.by_path.remove(key);
            }
        }
    }

    fn resolve_content(&mut self, key: ContentKey) -> Option<ImageId> {
        let id = *self.by_content.get(&key)?;
        if self.entries.contains(id) {
            Some(id)
        } else {
            // Defensive: eviction unindexes, so this should be unreachable. If
            // it ever happens, dropping the dangling key is better than handing
            // back a stale handle.
            self.by_content.remove(&key);
            None
        }
    }

    fn resolve_path(&mut self, key: ContentKey) -> Option<ImageId> {
        let id = *self.by_path.get(&key)?;
        if self.entries.contains(id) {
            Some(id)
        } else {
            self.by_path.remove(&key);
            None
        }
    }

    /// Runs a decode, counting the miss and the decode separately so a failed
    /// decode is visible as `misses > decodes`.
    fn decode_counted(
        &mut self,
        decode_fn: impl FnOnce(&DecodeOptions) -> Result<DecodedImage, ImageError>,
    ) -> Result<DecodedImage, ImageError> {
        self.stats.misses += 1;
        let image = decode_fn(&self.options)?;
        self.stats.decodes += 1;
        Ok(image)
    }

    fn admit(
        &mut self,
        content: ContentKey,
        path_keys: Vec<ContentKey>,
        image: DecodedImage,
    ) -> ImageId {
        let bytes = image.byte_len();
        self.clock += 1;
        let entry = CacheEntry {
            levels: MipChain::single(image),
            texture: None,
            content,
            path_keys,
            pins: 0,
            last_frame: self.frame,
            last_used: self.clock,
            bytes,
        };
        let keys = entry.path_keys.clone();
        let id = self.entries.insert(entry);
        self.by_content.insert(content, id);
        for key in keys {
            self.by_path.insert(key, id);
        }
        self.bytes += bytes;
        self.stats.decoded_bytes += bytes as u64;
        // The new entry carries the current frame stamp, so it is guaranteed to
        // survive its own admission.
        self.evict_to_budget();
        id
    }

    fn touch_entry(&mut self, id: ImageId) {
        self.clock += 1;
        let (clock, frame) = (self.clock, self.frame);
        if let Some(entry) = self.entries.get_mut(id) {
            entry.last_used = clock;
            entry.last_frame = frame;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::tests::{encode_png, rgba_checkerboard};

    /// A 5x5 RGBA image, exactly 100 bytes, distinguished by `tag`.
    fn raw(tag: u8) -> Vec<u8> {
        let mut data = vec![tag; 100];
        // Keep every texel opaque so premultiplication cannot change the bytes,
        // which keeps the byte count exactly 100 and the maths in the eviction
        // tests readable.
        for i in 0..25 {
            data[i * 4 + 3] = 255;
        }
        data
    }

    fn load_raw(cache: &mut ImageCache, tag: u8) -> ImageId {
        cache.load_rgba8(5, 5, &raw(tag), AlphaMode::Straight).expect("valid raw image")
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("sphere-image-tests").join(name);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_same_bytes_decode_once_and_share_an_id() {
        let png = encode_png(4, 4, &rgba_checkerboard(4, 4));
        let mut cache = ImageCache::default();
        let a = cache.load_bytes(&png).expect("load");
        let b = cache.load_bytes(&png).expect("load");
        assert_eq!(a, b, "identical content must share one id");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.stats().decodes, 1, "the engine rule is: decode once");
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1);

        // A separately built but byte-identical buffer must also hit.
        let png2 = encode_png(4, 4, &rgba_checkerboard(4, 4));
        assert_eq!(cache.load_bytes(&png2).expect("load"), a);
        assert_eq!(cache.stats().decodes, 1);
    }

    #[test]
    fn different_bytes_get_different_ids() {
        let mut cache = ImageCache::default();
        let a = cache.load_bytes(&encode_png(4, 4, &rgba_checkerboard(4, 4))).expect("load");
        let b = cache.load_bytes(&encode_png(4, 5, &rgba_checkerboard(4, 5))).expect("load");
        assert_ne!(a, b);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.stats().decodes, 2);
    }

    #[test]
    fn raw_buffers_are_keyed_by_pixels_and_shape() {
        let mut cache = ImageCache::default();
        let a = load_raw(&mut cache, 7);
        let b = load_raw(&mut cache, 7);
        assert_eq!(a, b);
        assert_ne!(a, load_raw(&mut cache, 8));
        // The same 100 bytes read as a different shape is a different image.
        let reshaped = cache.load_rgba8(25, 1, &raw(7), AlphaMode::Straight).expect("load");
        assert_ne!(a, reshaped);
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn the_same_path_loads_once() {
        let dir = temp_dir("same-path");
        let path = dir.join("knob.png");
        std::fs::write(&path, encode_png(4, 4, &rgba_checkerboard(4, 4))).expect("write");
        let mut cache = ImageCache::default();
        let a = cache.load_path(&path).expect("load");
        let b = cache.load_path(&path).expect("load");
        assert_eq!(a, b);
        assert_eq!(cache.stats().decodes, 1, "a repeat path load must not decode");
        assert_eq!(cache.stats().hits, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn two_paths_to_identical_content_share_one_decode() {
        let dir = temp_dir("aliased-paths");
        let bytes = encode_png(4, 4, &rgba_checkerboard(4, 4));
        let a_path = dir.join("copy-a.png");
        let b_path = dir.join("copy-b.png");
        std::fs::write(&a_path, &bytes).expect("write");
        std::fs::write(&b_path, &bytes).expect("write");

        let mut cache = ImageCache::default();
        let a = cache.load_path(&a_path).expect("load");
        let b = cache.load_path(&b_path).expect("load");
        assert_eq!(a, b, "identical files must collapse onto one entry");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.stats().decodes, 1);
        // The second path is now aliased, so a repeat load stops at the stat.
        assert_eq!(cache.load_path(&b_path).expect("load"), a);
        assert_eq!(cache.stats().decodes, 1);
        let _ = std::fs::remove_file(&a_path);
        let _ = std::fs::remove_file(&b_path);
    }

    #[test]
    fn bytes_and_the_file_holding_them_share_an_entry() {
        let dir = temp_dir("bytes-and-path");
        let bytes = encode_png(3, 3, &rgba_checkerboard(3, 3));
        let path = dir.join("shared.png");
        std::fs::write(&path, &bytes).expect("write");
        let mut cache = ImageCache::default();
        let from_bytes = cache.load_bytes(&bytes).expect("load");
        let from_path = cache.load_path(&path).expect("load");
        assert_eq!(from_bytes, from_path);
        assert_eq!(cache.stats().decodes, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_an_error_and_leaves_no_entry() {
        let mut cache = ImageCache::default();
        let err = cache.load_path(Path::new("W:/nope/sphere-image-absent.png"));
        assert!(matches!(err, Err(ImageError::Io { .. })));
        assert!(cache.is_empty());
        assert_eq!(cache.memory_bytes(), 0);
    }

    #[test]
    fn a_failed_decode_leaves_the_cache_clean_and_visible_in_the_stats() {
        let mut cache = ImageCache::default();
        assert!(cache.load_bytes(b"not an image at all").is_err());
        assert!(cache.is_empty());
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().decodes, 0, "a failed decode is a miss but not a decode");
        // ...and retrying still does not poison anything.
        assert!(cache.load_bytes(b"not an image at all").is_err());
        assert_eq!(cache.stats().misses, 2);
    }

    #[test]
    fn memory_bytes_tracks_what_is_actually_held() {
        let mut cache = ImageCache::default();
        assert_eq!(cache.memory_bytes(), 0);
        let a = load_raw(&mut cache, 1);
        assert_eq!(cache.memory_bytes(), 100);
        load_raw(&mut cache, 2);
        assert_eq!(cache.memory_bytes(), 200);
        // A repeat load must not double count.
        load_raw(&mut cache, 2);
        assert_eq!(cache.memory_bytes(), 200);
        assert!(cache.remove(a).is_some());
        assert_eq!(cache.memory_bytes(), 100);
        cache.clear();
        assert_eq!(cache.memory_bytes(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn removing_an_entry_returns_its_pixels_and_frees_its_key() {
        let mut cache = ImageCache::default();
        let a = load_raw(&mut cache, 3);
        let image = cache.remove(a).expect("removed");
        assert_eq!(image.dimensions(), (5, 5));
        assert!(!cache.contains(a));
        assert!(cache.remove(a).is_none(), "double remove must be a no-op");
        // The key is gone, so the next load decodes again rather than resolving
        // a dangling entry.
        let b = load_raw(&mut cache, 3);
        assert_ne!(a, b);
        assert_eq!(cache.stats().decodes, 2);
    }

    #[test]
    fn exceeding_the_budget_evicts_the_least_recently_used() {
        let mut cache = ImageCache::new(260);
        let a = load_raw(&mut cache, 1);
        cache.begin_frame();
        let b = load_raw(&mut cache, 2);
        cache.begin_frame();
        // Touching A makes B the least recently used.
        assert!(cache.get(a).is_some());
        let c = load_raw(&mut cache, 3);

        assert_eq!(cache.memory_bytes(), 200);
        assert!(cache.peek(b).is_none(), "B was the LRU and should have gone");
        assert!(cache.peek(a).is_some(), "A was touched most recently");
        assert!(cache.peek(c).is_some());
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(cache.stats().evicted_bytes, 100);
    }

    #[test]
    fn everything_loaded_this_frame_survives_even_over_budget() {
        // The frame-generation guard. A scene that loads three images in one
        // frame must be able to draw all three, budget or no budget.
        let mut cache = ImageCache::new(150);
        let a = load_raw(&mut cache, 1);
        let b = load_raw(&mut cache, 2);
        assert!(cache.peek(a).is_some(), "an id handed out this frame must stay valid");
        assert!(cache.peek(b).is_some());
        assert_eq!(cache.memory_bytes(), 200, "the budget yields to a live handle");
        assert_eq!(cache.stats().evictions, 0);

        // On the next frame the guard lifts and the backlog is collected.
        cache.begin_frame();
        let c = load_raw(&mut cache, 3);
        assert!(cache.peek(a).is_none());
        assert!(cache.peek(b).is_none());
        assert!(cache.peek(c).is_some());
        assert_eq!(cache.memory_bytes(), 100);
        assert_eq!(cache.stats().evictions, 2);
    }

    #[test]
    fn a_pinned_entry_survives_eviction() {
        let mut cache = ImageCache::new(150);
        let a = load_raw(&mut cache, 1);
        assert!(cache.pin(a));
        assert_eq!(cache.pin_count(a), 1);

        // Several frames of pressure, none of which may touch A.
        for tag in 2..8u8 {
            cache.begin_frame();
            load_raw(&mut cache, tag);
        }
        assert!(cache.peek(a).is_some(), "a pinned entry must never be evicted");

        // Once unpinned it becomes an ordinary candidate again.
        assert!(cache.unpin(a));
        assert_eq!(cache.pin_count(a), 0);
        cache.begin_frame();
        load_raw(&mut cache, 100);
        assert!(cache.peek(a).is_none(), "an unpinned entry should be reclaimable");
    }

    #[test]
    fn pins_nest_and_unbalanced_unpins_saturate() {
        let mut cache = ImageCache::new(150);
        let a = load_raw(&mut cache, 1);
        assert!(cache.pin(a));
        assert!(cache.pin(a));
        assert_eq!(cache.pin_count(a), 2);
        assert!(cache.unpin(a));
        assert_eq!(cache.pin_count(a), 1);
        assert!(cache.unpin(a));
        assert_eq!(cache.pin_count(a), 0);
        // The extra unpin must report failure rather than wrapping to u32::MAX.
        assert!(!cache.unpin(a));
        assert_eq!(cache.pin_count(a), 0);
    }

    #[test]
    fn eviction_reports_the_texture_the_renderer_must_free() {
        let mut cache = ImageCache::new(150);
        let a = load_raw(&mut cache, 1);
        let texture = TextureId::new(3, 1);
        assert!(cache.set_texture(a, texture));
        assert_eq!(cache.texture(a), Some(texture));

        cache.begin_frame();
        load_raw(&mut cache, 2);

        let evicted = cache.take_evicted();
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].id, a);
        assert_eq!(evicted[0].texture, Some(texture));
        assert_eq!(evicted[0].bytes, 100);
        // The list is drained, not copied.
        assert!(cache.take_evicted().is_empty());
    }

    #[test]
    fn a_stale_id_resolves_to_nothing_rather_than_someone_elses_image() {
        // Generational ids make slot reuse detectable; this is the guard that
        // keeps an evicted handle from aliasing whatever moved into its slot.
        let mut cache = ImageCache::new(150);
        let a = load_raw(&mut cache, 1);
        cache.begin_frame();
        load_raw(&mut cache, 2);
        assert!(!cache.contains(a), "A should have been evicted");
        cache.begin_frame();
        let c = load_raw(&mut cache, 3);
        assert_eq!(a.index(), c.index(), "the freed slot should have been reused");
        assert_ne!(a, c, "reuse must bump the generation");

        assert!(cache.peek(a).is_none(), "a stale handle must not alias the new occupant");
        assert!(!cache.set_texture(a, TextureId::new(1, 1)));
        assert!(!cache.touch(a));
        assert_eq!(cache.pin_count(a), 0);
        assert!(!cache.pin(a));
        assert!(cache.peek(c).is_some());
    }

    #[test]
    fn an_evicted_key_is_reloadable() {
        let mut cache = ImageCache::new(150);
        let a = load_raw(&mut cache, 1);
        cache.begin_frame();
        load_raw(&mut cache, 2);
        assert!(!cache.contains(a));
        // The content key went with the entry, so this is a fresh decode with a
        // fresh id rather than a resurrected dangling handle.
        cache.begin_frame();
        let a2 = load_raw(&mut cache, 1);
        assert_ne!(a, a2);
        assert!(cache.peek(a2).is_some());
        assert_eq!(cache.stats().decodes, 3);
    }

    #[test]
    fn an_image_larger_than_the_whole_budget_is_still_admitted() {
        // Refusing to cache it would mean refusing to draw it. Running over
        // budget is the lesser evil, and the warning says so.
        let mut cache = ImageCache::new(10);
        let a = load_raw(&mut cache, 1);
        assert!(cache.peek(a).is_some());
        assert_eq!(cache.memory_bytes(), 100);
        cache.begin_frame();
        let b = load_raw(&mut cache, 2);
        // The previous entry is reclaimable now, so only the newest survives.
        assert!(cache.peek(a).is_none());
        assert!(cache.peek(b).is_some());
        assert_eq!(cache.memory_bytes(), 100);
    }

    #[test]
    fn lowering_the_budget_evicts_immediately() {
        let mut cache = ImageCache::new(1000);
        load_raw(&mut cache, 1);
        cache.begin_frame();
        load_raw(&mut cache, 2);
        cache.begin_frame();
        load_raw(&mut cache, 3);
        assert_eq!(cache.memory_bytes(), 300);
        // Frame 2 is current, so the third image is protected; the other two go.
        let evicted = cache.set_budget_bytes(100);
        assert_eq!(evicted, 2);
        assert_eq!(cache.memory_bytes(), 100);
        assert_eq!(cache.budget_bytes(), 100);
    }

    #[test]
    fn building_mips_grows_the_accounted_memory_without_duplicating_level_zero() {
        let mut cache = ImageCache::new(DEFAULT_BUDGET_BYTES);
        let png = encode_png(64, 64, &rgba_checkerboard(64, 64));
        let id = cache.load_bytes(&png).expect("load");
        let base_bytes = cache.memory_bytes();
        assert_eq!(base_bytes, 64 * 64 * 4);

        cache.build_mips(id, ResizeFilter::Box).expect("mips");
        let (levels, chain_bytes, has_mips) = {
            let chain = cache.mips(id).expect("chain");
            (chain.len(), chain.byte_len(), chain.has_mips())
        };
        assert_eq!(levels, 7);
        assert!(has_mips);
        assert_eq!(cache.memory_bytes(), chain_bytes);
        // A full pyramid is about 4/3 of the base, not 7/3, which is what it
        // would be if the base were stored alongside the chain instead of in it.
        let ratio = cache.memory_bytes() as f64 / base_bytes as f64;
        assert!((ratio - 4.0 / 3.0).abs() < 0.01, "ratio {ratio}");

        // Idempotent: a second call must not rebuild or double the accounting.
        cache.build_mips(id, ResizeFilter::Box).expect("mips again");
        assert_eq!(cache.memory_bytes(), chain_bytes);
        assert_eq!(cache.mips(id).map(MipChain::len), Some(7));
    }

    #[test]
    fn building_mips_for_a_stale_id_is_an_error_not_a_panic() {
        let mut cache = ImageCache::new(150);
        let a = load_raw(&mut cache, 1);
        cache.begin_frame();
        load_raw(&mut cache, 2);
        assert!(matches!(cache.build_mips(a, ResizeFilter::Box), Err(ImageError::Decode(_))));
    }

    #[test]
    fn get_touches_and_peek_does_not() {
        let mut cache = ImageCache::new(260);
        let a = load_raw(&mut cache, 1);
        cache.begin_frame();
        let b = load_raw(&mut cache, 2);
        cache.begin_frame();
        // Peeking must not rescue A from being the LRU.
        assert!(cache.peek(a).is_some());
        load_raw(&mut cache, 3);
        assert!(cache.peek(a).is_none(), "peek must not affect eviction order");
        assert!(cache.peek(b).is_some());
    }

    #[test]
    fn lookup_finds_live_entries_only() {
        let mut cache = ImageCache::new(150);
        let key = ContentKey::of_rgba8(5, 5, AlphaMode::Straight, ColorSpace::Srgb, &raw(1));
        assert_eq!(cache.lookup(key), None);
        let a = load_raw(&mut cache, 1);
        assert_eq!(cache.lookup(key), Some(a));
        cache.begin_frame();
        load_raw(&mut cache, 2);
        assert_eq!(cache.lookup(key), None, "an evicted key must not resolve");
    }

    #[test]
    fn inserting_a_decoded_image_is_content_keyed_too() {
        let mut cache = ImageCache::default();
        let make = || {
            DecodedImage::from_rgba8(
                5,
                5,
                raw(9),
                AlphaMode::Straight,
                ColorSpace::Srgb,
                &DecodeOptions::default(),
            )
            .expect("image")
        };
        let a = cache.insert(make());
        let b = cache.insert(make());
        assert_eq!(a, b);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.memory_bytes(), 100);
    }

    #[test]
    fn hit_rate_reflects_the_load_history() {
        let mut cache = ImageCache::default();
        assert_eq!(cache.stats().hit_rate(), 1.0, "an empty history is not a failure");
        load_raw(&mut cache, 1);
        assert_eq!(cache.stats().hit_rate(), 0.0);
        load_raw(&mut cache, 1);
        assert_eq!(cache.stats().hit_rate(), 0.5);
        load_raw(&mut cache, 1);
        load_raw(&mut cache, 1);
        assert_eq!(cache.stats().hit_rate(), 0.75);
        assert_eq!(cache.stats().hits + cache.stats().misses, 4);
        cache.reset_stats();
        assert_eq!(*cache.stats(), CacheStats::default());
        assert_eq!(cache.len(), 1, "resetting counters must not drop entries");
    }

    #[test]
    fn textures_can_be_attached_and_detached() {
        let mut cache = ImageCache::default();
        let a = load_raw(&mut cache, 1);
        assert_eq!(cache.texture(a), None);
        assert!(cache.set_texture(a, TextureId::new(9, 2)));
        assert_eq!(cache.texture(a), Some(TextureId::new(9, 2)));
        assert_eq!(cache.clear_texture(a), Some(TextureId::new(9, 2)));
        assert_eq!(cache.texture(a), None);
        assert_eq!(cache.clear_texture(a), None);
    }

    #[test]
    fn content_keys_separate_their_domains() {
        // The same bytes used as a file, as pixels and as a path must not alias.
        let bytes = raw(5);
        let as_file = ContentKey::of_bytes(&bytes);
        let as_pixels = ContentKey::of_rgba8(5, 5, AlphaMode::Straight, ColorSpace::Srgb, &bytes);
        assert_ne!(as_file, as_pixels);
        // Shape and representation are part of the pixel key.
        assert_ne!(
            as_pixels,
            ContentKey::of_rgba8(25, 1, AlphaMode::Straight, ColorSpace::Srgb, &bytes)
        );
        assert_ne!(
            as_pixels,
            ContentKey::of_rgba8(5, 5, AlphaMode::Premultiplied, ColorSpace::Srgb, &bytes)
        );
        assert_ne!(
            as_pixels,
            ContentKey::of_rgba8(5, 5, AlphaMode::Straight, ColorSpace::Linear, &bytes)
        );
        assert!(format!("{as_file:?}").starts_with("ContentKey("));
    }

    #[test]
    fn a_rewritten_file_is_reloaded_rather_than_served_stale() {
        let dir = temp_dir("rewritten");
        let path = dir.join("changing.png");
        std::fs::write(&path, encode_png(4, 4, &rgba_checkerboard(4, 4))).expect("write");
        let mut cache = ImageCache::default();
        let first = cache.load_path(&path).expect("load");
        assert_eq!(cache.peek(first).map(DecodedImage::dimensions), Some((4, 4)));

        // Rewrite with different content. The path key folds in size and
        // modification time, so the cache must notice.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, encode_png(6, 6, &rgba_checkerboard(6, 6))).expect("rewrite");
        let second = cache.load_path(&path).expect("reload");
        assert_ne!(first, second, "a changed file must not be served from cache");
        assert_eq!(cache.peek(second).map(DecodedImage::dimensions), Some((6, 6)));
        assert_eq!(cache.stats().decodes, 2);
        let _ = std::fs::remove_file(&path);
    }
}
