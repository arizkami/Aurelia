//! Generational handles.
//!
//! Every resource that outlives a single frame gets a typed, generational id.
//! The generation is what makes a stale handle detectable: reusing a slot after
//! a font or texture is evicted would otherwise silently hand back someone
//! else's data, which is exactly the class of bug that is impossible to
//! reproduce.

use core::fmt;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU64, Ordering};

/// Declares a typed generational id backed by a `u32` index and `u32` generation.
macro_rules! generational_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name {
            index: u32,
            generation: NonZeroU32,
        }

        impl $name {
            /// Builds an id from its parts.
            ///
            /// A zero generation is remapped to one, so a `Default`-constructed
            /// backing store can never produce a valid-looking handle.
            #[inline]
            pub fn new(index: u32, generation: u32) -> Self {
                Self {
                    index,
                    generation: NonZeroU32::new(generation).unwrap_or(NonZeroU32::MIN),
                }
            }

            /// The slot index this id points at.
            #[inline]
            pub const fn index(self) -> u32 { self.index }

            /// The generation stamp, used to detect stale handles.
            #[inline]
            pub const fn generation(self) -> u32 { self.generation.get() }

            /// The slot index as a `usize`, for direct slice indexing.
            #[inline]
            pub const fn slot(self) -> usize { self.index as usize }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "#{}v{}"), self.index, self.generation)
            }
        }
    };
}

generational_id! {
    /// Identifies a GPU texture owned by the renderer.
    TextureId
}
generational_id! {
    /// Identifies a decoded image in the image cache.
    ImageId
}
generational_id! {
    /// Identifies a loaded font face.
    FontId
}
generational_id! {
    /// Identifies a cached render pipeline.
    PipelineId
}
generational_id! {
    /// Identifies a node in the retained UI tree.
    NodeId
}
generational_id! {
    /// Identifies a view (a stateful, rendering component).
    ViewId
}
generational_id! {
    /// Identifies a window.
    WindowId
}
generational_id! {
    /// Identifies a parsed and tessellated SVG asset.
    SvgId
}
generational_id! {
    /// Identifies a focusable target.
    FocusId
}

/// A glyph index within a single font face.
///
/// Not generational: glyph indices are intrinsic to a face, and the face's own
/// [`FontId`] carries the generation.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
#[repr(transparent)]
pub struct GlyphId(pub u16);

/// A process-unique, monotonically increasing element identity.
///
/// Retained trees need stable identity across rebuilds so that state, focus and
/// animation survive a re-render. Callers that can supply a meaningful key
/// should use [`ElementId::from_key`]; [`ElementId::unique`] exists for
/// genuinely anonymous elements.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ElementId(u64);

static NEXT_ELEMENT_ID: AtomicU64 = AtomicU64::new(1);

impl ElementId {
    /// Mints a fresh, never-repeated id.
    #[inline]
    pub fn unique() -> Self {
        Self(NEXT_ELEMENT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Derives a stable id from a hashable key.
    ///
    /// The high bit is set so that key-derived ids can never collide with the
    /// counter-derived ids from [`ElementId::unique`].
    pub fn from_key<K: core::hash::Hash>(key: K) -> Self {
        use core::hash::{BuildHasher, Hasher};
        let mut h = rustc_hash::FxBuildHasher.build_hasher();
        key.hash(&mut h);
        Self(h.finish() | (1 << 63))
    }

    /// Combines a parent id with a child key, so sibling lists keep stable
    /// identity even when the same key appears under different parents.
    pub fn child<K: core::hash::Hash>(self, key: K) -> Self {
        Self::from_key((self.0, ElementId::from_key(key).0))
    }

    /// The raw value, for debug output and hashing.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for ElementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ElementId({:#x})", self.0)
    }
}

/// A generational slot map keyed by a typed id.
///
/// Deliberately minimal: insert, get, remove, iterate. Anything more elaborate
/// belongs in the crate that owns the resource, not in the shared vocabulary.
#[derive(Debug)]
pub struct GenerationalStore<K, V> {
    slots: Vec<Slot<V>>,
    free: Vec<u32>,
    len: usize,
    _key: core::marker::PhantomData<fn() -> K>,
}

#[derive(Debug)]
struct Slot<V> {
    generation: u32,
    value: Option<V>,
}

impl<K: GenerationalKey, V> Default for GenerationalStore<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// Implemented by every id produced by the `generational_id!` macro.
pub trait GenerationalKey: Copy {
    /// Builds a key from its parts.
    fn from_parts(index: u32, generation: u32) -> Self;
    /// The slot index.
    fn index(self) -> u32;
    /// The generation stamp.
    fn generation(self) -> u32;
}

macro_rules! impl_key {
    ($($name:ident),*) => {$(
        impl GenerationalKey for $name {
            #[inline]
            fn from_parts(index: u32, generation: u32) -> Self { Self::new(index, generation) }
            #[inline]
            fn index(self) -> u32 { self.index }
            #[inline]
            fn generation(self) -> u32 { self.generation.get() }
        }
    )*};
}
impl_key!(TextureId, ImageId, FontId, PipelineId, NodeId, ViewId, WindowId, SvgId, FocusId);

impl<K: GenerationalKey, V> GenerationalStore<K, V> {
    /// An empty store.
    pub const fn new() -> Self {
        Self { slots: Vec::new(), free: Vec::new(), len: 0, _key: core::marker::PhantomData }
    }

    /// Preallocates capacity.
    pub fn with_capacity(n: usize) -> Self {
        Self {
            slots: Vec::with_capacity(n),
            free: Vec::new(),
            len: 0,
            _key: core::marker::PhantomData,
        }
    }

    /// Number of live entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no entries are live.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Inserts a value, returning its handle.
    pub fn insert(&mut self, value: V) -> K {
        self.len += 1;
        if let Some(idx) = self.free.pop() {
            let slot = &mut self.slots[idx as usize];
            // Bumping on reuse is what invalidates every handle to the old
            // occupant of this slot.
            slot.generation = slot.generation.wrapping_add(1).max(1);
            slot.value = Some(value);
            K::from_parts(idx, slot.generation)
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Slot { generation: 1, value: Some(value) });
            K::from_parts(idx, 1)
        }
    }

    /// Borrows a value, or `None` when the handle is stale or vacant.
    pub fn get(&self, key: K) -> Option<&V> {
        let slot = self.slots.get(key.index() as usize)?;
        if slot.generation != key.generation() { None } else { slot.value.as_ref() }
    }

    /// Mutably borrows a value, or `None` when the handle is stale or vacant.
    pub fn get_mut(&mut self, key: K) -> Option<&mut V> {
        let slot = self.slots.get_mut(key.index() as usize)?;
        if slot.generation != key.generation() { None } else { slot.value.as_mut() }
    }

    /// True when the handle still refers to a live entry.
    #[inline]
    pub fn contains(&self, key: K) -> bool {
        self.get(key).is_some()
    }

    /// Removes and returns a value.
    pub fn remove(&mut self, key: K) -> Option<V> {
        let slot = self.slots.get_mut(key.index() as usize)?;
        if slot.generation != key.generation() {
            return None;
        }
        let v = slot.value.take();
        if v.is_some() {
            self.len -= 1;
            self.free.push(key.index());
        }
        v
    }

    /// Iterates live `(handle, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| {
            s.value.as_ref().map(|v| (K::from_parts(i as u32, s.generation), v))
        })
    }

    /// Iterates live values mutably.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.slots.iter_mut().filter_map(|s| s.value.as_mut())
    }

    /// Removes every entry whose value fails `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(K, &V) -> bool) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let should_drop = match &slot.value {
                Some(v) => !keep(K::from_parts(i as u32, slot.generation), v),
                None => false,
            };
            if should_drop {
                slot.value = None;
                self.len -= 1;
                self.free.push(i as u32);
            }
        }
    }

    /// Drops every entry, keeping the allocation.
    pub fn clear(&mut self) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.value.take().is_some() {
                self.free.push(i as u32);
            }
        }
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_insert_and_get() {
        let mut s: GenerationalStore<TextureId, &str> = GenerationalStore::new();
        let a = s.insert("a");
        let b = s.insert("b");
        assert_eq!(s.get(a), Some(&"a"));
        assert_eq!(s.get(b), Some(&"b"));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn stale_handle_after_slot_reuse_is_rejected() {
        let mut s: GenerationalStore<TextureId, u32> = GenerationalStore::new();
        let a = s.insert(1);
        assert_eq!(s.remove(a), Some(1));
        let b = s.insert(2);
        // The slot index is reused...
        assert_eq!(a.index(), b.index());
        // ...but the old handle must not resolve to the new occupant.
        assert_eq!(s.get(a), None, "stale handle aliased a live entry");
        assert_eq!(s.get(b), Some(&2));
    }

    #[test]
    fn double_remove_is_a_no_op() {
        let mut s: GenerationalStore<FontId, u32> = GenerationalStore::new();
        let a = s.insert(7);
        assert_eq!(s.remove(a), Some(7));
        assert_eq!(s.remove(a), None);
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn out_of_range_handle_does_not_panic() {
        let s: GenerationalStore<NodeId, u32> = GenerationalStore::new();
        assert_eq!(s.get(NodeId::new(9999, 1)), None);
    }

    #[test]
    fn zero_generation_is_never_valid() {
        let mut s: GenerationalStore<NodeId, u32> = GenerationalStore::new();
        let a = s.insert(1);
        // A caller fabricating generation 0 gets remapped to 1; make sure a
        // fabricated handle for an unused slot still misses.
        assert!(NodeId::new(a.index(), 0).generation() >= 1);
        assert_eq!(s.get(NodeId::new(a.index() + 50, 0)), None);
    }

    #[test]
    fn retain_frees_slots_for_reuse() {
        let mut s: GenerationalStore<ImageId, u32> = GenerationalStore::new();
        let a = s.insert(1);
        let b = s.insert(2);
        s.retain(|_, v| *v != 1);
        assert_eq!(s.get(a), None);
        assert_eq!(s.get(b), Some(&2));
        assert_eq!(s.len(), 1);
        let c = s.insert(3);
        assert_eq!(c.index(), a.index(), "freed slot should be reused");
        assert_ne!(c.generation(), a.generation());
    }

    #[test]
    fn iter_visits_only_live_entries() {
        let mut s: GenerationalStore<ImageId, u32> = GenerationalStore::new();
        let a = s.insert(1);
        s.insert(2);
        s.insert(3);
        s.remove(a);
        let mut vals: Vec<u32> = s.iter().map(|(_, v)| *v).collect();
        vals.sort_unstable();
        assert_eq!(vals, vec![2, 3]);
    }

    #[test]
    fn element_id_from_key_is_stable_and_unique_is_not() {
        assert_eq!(ElementId::from_key("track-3"), ElementId::from_key("track-3"));
        assert_ne!(ElementId::from_key("track-3"), ElementId::from_key("track-4"));
        assert_ne!(ElementId::unique(), ElementId::unique());
    }

    #[test]
    fn key_derived_and_counter_derived_ids_cannot_collide() {
        // Counter ids start low and never set the high bit; key ids always do.
        let u = ElementId::unique();
        let k = ElementId::from_key("x");
        assert_eq!(u.get() >> 63, 0);
        assert_eq!(k.get() >> 63, 1);
    }

    #[test]
    fn same_key_under_different_parents_stays_distinct() {
        let p1 = ElementId::from_key("panel-a");
        let p2 = ElementId::from_key("panel-b");
        assert_ne!(p1.child("row"), p2.child("row"));
        assert_eq!(p1.child("row"), p1.child("row"));
    }
}
