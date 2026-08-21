//! Content-addressed SVG parsing and drawing.
//!
//! Parsing an icon is expensive and an icon is drawn every frame, so the cache
//! is not an optimisation here, it is the reason the crate is usable at all.

use crate::document::SvgDocument;
use rustc_hash::FxHashMap;
use spherekit_core::{Affine, Color, GenerationalStore, Px, Rect, SvgError, SvgId};
use spherekit_render::Canvas;

/// Counters for the diagnostics overlay.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SvgCacheStats {
    /// Loads served from the cache without parsing.
    pub hits: u64,
    /// Documents actually parsed.
    pub misses: u64,
    /// Parses that failed.
    pub failures: u64,
    /// Documents currently held.
    pub documents: usize,
    /// Approximate bytes held.
    pub bytes: usize,
}

impl SvgCacheStats {
    /// Fraction of loads served without parsing.
    pub fn hit_rate(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 { 1.0 } else { self.hits as f32 / total as f32 }
    }
}

/// Parses SVG documents once and draws them many times.
pub struct SvgCache {
    documents: GenerationalStore<SvgId, Entry>,
    /// Content hash to id, so the same bytes never parse twice.
    by_content: FxHashMap<u64, SvgId>,
    stats: SvgCacheStats,
    budget_bytes: usize,
    /// Frame counter, for least-recently-used eviction.
    frame: u64,
}

struct Entry {
    document: SvgDocument,
    /// Which content hash this came from, so eviction can clear the index.
    content: u64,
    bytes: usize,
    last_used: u64,
}

impl Default for SvgCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SvgCache {
    /// Icons are small; eight megabytes of parsed geometry is a very large icon
    /// set and still a rounding error next to a single texture.
    pub const DEFAULT_BUDGET_BYTES: usize = 8 * 1024 * 1024;

    /// A cache with the default budget.
    pub fn new() -> Self {
        Self::with_budget(Self::DEFAULT_BUDGET_BYTES)
    }

    /// A cache with an explicit byte budget.
    pub fn with_budget(budget_bytes: usize) -> Self {
        Self {
            documents: GenerationalStore::new(),
            by_content: FxHashMap::default(),
            stats: SvgCacheStats::default(),
            budget_bytes,
            frame: 0,
        }
    }

    /// Counters.
    pub fn stats(&self) -> SvgCacheStats {
        SvgCacheStats { documents: self.documents.len(), bytes: self.bytes(), ..self.stats }
    }

    /// Approximate bytes of parsed geometry held.
    pub fn bytes(&self) -> usize {
        self.documents.iter().map(|(_, e)| e.bytes).sum()
    }

    /// Advances the frame counter that drives eviction.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Parses SVG bytes, or returns the existing handle for identical bytes.
    pub fn load(&mut self, data: &[u8]) -> Result<SvgId, SvgError> {
        let content = content_hash(data);
        if let Some(id) = self.by_content.get(&content).copied()
            && self.documents.contains(id)
        {
            self.stats.hits += 1;
            if let Some(entry) = self.documents.get_mut(id) {
                entry.last_used = self.frame;
            }
            return Ok(id);
        }

        let document = match SvgDocument::parse(data) {
            Ok(d) => d,
            Err(e) => {
                self.stats.failures += 1;
                return Err(e);
            }
        };
        self.stats.misses += 1;

        let bytes = estimate_bytes(&document);
        let id = self.documents.insert(Entry { document, content, bytes, last_used: self.frame });
        self.by_content.insert(content, id);
        self.evict_to_budget();
        Ok(id)
    }

    /// Parses an SVG string.
    pub fn load_str(&mut self, text: &str) -> Result<SvgId, SvgError> {
        self.load(text.as_bytes())
    }

    /// Loads and parses an SVG file.
    pub fn load_file(&mut self, path: impl AsRef<std::path::Path>) -> Result<SvgId, SvgError> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|source| SvgError::Io { path: path.display().to_string(), source })?;
        self.load(&data)
    }

    /// The parsed document for a handle, or `None` when it is stale.
    pub fn document(&self, id: SvgId) -> Option<&SvgDocument> {
        self.documents.get(id).map(|e| &e.document)
    }

    /// True when the handle still resolves.
    pub fn contains(&self, id: SvgId) -> bool {
        self.documents.contains(id)
    }

    /// Number of parsed documents held.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// True when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Drops a document.
    pub fn remove(&mut self, id: SvgId) -> bool {
        match self.documents.remove(id) {
            Some(entry) => {
                self.by_content.remove(&entry.content);
                true
            }
            None => false,
        }
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.documents.clear();
        self.by_content.clear();
    }

    /// Draws a document into `dest`, scaled to fit and centred.
    ///
    /// `tint` replaces every paint in the document, which is what a
    /// theme-coloured monochrome icon wants. Passing `None` keeps the
    /// document's own colours.
    ///
    /// Returns `false` when the handle is stale or the destination is empty;
    /// a missing icon must not be a reason to fail a frame.
    pub fn render(
        &mut self,
        id: SvgId,
        canvas: &mut Canvas<'_>,
        dest: Rect<Px>,
        tint: Option<Color>,
    ) -> bool {
        let frame = self.frame;
        let Some(entry) = self.documents.get_mut(id) else { return false };
        entry.last_used = frame;
        if dest.is_empty() {
            return false;
        }

        let transform = entry.document.fit_transform(dest);
        // Transform the geometry rather than the canvas, so the caller's own
        // transform and clip stack are untouched and the icon composes normally
        // with whatever is around it.
        for shape in &entry.document.shapes {
            if shape.is_invisible() {
                continue;
            }
            let path = shape.path.transformed(transform);

            if let Some(fill) = &shape.fill {
                let brush = match tint {
                    Some(color) => spherekit_core::Brush::Solid(color),
                    None => fill.clone(),
                };
                canvas.fill_path(path.clone(), brush.scale_alpha(shape.opacity));
            }
            if let Some(stroke) = &shape.stroke {
                let brush = match tint {
                    Some(color) => spherekit_core::Brush::Solid(color),
                    None => stroke.clone(),
                };
                // The stroke width is in user space, so it scales with the
                // icon; a 1-unit hairline in a 16-unit view box drawn at 64 px
                // should be 4 px wide, not 1.
                let mut style = shape.stroke_style.clone();
                style.width = Px(style.width.get() * transform.approx_scale());
                canvas.stroke_path(path, brush.scale_alpha(shape.opacity), style);
            }
        }
        true
    }

    /// Draws with an explicit transform instead of a destination rectangle.
    pub fn render_transformed(
        &mut self,
        id: SvgId,
        canvas: &mut Canvas<'_>,
        transform: Affine,
        tint: Option<Color>,
    ) -> bool {
        let Some(document) = self.documents.get(id).map(|e| e.document.clone()) else {
            return false;
        };
        let bounds = Rect::new(spherekit_core::Point::ZERO, document.view_box);
        let _ = bounds;
        for shape in &document.shapes {
            if shape.is_invisible() {
                continue;
            }
            let path = shape.path.transformed(transform);
            if let Some(fill) = &shape.fill {
                let brush = tint.map(spherekit_core::Brush::Solid).unwrap_or_else(|| fill.clone());
                canvas.fill_path(path.clone(), brush.scale_alpha(shape.opacity));
            }
            if let Some(stroke) = &shape.stroke {
                let brush =
                    tint.map(spherekit_core::Brush::Solid).unwrap_or_else(|| stroke.clone());
                let mut style = shape.stroke_style.clone();
                style.width = Px(style.width.get() * transform.approx_scale());
                canvas.stroke_path(path, brush.scale_alpha(shape.opacity), style);
            }
        }
        true
    }

    /// Evicts least-recently-used documents until the budget is met.
    fn evict_to_budget(&mut self) {
        if self.bytes() <= self.budget_bytes {
            return;
        }
        // Collect first, because eviction mutates the store being iterated.
        let mut candidates: Vec<(u64, SvgId, usize)> =
            self.documents.iter().map(|(id, e)| (e.last_used, id, e.bytes)).collect();
        candidates.sort_by_key(|(last_used, _, _)| *last_used);

        let mut total = self.bytes();
        for (last_used, id, bytes) in candidates {
            if total <= self.budget_bytes {
                break;
            }
            // Never evict something loaded on the current frame: it is about to
            // be drawn, and evicting it would guarantee a re-parse next frame.
            if last_used == self.frame {
                continue;
            }
            if self.remove(id) {
                total = total.saturating_sub(bytes);
            }
        }
    }
}

/// A 64-bit content hash. Two different documents colliding would return the
/// wrong icon, so this is wide enough that it will not happen in practice.
fn content_hash(data: &[u8]) -> u64 {
    use core::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one(data)
}

/// Approximate heap footprint of a parsed document.
fn estimate_bytes(document: &SvgDocument) -> usize {
    use core::mem::size_of;
    document
        .shapes
        .iter()
        .map(|s| {
            size_of::<crate::document::SvgShape>()
                + core::mem::size_of_val(s.path.points())
                + s.path.verbs().len()
        })
        .sum::<usize>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{ScaleFactor, Size, px, rect, size};
    use spherekit_render::Scene;

    const ICON: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
        <path d="M2 8 L14 8" stroke="red" stroke-width="2"/></svg>"#;

    const FILLED: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
        <rect width="100" height="100" fill="lime"/></svg>"#;

    fn scene() -> Scene {
        Scene::new(size(px(400.0), px(300.0)), ScaleFactor::IDENTITY)
    }

    #[test]
    fn identical_bytes_parse_once_and_share_a_handle() {
        let mut cache = SvgCache::new();
        let a = cache.load_str(ICON).unwrap();
        let b = cache.load_str(ICON).unwrap();
        assert_eq!(a, b);
        assert_eq!(cache.stats().misses, 1, "the same bytes were parsed twice");
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn different_bytes_get_different_handles() {
        let mut cache = SvgCache::new();
        let a = cache.load_str(ICON).unwrap();
        let b = cache.load_str(FILLED).unwrap();
        assert_ne!(a, b);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn a_failed_parse_leaves_the_cache_clean() {
        let mut cache = SvgCache::new();
        assert!(cache.load_str("<svg><path d=").is_err());
        assert!(cache.is_empty());
        assert_eq!(cache.stats().failures, 1);
        assert_eq!(cache.stats().misses, 0);
    }

    #[test]
    fn a_stale_handle_resolves_to_nothing_rather_than_someone_elses_icon() {
        let mut cache = SvgCache::new();
        let a = cache.load_str(ICON).unwrap();
        assert!(cache.remove(a));
        assert!(!cache.contains(a));
        assert!(cache.document(a).is_none());

        // The slot is reused; the old handle must not resolve to the new icon.
        let b = cache.load_str(FILLED).unwrap();
        assert_eq!(a.index(), b.index());
        assert!(cache.document(a).is_none(), "a stale handle aliased a live entry");
        assert!(cache.document(b).is_some());
    }

    #[test]
    fn rendering_produces_geometry_scaled_into_the_destination() {
        let mut cache = SvgCache::new();
        let id = cache.load_str(FILLED).unwrap();
        let mut s = scene();
        {
            let mut canvas = spherekit_render::Canvas::new(&mut s);
            assert!(cache.render(
                id,
                &mut canvas,
                rect(px(0.0), px(0.0), px(200.0), px(200.0)),
                None
            ));
        }
        assert!(!s.is_empty());
        // A 100x100 view box into 200x200 doubles the geometry.
        let path = &s.paths[0];
        let b = path.control_bounds();
        assert!((b.width().get() - 200.0).abs() < 0.5, "{b:?}");
    }

    #[test]
    fn rendering_a_stale_handle_returns_false_rather_than_failing_the_frame() {
        let mut cache = SvgCache::new();
        let id = cache.load_str(ICON).unwrap();
        cache.remove(id);
        let mut s = scene();
        {
            let mut canvas = spherekit_render::Canvas::new(&mut s);
            assert!(!cache.render(
                id,
                &mut canvas,
                rect(px(0.0), px(0.0), px(16.0), px(16.0)),
                None
            ));
        }
        assert!(s.is_empty());
    }

    #[test]
    fn a_zero_size_destination_draws_nothing() {
        let mut cache = SvgCache::new();
        let id = cache.load_str(FILLED).unwrap();
        let mut s = scene();
        {
            let mut canvas = spherekit_render::Canvas::new(&mut s);
            assert!(!cache.render(
                id,
                &mut canvas,
                Rect::new(spherekit_core::Point::ZERO, Size::new(Px::ZERO, px(10.0))),
                None
            ));
        }
        assert!(s.is_empty());
    }

    #[test]
    fn a_tint_replaces_every_paint() {
        let mut cache = SvgCache::new();
        let id = cache.load_str(FILLED).unwrap();
        let mut s = scene();
        {
            let mut canvas = spherekit_render::Canvas::new(&mut s);
            cache.render(
                id,
                &mut canvas,
                rect(px(0.0), px(0.0), px(16.0), px(16.0)),
                Some(Color::hex(0x0000FF)),
            );
        }
        // The document is green; the tint must win.
        assert!(
            s.paints.iter().any(|p| p.brush == spherekit_core::Brush::Solid(Color::hex(0x0000FF))),
            "the tint did not replace the document's own fill"
        );
        assert!(
            !s.paints.iter().any(|p| p.brush == spherekit_core::Brush::Solid(Color::hex(0x00FF00))),
            "the document's own colour survived the tint"
        );
    }

    #[test]
    fn stroke_width_scales_with_the_icon() {
        // A 2-unit stroke in a 16-unit view box drawn at 64 px must be 8 px
        // wide, not 2. Leaving it unscaled makes large icons look spindly.
        let mut cache = SvgCache::new();
        let id = cache.load_str(ICON).unwrap();
        let mut s = scene();
        {
            let mut canvas = spherekit_render::Canvas::new(&mut s);
            cache.render(id, &mut canvas, rect(px(0.0), px(0.0), px(64.0), px(64.0)), None);
        }
        assert_eq!(s.strokes.len(), 1);
        assert!((s.strokes[0].width.get() - 8.0).abs() < 0.1, "{:?}", s.strokes[0].width);
    }

    #[test]
    fn eviction_drops_the_least_recently_used_but_never_this_frames_load() {
        // A tiny budget forces eviction on every insert.
        let mut cache = SvgCache::with_budget(1);
        cache.begin_frame();
        let first = cache.load_str(ICON).unwrap();
        // Both were loaded this frame, so neither may be evicted.
        let second = cache.load_str(FILLED).unwrap();
        assert!(cache.contains(first) && cache.contains(second), "a this-frame load was evicted");

        // On a later frame the older one becomes a candidate.
        cache.begin_frame();
        cache.begin_frame();
        let third = cache
            .load_str(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 8 8">
               <rect width="8" height="8" fill="rgb(18,52,86)"/></svg>"#,
            )
            .unwrap();
        assert!(cache.contains(third));
        assert!(cache.len() < 3, "nothing was evicted despite a 1-byte budget");
    }

    #[test]
    fn clearing_empties_both_the_store_and_the_content_index() {
        let mut cache = SvgCache::new();
        cache.load_str(ICON).unwrap();
        cache.load_str(FILLED).unwrap();
        cache.clear();
        assert!(cache.is_empty());
        // Reloading must parse again rather than returning a dangling handle.
        let reloaded = cache.load_str(ICON).unwrap();
        assert!(cache.contains(reloaded));
        assert_eq!(cache.stats().misses, 3);
    }

    #[test]
    fn hit_rate_of_an_untouched_cache_is_one() {
        assert_eq!(SvgCacheStats::default().hit_rate(), 1.0);
    }

    #[test]
    fn a_missing_file_is_an_error_and_leaves_no_entry() {
        let mut cache = SvgCache::new();
        let err = cache.load_file("does/not/exist.svg").unwrap_err();
        assert!(matches!(err, SvgError::Io { .. }));
        assert!(cache.is_empty());
    }

    #[test]
    fn an_unsupported_feature_is_refused_rather_than_dropped() {
        let mut cache = SvgCache::new();
        let err = cache
            .load_str(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
                   <text x="0" y="5">hi</text></svg>"#,
            )
            .unwrap_err();
        assert!(matches!(err, SvgError::Unsupported(_)), "{err}");
        assert_eq!(cache.stats().failures, 1);
    }
}
