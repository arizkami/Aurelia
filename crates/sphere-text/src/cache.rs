//! Shaping and glyph caches with bounded memory.
//!
//! # What is cached and why
//!
//! Shaping a paragraph — bidi resolution, script itemisation, per-run
//! `rustybuzz` shaping, fallback, line breaking, alignment — costs tens of
//! microseconds for a label that has not changed since the last frame. A DAW
//! mixer strip has hundreds of such labels and redraws at 60 Hz, so re-shaping
//! unchanged text is by a wide margin the largest avoidable cost in the text
//! stack. [`ShapeCache`] removes it.
//!
//! [`GlyphCache`] is the second, much smaller win: it maps a [`GlyphKey`] to the
//! [`AtlasPlacement`] the rasteriser produced, so a glyph is rasterised and
//! packed exactly once.
//!
//! # Bounded, always
//!
//! Neither cache may grow without limit — an engine that leaks a megabyte per
//! minute of scrolling is broken even if every frame is fast. Both are built on
//! [`LruCache`], which accounts bytes, evicts least-recently-used entries when
//! it goes over budget, and counts hits and misses for the diagnostics overlay.
//!
//! # Key safety
//!
//! [`ShapeCache`] hashes the text, style and wrap width into a 64-bit
//! fingerprint and uses that as the map key, because a `FxHashMap` keyed on the
//! `String` itself would have to hash the text *and* store a second copy of it,
//! and a lookup would have to allocate a key.
//!
//! A 64-bit fingerprint can collide. Rather than pray, every entry stores the
//! text, the [`TextStyle`] and the wrap width it was built from, and every
//! lookup compares them; a mismatch is reported as a miss and counted in
//! [`ShapeCache::collisions`]. So a collision costs one re-shape — never a
//! wrong layout on screen. That is the trade this module picks over a 128-bit
//! hash: exact verification is cheap here (the text has just been hashed, so it
//! is already in cache) and it is *certain* rather than merely improbable.

use core::fmt;
use core::hash::{Hash, Hasher};

use rustc_hash::FxHashMap;
use sphere_core::Px;

use crate::types::{
    AtlasPlacement, GlyphKey, ShapedGlyph, ShapedRun, TextLayout, TextLine, TextStyle,
    VariationAxis,
};

/// Sentinel for "no neighbour" in the intrusive LRU list.
const NIL: u32 = u32::MAX;

/// A value or key that can report the heap it keeps alive.
///
/// Byte budgets are meaningless if a `TextLayout` is charged 48 bytes for its
/// three `Vec` headers while holding ten thousand glyphs, so every cached type
/// has to answer this honestly. `size_of::<Self>()` is added by the cache, so
/// implementations report *only* out-of-line allocations.
pub trait CacheCost {
    /// Bytes this value owns on the heap, excluding `size_of::<Self>()`.
    fn heap_bytes(&self) -> usize;
}

/// Implements [`CacheCost`] for types that own nothing out of line.
macro_rules! inline_cost {
    ($($t:ty),* $(,)?) => {$(
        impl CacheCost for $t {
            #[inline]
            fn heap_bytes(&self) -> usize { 0 }
        }
    )*};
}
inline_cost!(u8, u16, u32, u64, u128, usize, GlyphKey, AtlasPlacement, VariationAxis);

impl CacheCost for str {
    #[inline]
    fn heap_bytes(&self) -> usize {
        self.len()
    }
}

impl CacheCost for String {
    #[inline]
    fn heap_bytes(&self) -> usize {
        self.capacity()
    }
}

impl CacheCost for Box<str> {
    #[inline]
    fn heap_bytes(&self) -> usize {
        self.len()
    }
}

impl<T: CacheCost> CacheCost for Vec<T> {
    #[inline]
    fn heap_bytes(&self) -> usize {
        // Capacity, not length: the spare tail is memory the cache is holding.
        self.capacity() * size_of::<T>() + self.iter().map(CacheCost::heap_bytes).sum::<usize>()
    }
}

impl<T: CacheCost> CacheCost for Option<T> {
    #[inline]
    fn heap_bytes(&self) -> usize {
        self.as_ref().map_or(0, CacheCost::heap_bytes)
    }
}

impl CacheCost for ShapedGlyph {
    #[inline]
    fn heap_bytes(&self) -> usize {
        0
    }
}

impl CacheCost for ShapedRun {
    #[inline]
    fn heap_bytes(&self) -> usize {
        self.glyphs.heap_bytes()
    }
}

impl CacheCost for TextLine {
    #[inline]
    fn heap_bytes(&self) -> usize {
        self.runs.heap_bytes()
    }
}

impl CacheCost for TextLayout {
    #[inline]
    fn heap_bytes(&self) -> usize {
        self.lines.heap_bytes()
    }
}

impl CacheCost for TextStyle {
    #[inline]
    fn heap_bytes(&self) -> usize {
        self.font.families.heap_bytes()
            + self.variations.heap_bytes()
            + self.features.capacity() * size_of::<([u8; 4], u32)>()
    }
}

/// Bookkeeping charged to every entry on top of its key and value.
///
/// The links, the cached cost, and the second copy of the key that lives in the
/// index map next to its slot number. Approximate — hash-table load factor is
/// not modelled — but it stops a cache of millions of tiny entries from
/// reporting near-zero bytes.
#[inline]
const fn link_bytes<K>() -> usize {
    2 * size_of::<u32>() + size_of::<usize>() + size_of::<K>() + size_of::<u32>()
}

/// Total bytes charged for one entry.
#[inline]
fn entry_cost<K: CacheCost, V: CacheCost>(key: &K, value: &V) -> usize {
    // The key's heap is counted twice because it genuinely exists twice: once in
    // the slot and once as the index map's key.
    size_of::<K>() + key.heap_bytes() * 2 + size_of::<V>() + value.heap_bytes() + link_bytes::<K>()
}

/// Cumulative counters for the diagnostics overlay.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups that found a live entry.
    pub hits: u64,
    /// Lookups that did not.
    pub misses: u64,
    /// Entries dropped because the cache went over budget.
    pub evictions: u64,
    /// Inserts refused because the value alone exceeds the whole budget.
    pub rejections: u64,
    /// Entries currently held.
    pub entries: usize,
    /// Bytes currently accounted for.
    pub bytes: usize,
    /// The byte budget.
    pub budget: usize,
}

impl CacheStats {
    /// Hits over total lookups, or `0.0` when nothing has been looked up.
    #[inline]
    #[must_use]
    pub fn hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 { 0.0 } else { self.hits as f32 / total as f32 }
    }

    /// Bytes over budget, in `0.0..=1.0`.
    #[inline]
    #[must_use]
    pub fn fill(&self) -> f32 {
        if self.budget == 0 { 0.0 } else { (self.bytes as f32 / self.budget as f32).min(1.0) }
    }
}

/// One cache entry plus its position in the recency list.
struct Node<K, V> {
    key: K,
    value: V,
    cost: usize,
    /// Towards the most recently used end, or [`NIL`].
    prev: u32,
    /// Towards the least recently used end, or [`NIL`].
    next: u32,
}

/// A byte-budgeted least-recently-used map.
///
/// The recency order is an intrusive doubly linked list over a slot vector
/// rather than a `VecDeque` of keys: promoting on every hit has to be `O(1)` and
/// must not re-hash or move the value, because the hot path is "look up a glyph
/// that is already cached" running once per glyph per frame.
///
/// Eviction happens on insert, from the least recently used end, until the
/// budget is met. A value whose own cost exceeds the entire budget is *rejected*
/// instead: evicting the whole cache to make room for something that still would
/// not fit is the worst of both outcomes.
pub struct LruCache<K, V> {
    slots: Vec<Option<Node<K, V>>>,
    free: Vec<u32>,
    index: FxHashMap<K, u32>,
    /// Most recently used.
    head: u32,
    /// Least recently used.
    tail: u32,
    bytes: usize,
    budget: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
    rejections: u64,
}

impl<K, V> fmt::Debug for LruCache<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not derived: the entries are the point of the cache, not of its Debug.
        f.debug_struct("LruCache")
            .field("entries", &self.index.len())
            .field("bytes", &self.bytes)
            .field("budget", &self.budget)
            .field("hits", &self.hits)
            .field("misses", &self.misses)
            .finish()
    }
}

impl<K: Hash + Eq + Clone + CacheCost, V: CacheCost> LruCache<K, V> {
    /// An empty cache with the given byte budget.
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            index: FxHashMap::default(),
            head: NIL,
            tail: NIL,
            bytes: 0,
            budget: budget_bytes,
            hits: 0,
            misses: 0,
            evictions: 0,
            rejections: 0,
        }
    }

    /// Entries currently held.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// True when nothing is cached.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Bytes currently accounted for.
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The byte budget.
    #[inline]
    #[must_use]
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Changes the budget, immediately evicting down to it if it shrank.
    pub fn set_budget(&mut self, budget_bytes: usize) {
        self.budget = budget_bytes;
        self.evict_to_budget();
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            rejections: self.rejections,
            entries: self.index.len(),
            bytes: self.bytes,
            budget: self.budget,
        }
    }

    /// Zeroes the hit/miss/eviction counters without touching the entries.
    ///
    /// Separate from [`LruCache::clear`] because the overlay wants per-interval
    /// rates while the cache keeps working.
    pub fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
        self.rejections = 0;
    }

    /// Looks up a key, promoting it to most recently used and counting the
    /// result.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let Some(&slot) = self.index.get(key) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        self.promote_slot(slot);
        self.slots[slot as usize].as_ref().map(|n| &n.value)
    }

    /// Like [`LruCache::get`] but yields a mutable reference.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let Some(&slot) = self.index.get(key) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        self.promote_slot(slot);
        self.slots[slot as usize].as_mut().map(|n| &mut n.value)
    }

    /// Looks up a key without promoting it or counting the lookup.
    ///
    /// The building block for caches layered on top that verify the entry before
    /// deciding whether it really was a hit; see [`ShapeCache`].
    #[must_use]
    pub fn peek(&self, key: &K) -> Option<&V> {
        let &slot = self.index.get(key)?;
        self.slots[slot as usize].as_ref().map(|n| &n.value)
    }

    /// True when the key is present, without promoting or counting.
    #[inline]
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    /// Marks a key as most recently used. Returns false if it is absent.
    pub fn promote(&mut self, key: &K) -> bool {
        let Some(&slot) = self.index.get(key) else { return false };
        self.promote_slot(slot);
        true
    }

    /// Records a hit that a layered cache resolved itself.
    #[inline]
    pub fn record_hit(&mut self) {
        self.hits += 1;
    }

    /// Records a miss that a layered cache resolved itself.
    #[inline]
    pub fn record_miss(&mut self) {
        self.misses += 1;
    }

    /// Inserts or replaces an entry, evicting as needed.
    ///
    /// Returns `false` — leaving the cache untouched — when the entry alone
    /// exceeds the budget.
    pub fn insert(&mut self, key: K, value: V) -> bool {
        let cost = entry_cost(&key, &value);
        if cost > self.budget {
            self.rejections += 1;
            return false;
        }
        match self.index.get(&key).copied() {
            Some(slot) => {
                let node = self.slots[slot as usize].as_mut().expect("indexed slot is live");
                self.bytes = self.bytes - node.cost + cost;
                node.value = value;
                node.cost = cost;
                self.promote_slot(slot);
            }
            None => {
                let node = Node { key: key.clone(), value, cost, prev: NIL, next: NIL };
                let slot = match self.free.pop() {
                    Some(s) => {
                        self.slots[s as usize] = Some(node);
                        s
                    }
                    None => {
                        self.slots.push(Some(node));
                        (self.slots.len() - 1) as u32
                    }
                };
                self.index.insert(key, slot);
                self.push_front(slot);
                self.bytes += cost;
            }
        }
        // The new entry is at the head and costs at most the whole budget, so
        // this can never evict what was just inserted.
        self.evict_to_budget();
        true
    }

    /// Removes an entry, returning its value.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.index.remove(key)?;
        self.unlink(slot);
        let node = self.slots[slot as usize].take().expect("indexed slot is live");
        self.bytes -= node.cost;
        self.free.push(slot);
        Some(node.value)
    }

    /// Removes and returns the least recently used entry.
    pub fn pop_lru(&mut self) -> Option<(K, V)> {
        let slot = self.tail;
        if slot == NIL {
            return None;
        }
        self.unlink(slot);
        let node = self.slots[slot as usize].take().expect("listed slot is live");
        self.index.remove(&node.key);
        self.bytes -= node.cost;
        self.free.push(slot);
        Some((node.key, node.value))
    }

    /// Drops every entry, keeping the cumulative counters.
    ///
    /// The counters are lifetime diagnostics; a cache invalidated because a font
    /// was reloaded has not stopped having had the hit rate it had. Use
    /// [`LruCache::reset_stats`] to clear them.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.free.clear();
        self.index.clear();
        self.head = NIL;
        self.tail = NIL;
        self.bytes = 0;
    }

    /// Iterates entries from most to least recently used.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        let mut cur = self.head;
        core::iter::from_fn(move || {
            let node = self.slots.get(cur as usize)?.as_ref()?;
            cur = node.next;
            Some((&node.key, &node.value))
        })
    }

    fn evict_to_budget(&mut self) {
        while self.bytes > self.budget && self.tail != NIL {
            if self.pop_lru().is_some() {
                self.evictions += 1;
            }
        }
    }

    fn promote_slot(&mut self, slot: u32) {
        if self.head == slot {
            return;
        }
        self.unlink(slot);
        self.push_front(slot);
    }

    fn push_front(&mut self, slot: u32) {
        let old_head = self.head;
        {
            let node = self.slots[slot as usize].as_mut().expect("slot is live");
            node.prev = NIL;
            node.next = old_head;
        }
        if old_head != NIL {
            self.slots[old_head as usize].as_mut().expect("head is live").prev = slot;
        } else {
            self.tail = slot;
        }
        self.head = slot;
    }

    fn unlink(&mut self, slot: u32) {
        let (prev, next) = {
            let node = self.slots[slot as usize].as_ref().expect("slot is live");
            (node.prev, node.next)
        };
        if prev != NIL {
            self.slots[prev as usize].as_mut().expect("prev is live").next = next;
        } else {
            self.head = next;
        }
        if next != NIL {
            self.slots[next as usize].as_mut().expect("next is live").prev = prev;
        } else {
            self.tail = prev;
        }
        let node = self.slots[slot as usize].as_mut().expect("slot is live");
        node.prev = NIL;
        node.next = NIL;
    }
}

/// Default byte budget for [`ShapeCache`]: 4 MiB.
///
/// A laid-out paragraph costs roughly 40 bytes per glyph, so this holds on the
/// order of 100 000 glyphs — far more than any one view shows, which is the
/// point: the cache should survive scrolling back and forth.
pub const DEFAULT_SHAPE_BUDGET: usize = 4 * 1024 * 1024;

/// Default byte budget for [`GlyphCache`]: 1 MiB.
pub const DEFAULT_GLYPH_BUDGET: usize = 1024 * 1024;

/// A 64-bit fingerprint of a shaping request.
///
/// Opaque on purpose: it is a hash-table key, never an identity. Identity is
/// established by comparing the stored text and style — see the module docs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShapeFingerprint(u64);

inline_cost!(ShapeFingerprint);

/// Computes the fingerprint of a shaping request.
///
/// Exposed so a caller that already has a fingerprint (from a previous frame's
/// diff, say) can skip re-hashing, and so tests can reason about collisions.
#[must_use]
pub fn shape_fingerprint(text: &str, style: &TextStyle, max_width: Option<Px>) -> ShapeFingerprint {
    // std's SipHash-1-3 rather than FxHash: FxHash is a multiply-xor mixer built
    // for short, already-well-distributed keys, and it clusters badly on long
    // ASCII strings, which is exactly what this hashes. The map itself is still
    // an FxHashMap, because by then the key is a single well-mixed u64.
    let mut h = std::hash::DefaultHasher::new();
    text.hash(&mut h);
    hash_style(style, &mut h);
    match max_width {
        // Discriminant first so `None` and `Some(0.0)` cannot collide, and raw
        // bits so a NaN wrap width still hashes deterministically.
        Some(w) => {
            1u8.hash(&mut h);
            w.get().to_bits().hash(&mut h);
        }
        None => 0u8.hash(&mut h),
    }
    ShapeFingerprint(h.finish())
}

/// Hashes a [`TextStyle`] by hand.
///
/// `TextStyle` cannot derive `Hash`: it holds `f32`-backed [`Px`] fields, and
/// `f32` has no total ordering. Hashing the raw bits gives a deterministic,
/// bit-exact key, which is the right choice for a cache — two styles that differ
/// only by `-0.0` versus `0.0` shape identically, so this is at worst one
/// redundant entry, never a wrong one.
fn hash_style<H: Hasher>(s: &TextStyle, h: &mut H) {
    s.font.families.len().hash(h);
    for f in &s.font.families {
        f.hash(h);
    }
    s.font.weight.hash(h);
    s.font.style.hash(h);
    s.font.stretch.hash(h);
    s.font_size.get().to_bits().hash(h);
    match s.line_height {
        Some(v) => {
            1u8.hash(h);
            v.get().to_bits().hash(h);
        }
        None => 0u8.hash(h),
    }
    s.letter_spacing.get().to_bits().hash(h);
    s.word_spacing.get().to_bits().hash(h);
    s.align.hash(h);
    s.direction.hash(h);
    s.wrap.hash(h);
    s.overflow.hash(h);
    s.variations.len().hash(h);
    for v in &s.variations {
        v.tag.hash(h);
        v.value.to_bits().hash(h);
    }
    s.features.len().hash(h);
    for (tag, value) in &s.features {
        tag.hash(h);
        value.hash(h);
    }
}

/// A cached layout together with everything needed to prove it is the right one.
struct ShapeEntry {
    text: Box<str>,
    style: TextStyle,
    max_width: Option<Px>,
    layout: TextLayout,
}

/// Compares two wrap widths the way the fingerprint hashes them: by raw bits.
///
/// `Px` is `f32`-backed, so `PartialEq` says a NaN wrap width does not equal
/// itself. Using it here would make an entry stored under a NaN width
/// permanently unreachable — every lookup a miss, every miss counted as a
/// fingerprint collision — while still occupying its slot. A NaN width is
/// nonsense input, but "shape it again every frame and lie in the diagnostics"
/// is the wrong way to handle nonsense; bitwise equality matches what
/// [`shape_fingerprint`] already does and keeps the two halves of the key
/// consistent.
#[inline]
fn width_eq(a: Option<Px>, b: Option<Px>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.get().to_bits() == y.get().to_bits(),
        _ => false,
    }
}

impl ShapeEntry {
    #[inline]
    fn matches(&self, text: &str, style: &TextStyle, max_width: Option<Px>) -> bool {
        // Cheapest discriminators first; the text compare is last because it is
        // the only one that is O(n).
        width_eq(self.max_width, max_width) && &*self.text == text && &self.style == style
    }
}

impl CacheCost for ShapeEntry {
    fn heap_bytes(&self) -> usize {
        self.text.heap_bytes() + self.style.heap_bytes() + self.layout.heap_bytes()
    }
}

/// A bounded cache of laid-out paragraphs, keyed on text, style and wrap width.
///
/// # Why the wrap width is part of the key
///
/// Line breaking depends on it, and line breaking is not separable from shaping:
/// the breaker needs shaped advances, and justification and ellipsis change the
/// glyph run afterwards. Caching "shaped runs" independently of the width would
/// therefore only cache the cheap half. The entry is the whole [`TextLayout`],
/// and its [`ShapedRun`]s are reachable through it.
pub struct ShapeCache {
    inner: LruCache<ShapeFingerprint, ShapeEntry>,
    collisions: u64,
}

impl Default for ShapeCache {
    fn default() -> Self {
        Self::new(DEFAULT_SHAPE_BUDGET)
    }
}

impl fmt::Debug for ShapeCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShapeCache")
            .field("inner", &self.inner)
            .field("collisions", &self.collisions)
            .finish()
    }
}

impl ShapeCache {
    /// An empty cache with the given byte budget.
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self { inner: LruCache::new(budget_bytes), collisions: 0 }
    }

    /// Fingerprint collisions observed: lookups that found an entry whose stored
    /// text or style did not match.
    ///
    /// Expected to stay at zero forever. A nonzero value is harmless — it costs
    /// one re-shape — but is worth surfacing, because a *growing* count would
    /// mean two hot strings are thrashing one slot.
    #[inline]
    #[must_use]
    pub fn collisions(&self) -> u64 {
        self.collisions
    }

    /// The byte accounting and hit counters.
    #[inline]
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        self.inner.stats()
    }

    /// Entries currently held.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when nothing is cached.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Bytes currently accounted for.
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.inner.bytes()
    }

    /// The byte budget.
    #[inline]
    #[must_use]
    pub fn budget(&self) -> usize {
        self.inner.budget()
    }

    /// Changes the byte budget, evicting down to it if it shrank.
    pub fn set_budget(&mut self, budget_bytes: usize) {
        self.inner.set_budget(budget_bytes);
    }

    /// Drops every cached layout. Counters survive; see [`LruCache::clear`].
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Zeroes the diagnostics counters.
    pub fn reset_stats(&mut self) {
        self.inner.reset_stats();
        self.collisions = 0;
    }

    /// Returns the cached layout for exactly this request, if there is one.
    ///
    /// Promotes the entry and updates the hit/miss counters. A stored entry
    /// whose text or style does not match is a fingerprint collision: it counts
    /// as a miss, not as a hit with the wrong data.
    pub fn get(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: Option<Px>,
    ) -> Option<&TextLayout> {
        let fp = shape_fingerprint(text, style, max_width);
        match self.inner.peek(&fp) {
            Some(e) if e.matches(text, style, max_width) => {
                self.inner.record_hit();
                self.inner.promote(&fp);
                self.inner.peek(&fp).map(|e| &e.layout)
            }
            Some(_) => {
                self.collisions += 1;
                self.inner.record_miss();
                None
            }
            None => {
                self.inner.record_miss();
                None
            }
        }
    }

    /// Stores a layout, taking ownership of the verification copies of the
    /// request.
    ///
    /// Returns `false` if the layout alone exceeds the whole budget, in which
    /// case nothing is cached and nothing is evicted — a single enormous
    /// paragraph must not flush every label on screen.
    pub fn insert(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: Option<Px>,
        layout: TextLayout,
    ) -> bool {
        let fp = shape_fingerprint(text, style, max_width);
        let entry = ShapeEntry { text: text.into(), style: style.clone(), max_width, layout };
        self.inner.insert(fp, entry)
    }

    /// Drops one request's layout, if it is cached.
    pub fn remove(&mut self, text: &str, style: &TextStyle, max_width: Option<Px>) -> bool {
        self.inner.remove(&shape_fingerprint(text, style, max_width)).is_some()
    }
}

/// A bounded [`GlyphKey`] to [`AtlasPlacement`] cache.
///
/// Sits in front of "rasterise, then pack into the atlas", so a glyph goes
/// through neither step twice.
///
/// # Atlas generations
///
/// Placements are only valid while the atlas has not moved anything. The atlas
/// bumps its generation on clear, compaction and every page eviction; call
/// [`GlyphCache::sync_atlas`] once per frame with
/// [`crate::atlas::GlyphAtlas::generation`] so a changed generation drops the
/// cached placements instead of drawing texels that now belong to some other
/// glyph.
pub struct GlyphCache {
    inner: LruCache<GlyphKey, AtlasPlacement>,
    atlas_generation: u64,
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self::new(DEFAULT_GLYPH_BUDGET)
    }
}

impl fmt::Debug for GlyphCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlyphCache")
            .field("inner", &self.inner)
            .field("atlas_generation", &self.atlas_generation)
            .finish()
    }
}

impl GlyphCache {
    /// An empty cache with the given byte budget.
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self { inner: LruCache::new(budget_bytes), atlas_generation: 0 }
    }

    /// Looks up a glyph, promoting it and counting the result.
    pub fn get(&mut self, key: &GlyphKey) -> Option<AtlasPlacement> {
        self.inner.get(key).copied()
    }

    /// Records where a glyph landed. Returns `false` only if the budget is so
    /// small that a single placement does not fit.
    pub fn insert(&mut self, key: GlyphKey, placement: AtlasPlacement) -> bool {
        self.inner.insert(key, placement)
    }

    /// Looks a glyph up without promoting it or counting the lookup.
    #[inline]
    #[must_use]
    pub fn peek(&self, key: &GlyphKey) -> Option<AtlasPlacement> {
        self.inner.peek(key).copied()
    }

    /// True when the glyph is cached, without promoting or counting.
    #[inline]
    #[must_use]
    pub fn contains(&self, key: &GlyphKey) -> bool {
        self.inner.contains(key)
    }

    /// Drops one glyph.
    pub fn remove(&mut self, key: &GlyphKey) -> bool {
        self.inner.remove(key).is_some()
    }

    /// Reconciles with the atlas, clearing the cache if the atlas moved anything.
    ///
    /// Returns `true` when the cache was dropped, which is the signal a caller
    /// needs to re-place glyphs this frame.
    pub fn sync_atlas(&mut self, atlas_generation: u64) -> bool {
        if atlas_generation == self.atlas_generation {
            return false;
        }
        self.atlas_generation = atlas_generation;
        self.inner.clear();
        true
    }

    /// The atlas generation this cache is reconciled with.
    #[inline]
    #[must_use]
    pub fn atlas_generation(&self) -> u64 {
        self.atlas_generation
    }

    /// The byte accounting and hit counters.
    #[inline]
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        self.inner.stats()
    }

    /// Entries currently held.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when nothing is cached.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Bytes currently accounted for.
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.inner.bytes()
    }

    /// The byte budget.
    #[inline]
    #[must_use]
    pub fn budget(&self) -> usize {
        self.inner.budget()
    }

    /// Changes the byte budget, evicting down to it if it shrank.
    pub fn set_budget(&mut self, budget_bytes: usize) {
        self.inner.set_budget(budget_bytes);
    }

    /// Drops every cached placement. Counters survive; see [`LruCache::clear`].
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Zeroes the diagnostics counters.
    pub fn reset_stats(&mut self) {
        self.inner.reset_stats();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_core::{FontId, GlyphId, Point, Size, px, rect};

    use crate::types::{FontRequest, FontWeight};

    fn gkey(n: u16) -> GlyphKey {
        GlyphKey::mtsdf(FontId::new(0, 1), GlyphId(n), 0)
    }

    fn placement(page: u32) -> AtlasPlacement {
        AtlasPlacement {
            page,
            uv: [0.0, 0.0, 0.1, 0.1],
            texels: rect(0, 0, 8, 8),
            bounds_em: rect(0.0, -0.7, 0.5, 0.7),
            range_em: 0.05,
            format: crate::types::GlyphFormat::Mtsdf,
        }
    }

    /// A layout with `glyphs` glyphs on one line, so its cost is predictable.
    fn layout(glyphs: usize) -> TextLayout {
        let run = ShapedRun {
            font: FontId::new(0, 1),
            font_size: px(12.0),
            rtl: false,
            glyphs: (0..glyphs)
                .map(|i| ShapedGlyph {
                    glyph: GlyphId(i as u16),
                    font: FontId::new(0, 1),
                    position: Point::new(px(i as f32 * 7.0), Px::ZERO),
                    advance: px(7.0),
                    cluster: i..i + 1,
                    rtl: false,
                })
                .collect(),
            source: 0..glyphs,
        };
        TextLayout {
            lines: vec![TextLine {
                runs: vec![run],
                baseline: px(10.0),
                height: px(14.0),
                width: px(glyphs as f32 * 7.0),
                source: 0..glyphs,
            }],
            size: Size::new(px(glyphs as f32 * 7.0), px(14.0)),
            max_width: None,
        }
    }

    fn style() -> TextStyle {
        TextStyle { font: FontRequest::family("Inter"), font_size: px(13.0), ..Default::default() }
    }

    /// Byte cost of one `(u32, Vec<u8>)` entry holding `n` payload bytes.
    fn unit_cost(n: usize) -> usize {
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(usize::MAX);
        c.insert(0, vec![0u8; n]);
        c.bytes()
    }

    #[test]
    fn eviction_drops_the_least_recently_used_not_the_newest() {
        let unit = unit_cost(64);
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(unit * 2);
        assert!(c.insert(1, vec![1u8; 64]));
        assert!(c.insert(2, vec![2u8; 64]));
        assert_eq!(c.len(), 2);
        assert!(c.insert(3, vec![3u8; 64]));
        assert_eq!(c.len(), 2, "the budget must hold exactly two");
        assert!(!c.contains(&1), "key 1 was least recently used and must be gone");
        assert!(c.contains(&2));
        assert!(c.contains(&3), "the entry just inserted must never be the one evicted");
        assert_eq!(c.stats().evictions, 1);
        assert!(c.bytes() <= c.budget());
    }

    #[test]
    fn get_promotes_so_the_other_entry_is_evicted_instead() {
        let unit = unit_cost(64);
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(unit * 2);
        c.insert(1, vec![1u8; 64]);
        c.insert(2, vec![2u8; 64]);
        // Without this, key 1 would be the eviction victim.
        assert_eq!(c.get(&1).map(|v| v[0]), Some(1));
        c.insert(3, vec![3u8; 64]);
        assert!(c.contains(&1), "promotion must protect key 1");
        assert!(!c.contains(&2), "key 2 is now the least recently used");
    }

    #[test]
    fn hit_and_miss_counters_are_accurate() {
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(1 << 20);
        c.insert(1, vec![0u8; 8]);
        assert_eq!(
            c.stats(),
            CacheStats { entries: 1, bytes: c.bytes(), budget: 1 << 20, ..Default::default() }
        );
        assert!(c.get(&1).is_some());
        assert!(c.get(&1).is_some());
        assert!(c.get(&99).is_none());
        let s = c.stats();
        assert_eq!((s.hits, s.misses), (2, 1));
        assert!((s.hit_rate() - 2.0 / 3.0).abs() < 1e-6);
        // peek and contains must not move the numbers.
        assert!(c.peek(&1).is_some());
        assert!(c.contains(&1));
        assert!(c.peek(&99).is_none());
        assert_eq!(c.stats().hits, 2);
        assert_eq!(c.stats().misses, 1);
        c.reset_stats();
        assert_eq!(c.stats().hit_rate(), 0.0);
        assert_eq!(c.len(), 1, "resetting the counters must not drop entries");
    }

    #[test]
    fn a_value_larger_than_the_budget_is_rejected_not_ruinous() {
        let unit = unit_cost(64);
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(unit * 2);
        c.insert(1, vec![1u8; 64]);
        c.insert(2, vec![2u8; 64]);
        let before = c.bytes();

        assert!(!c.insert(9, vec![0u8; unit * 4]), "an oversized value must be refused");
        assert_eq!(c.len(), 2, "and must not have flushed the cache to try");
        assert_eq!(c.bytes(), before);
        assert!(c.contains(&1) && c.contains(&2));
        assert_eq!(c.stats().rejections, 1);
        assert_eq!(c.stats().evictions, 0);
    }

    #[test]
    fn replacing_a_key_re_costs_it_rather_than_double_counting() {
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(1 << 20);
        c.insert(1, vec![0u8; 16]);
        let small = c.bytes();
        c.insert(1, vec![0u8; 1024]);
        assert_eq!(c.len(), 1);
        assert!(c.bytes() > small);
        c.insert(1, vec![0u8; 16]);
        assert_eq!(c.bytes(), small, "shrinking a value must give the bytes back");
    }

    #[test]
    fn shrinking_the_budget_evicts_immediately() {
        let unit = unit_cost(32);
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(unit * 4);
        for i in 0..4u32 {
            c.insert(i, vec![0u8; 32]);
        }
        assert_eq!(c.len(), 4);
        c.set_budget(unit * 2);
        assert_eq!(c.len(), 2);
        assert!(c.contains(&2) && c.contains(&3), "the two most recent survive");
        assert!(c.bytes() <= c.budget());
    }

    #[test]
    fn clear_empties_the_cache_but_keeps_lifetime_counters() {
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(1 << 20);
        c.insert(1, vec![0u8; 8]);
        c.get(&1);
        c.get(&2);
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.bytes(), 0);
        assert_eq!(c.stats().hits, 1);
        assert_eq!(c.stats().misses, 1);
        // ...and the cache still works afterwards.
        assert!(c.insert(5, vec![0u8; 8]));
        assert!(c.get(&5).is_some());
    }

    #[test]
    fn slots_are_recycled_so_churn_does_not_grow_the_backing_store() {
        let unit = unit_cost(16);
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(unit * 3);
        for i in 0..1000u32 {
            c.insert(i, vec![0u8; 16]);
        }
        assert_eq!(c.len(), 3);
        assert!(c.slots.len() <= 4, "slot vector grew to {}", c.slots.len());
        // The three survivors are the three most recent, in order.
        let keys: Vec<u32> = c.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![999, 998, 997]);
    }

    #[test]
    fn pop_lru_walks_the_list_in_order_and_then_returns_none() {
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(1 << 20);
        for i in 0..3u32 {
            c.insert(i, vec![0u8; 8]);
        }
        c.get(&0); // 0 becomes most recent, so the order is 0, 2, 1.
        assert_eq!(c.pop_lru().map(|(k, _)| k), Some(1));
        assert_eq!(c.pop_lru().map(|(k, _)| k), Some(2));
        assert_eq!(c.pop_lru().map(|(k, _)| k), Some(0));
        assert_eq!(c.pop_lru().map(|(k, _)| k), None);
        assert_eq!(c.bytes(), 0);
        assert!(c.is_empty());
    }

    #[test]
    fn removing_the_only_entry_leaves_a_consistent_list() {
        // Head and tail both point at the removed slot; a sloppy unlink leaves a
        // dangling head and the next insert panics or loses entries.
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(1 << 20);
        c.insert(1, vec![0u8; 8]);
        assert!(c.remove(&1).is_some());
        assert!(c.is_empty());
        assert_eq!(c.bytes(), 0);
        assert!(c.remove(&1).is_none());
        c.insert(2, vec![0u8; 8]);
        c.insert(3, vec![0u8; 8]);
        assert_eq!(c.iter().count(), 2);
        assert!(c.remove(&3).is_some(), "removing the head");
        assert!(c.remove(&2).is_some(), "removing the tail");
        assert!(c.is_empty());
    }

    #[test]
    fn a_zero_budget_cache_accepts_nothing_and_does_not_spin() {
        let mut c: LruCache<u32, Vec<u8>> = LruCache::new(0);
        assert!(!c.insert(1, vec![0u8; 8]));
        assert!(c.is_empty());
        assert_eq!(c.stats().rejections, 1);
        assert_eq!(c.stats().evictions, 0);
    }

    #[test]
    fn cost_accounting_reflects_the_payload_not_just_the_header() {
        let big = layout(4096);
        let small = layout(1);
        assert!(
            big.heap_bytes() > small.heap_bytes() + 4000 * size_of::<ShapedGlyph>() - 64,
            "a layout must be charged for its glyphs, not for three Vec headers"
        );
        // Empty containers still cost nothing on the heap.
        assert_eq!(TextLayout::default().heap_bytes(), 0);
        assert_eq!(Vec::<u8>::new().heap_bytes(), 0);
    }

    #[test]
    fn shape_cache_round_trips_a_layout() {
        let mut c = ShapeCache::new(1 << 20);
        let st = style();
        assert!(c.get("Gain", &st, None).is_none());
        assert!(c.insert("Gain", &st, None, layout(4)));
        let got = c.get("Gain", &st, None).expect("cached");
        assert_eq!(got.glyph_count(), 4);
        let s = c.stats();
        assert_eq!((s.hits, s.misses), (1, 1));
        assert_eq!(c.collisions(), 0);
    }

    #[test]
    fn shape_cache_separates_text_style_and_wrap_width() {
        let mut c = ShapeCache::new(1 << 20);
        let a = style();
        let b = TextStyle { font_size: px(13.5), ..style() };
        let bold =
            TextStyle { font: FontRequest::family("Inter").weight(FontWeight::BOLD), ..style() };
        c.insert("Gain", &a, None, layout(4));

        assert!(c.get("Gaim", &a, None).is_none(), "a different string is a different key");
        assert!(c.get("Gain", &b, None).is_none(), "half a pixel of size is a different key");
        assert!(c.get("Gain", &bold, None).is_none(), "weight is part of the key");
        assert!(c.get("Gain", &a, Some(px(100.0))).is_none(), "wrap width is part of the key");
        assert!(c.get("Gain", &a, None).is_some(), "...and the original still hits");

        // A wrapped layout is a distinct entry, not a replacement.
        c.insert("Gain", &a, Some(px(100.0)), layout(2));
        assert_eq!(c.len(), 2);
        assert_eq!(c.get("Gain", &a, None).map(TextLayout::glyph_count), Some(4));
        assert_eq!(c.get("Gain", &a, Some(px(100.0))).map(TextLayout::glyph_count), Some(2));
        // Different wrap widths are also distinct from each other.
        assert!(c.get("Gain", &a, Some(px(100.5))).is_none());
    }

    #[test]
    fn shape_cache_verifies_the_entry_so_a_collision_cannot_return_a_wrong_layout() {
        // Forge the collision the fingerprint is too wide to produce naturally:
        // file "Threshold"'s layout under "Ratio"'s fingerprint.
        let mut c = ShapeCache::new(1 << 20);
        let st = style();
        let forged = shape_fingerprint("Ratio", &st, None);
        c.inner.insert(
            forged,
            ShapeEntry {
                text: "Threshold".into(),
                style: st.clone(),
                max_width: None,
                layout: layout(9),
            },
        );
        assert_eq!(c.len(), 1);

        assert!(c.get("Ratio", &st, None).is_none(), "must not hand back the wrong paragraph");
        assert_eq!(c.collisions(), 1);
        assert_eq!(c.stats().misses, 1, "a collision is a miss, not a hit");
        assert_eq!(c.stats().hits, 0);

        // Re-inserting under the same fingerprint replaces the stale entry, so a
        // collision costs one re-shape and then stops costing anything.
        assert!(c.insert("Ratio", &st, None, layout(5)));
        assert_eq!(c.len(), 1);
        assert_eq!(c.get("Ratio", &st, None).map(TextLayout::glyph_count), Some(5));
    }

    #[test]
    fn shape_cache_evicts_under_pressure_instead_of_growing() {
        let mut c = ShapeCache::new(64 * 1024);
        let st = style();
        for i in 0..500 {
            let text = format!("channel strip label number {i}");
            assert!(c.insert(&text, &st, None, layout(64)));
            assert!(c.bytes() <= c.budget(), "budget blown at {i}: {} bytes", c.bytes());
        }
        assert!(c.len() < 500, "an unbounded cache is a leak");
        assert!(c.stats().evictions > 0);
        // The most recent insert is still there; the first is long gone.
        assert!(c.get("channel strip label number 499", &st, None).is_some());
        assert!(c.get("channel strip label number 0", &st, None).is_none());
    }

    #[test]
    fn an_enormous_paragraph_is_refused_without_flushing_the_labels() {
        let mut c = ShapeCache::new(8 * 1024);
        let st = style();
        c.insert("Gain", &st, None, layout(4));
        c.insert("Ratio", &st, None, layout(4));
        let before = c.len();
        assert!(!c.insert("novel", &st, None, layout(100_000)));
        assert_eq!(c.len(), before, "the labels must survive");
        assert_eq!(c.stats().rejections, 1);
        assert!(c.get("Gain", &st, None).is_some());
    }

    #[test]
    fn shape_cache_remove_and_clear() {
        let mut c = ShapeCache::new(1 << 20);
        let st = style();
        c.insert("Gain", &st, None, layout(4));
        assert!(c.remove("Gain", &st, None));
        assert!(!c.remove("Gain", &st, None));
        c.insert("Gain", &st, None, layout(4));
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.bytes(), 0);
    }

    #[test]
    fn glyph_cache_hits_misses_and_eviction() {
        let mut c = GlyphCache::new(1 << 16);
        assert!(c.get(&gkey(1)).is_none());
        assert!(c.insert(gkey(1), placement(0)));
        assert_eq!(c.get(&gkey(1)), Some(placement(0)));
        assert_eq!(c.peek(&gkey(1)), Some(placement(0)));
        assert!(c.contains(&gkey(1)));
        let s = c.stats();
        assert_eq!((s.hits, s.misses, s.entries), (1, 1, 1));

        // Fill well past the budget and check it never exceeds it.
        for i in 0..10_000u16 {
            c.insert(gkey(i), placement(u32::from(i) % 4));
            assert!(c.bytes() <= c.budget());
        }
        assert!(c.len() < 10_000);
        assert!(c.stats().evictions > 0);
        assert!(c.remove(&gkey(9999)));
        assert!(!c.remove(&gkey(9999)));
    }

    #[test]
    fn glyph_cache_drops_everything_when_the_atlas_moves() {
        let mut c = GlyphCache::new(1 << 16);
        c.insert(gkey(1), placement(0));
        assert!(!c.sync_atlas(0), "the same generation is not a change");
        assert!(c.contains(&gkey(1)));

        // The atlas compacted or evicted a page: every placement is now stale.
        assert!(c.sync_atlas(1));
        assert!(!c.contains(&gkey(1)), "stale placements must not survive");
        assert_eq!(c.atlas_generation(), 1);
        assert_eq!(c.bytes(), 0);
        assert!(!c.sync_atlas(1));
    }

    #[test]
    fn a_glyph_cache_budget_of_one_entry_still_works() {
        let mut c = GlyphCache::new(0);
        assert!(!c.insert(gkey(1), placement(0)), "nothing fits in a zero budget");
        c.set_budget(entry_cost(&gkey(1), &placement(0)));
        assert!(c.insert(gkey(1), placement(0)));
        assert!(c.insert(gkey(2), placement(0)));
        assert_eq!(c.len(), 1);
        assert!(c.contains(&gkey(2)));
    }

    #[test]
    fn fill_and_hit_rate_report_sensibly_when_empty() {
        let c: LruCache<u32, Vec<u8>> = LruCache::new(0);
        let s = c.stats();
        assert_eq!(s.hit_rate(), 0.0);
        assert_eq!(s.fill(), 0.0, "a zero budget must not divide by zero");
        let d = GlyphCache::default();
        assert_eq!(d.stats().budget, DEFAULT_GLYPH_BUDGET);
        assert_eq!(ShapeCache::default().stats().budget, DEFAULT_SHAPE_BUDGET);
        assert!(ShapeCache::default().is_empty());
    }

    #[test]
    fn style_hashing_covers_every_field_that_changes_the_output() {
        // A field left out of hash_style silently returns a stale layout when
        // only that field changes, which is the nastiest bug this module can
        // have: it looks like the style was ignored.
        let base = TextStyle::default();
        let fp = |s: &TextStyle| shape_fingerprint("x", s, None);
        let variants: Vec<TextStyle> = vec![
            TextStyle { font: FontRequest::family("A"), ..base.clone() },
            TextStyle { font: FontRequest::family("B"), ..base.clone() },
            TextStyle { font: FontRequest::family("A").weight(FontWeight::BOLD), ..base.clone() },
            TextStyle {
                font: FontRequest::family("A").style(crate::types::FontStyle::Italic),
                ..base.clone()
            },
            TextStyle { font_size: px(12.0), ..base.clone() },
            TextStyle { font_size: px(12.5), ..base.clone() },
            TextStyle { line_height: Some(px(20.0)), ..base.clone() },
            TextStyle { letter_spacing: px(1.0), ..base.clone() },
            TextStyle { word_spacing: px(1.0), ..base.clone() },
            TextStyle { align: crate::types::TextAlign::Center, ..base.clone() },
            TextStyle { direction: crate::types::TextDirection::Rtl, ..base.clone() },
            TextStyle { wrap: crate::types::WrapMode::None, ..base.clone() },
            TextStyle { overflow: crate::types::Overflow::Ellipsis, ..base.clone() },
            TextStyle {
                variations: vec![VariationAxis { tag: *b"wght", value: 700.0 }],
                ..base.clone()
            },
            TextStyle {
                variations: vec![VariationAxis { tag: *b"wght", value: 400.0 }],
                ..base.clone()
            },
            TextStyle { features: vec![(*b"liga", 0)], ..base.clone() },
            TextStyle { features: vec![(*b"liga", 1)], ..base.clone() },
            base.clone(),
        ];
        let mut seen: Vec<ShapeFingerprint> = Vec::new();
        for v in &variants {
            let f = fp(v);
            assert!(!seen.contains(&f), "two distinct styles share a fingerprint: {v:?}");
            seen.push(f);
        }
        // ...and the same style always hashes the same.
        assert_eq!(fp(&base), fp(&base.clone()));
    }

    #[test]
    fn a_multi_family_fallback_list_is_not_confused_by_concatenation() {
        // ["ab", "c"] and ["a", "bc"] must not hash alike: the fallback order
        // decides which face a missing glyph comes from.
        let st = |f: &[&str]| TextStyle {
            font: FontRequest {
                families: f.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_ne!(
            shape_fingerprint("x", &st(&["ab", "c"]), None),
            shape_fingerprint("x", &st(&["a", "bc"]), None)
        );
        assert_ne!(
            shape_fingerprint("x", &st(&["a"]), None),
            shape_fingerprint("x", &st(&["a", "a"]), None)
        );
    }

    #[test]
    fn a_nan_wrap_width_is_still_a_usable_key() {
        // `Px` is f32-backed and NaN != NaN, so a PartialEq verification would
        // make this entry unreachable forever: a re-shape every frame, reported
        // as a fingerprint collision that never happened.
        let mut c = ShapeCache::new(1 << 20);
        let st = style();
        let nan = px(f32::NAN);
        assert!(c.insert("x", &st, Some(nan), layout(3)));
        assert_eq!(c.get("x", &st, Some(nan)).map(TextLayout::glyph_count), Some(3));
        assert_eq!(c.collisions(), 0, "an exact re-lookup is not a collision");
        assert_eq!(c.stats().hits, 1);
        // ...and it is still distinct from a finite width and from None.
        assert!(c.get("x", &st, Some(px(100.0))).is_none());
        assert!(c.get("x", &st, None).is_none());
        // Positive and negative zero shape identically and must not collide
        // with NaN either way.
        assert!(c.insert("x", &st, Some(px(0.0)), layout(1)));
        assert_eq!(c.get("x", &st, Some(nan)).map(TextLayout::glyph_count), Some(3));
        assert_eq!(c.get("x", &st, Some(px(0.0))).map(TextLayout::glyph_count), Some(1));
    }

    #[test]
    fn none_and_zero_wrap_width_are_different_keys() {
        let st = style();
        assert_ne!(shape_fingerprint("x", &st, None), shape_fingerprint("x", &st, Some(Px::ZERO)));
    }

    #[test]
    fn empty_text_is_cacheable() {
        let mut c = ShapeCache::new(1 << 20);
        let st = style();
        assert!(c.insert("", &st, None, TextLayout::default()));
        assert!(c.get("", &st, None).is_some_and(TextLayout::is_empty));
        assert!(c.get("x", &st, None).is_none());
    }
}
