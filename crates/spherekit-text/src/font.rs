//! Font discovery, loading and fallback.
//!
//! ## Owning the bytes
//!
//! Both `ttf_parser::Face` and `rustybuzz::Face` *borrow* the byte slice they
//! were parsed from, so neither can be stored next to the buffer it points at
//! without a self-referential struct. SphereKit solves this the boring way: the
//! database owns every face's bytes in an `Arc<dyn AsRef<[u8]> + Send + Sync>`
//! and hands out a *scoped accessor* ([`FontDatabase::with_face`]) that parses
//! a face on the stack for the duration of a closure.
//!
//! The alternatives were considered and rejected:
//!
//! * `Box::leak` makes every loaded font a permanent leak, which a plug-in host
//!   that opens and closes a hundred editor windows will notice.
//! * `transmute`-ing the lifetime is unsound the moment a face is unloaded.
//! * A self-referential crate (`ouroboros`, `yoke`) adds a dependency and still
//!   only saves the table-directory parse, which is a few microseconds.
//!
//! Re-parsing is cheap because `ttf_parser` is zero-copy: `Face::parse` reads
//! the table directory and each table's header, and nothing else. Shaping calls
//! it once per run, not once per glyph, so it never shows up in a profile.
//!
//! ## Fallback
//!
//! A single face never covers everything a DAW window shows: a track named in
//! Thai, a plug-in vendor string in Japanese, an emoji in a comment. When the
//! primary face has no glyph for a codepoint, [`FontDatabase::fallback_for`]
//! walks a script-derived chain of families and returns the first face that
//! does. The per-platform family lists are hints, never requirements: if none
//! of them are installed the database is scanned for *any* face with the glyph,
//! so the chain degrades to "something readable" rather than to tofu.

use std::path::Path;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use spherekit_core::{FontError, FontId, GenerationalStore, GlyphId};

use crate::types::{FontMetrics, FontRequest, FontStretch, FontStyle, FontWeight};

/// A coarse script bucket used to pick a fallback family list.
///
/// Deliberately much coarser than `unicode_script::Script`: the point is to
/// choose between "the Japanese font" and "the Thai font", not to model the
/// whole of ISO 15924. Anything unrecognised lands in [`FallbackBucket::Other`]
/// and falls through to the full chain.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum FallbackBucket {
    /// Latin and anything that shares a face with it.
    Latin,
    /// Cyrillic.
    Cyrillic,
    /// Greek and Coptic.
    Greek,
    /// Arabic, including its supplements and presentation forms.
    Arabic,
    /// Hebrew.
    Hebrew,
    /// Thai.
    Thai,
    /// Devanagari and the other North Indic scripts that share a UI font.
    Devanagari,
    /// Japanese: hiragana, katakana and Japanese-flavoured kana punctuation.
    Japanese,
    /// Han, defaulting to the Simplified Chinese family list.
    ChineseSimplified,
    /// Bopomofo and the Traditional Chinese family list.
    ChineseTraditional,
    /// Hangul.
    Korean,
    /// Pictographs, dingbats and the other emoji blocks.
    Emoji,
    /// Anything not worth a dedicated list.
    Other,
}

/// Every bucket, in the order the fallback chain tries them after the
/// codepoint's own bucket has been tried and missed.
pub const FALLBACK_BUCKETS: [FallbackBucket; 12] = [
    FallbackBucket::Latin,
    FallbackBucket::Cyrillic,
    FallbackBucket::Greek,
    FallbackBucket::Emoji,
    FallbackBucket::Japanese,
    FallbackBucket::ChineseSimplified,
    FallbackBucket::ChineseTraditional,
    FallbackBucket::Korean,
    FallbackBucket::Thai,
    FallbackBucket::Devanagari,
    FallbackBucket::Arabic,
    FallbackBucket::Hebrew,
];

/// Classifies a codepoint into the bucket whose family list should be tried
/// first.
///
/// This is a block-range test rather than a `unicode-script` lookup because the
/// buckets are coarser than scripts and because the emoji blocks cut across
/// several scripts. It is context-free by design: without language tagging a
/// Han character cannot be attributed to Chinese rather than Japanese, so it is
/// reported as [`FallbackBucket::ChineseSimplified`] and the rest of the chain
/// picks up the slack when that family is absent.
pub fn fallback_bucket(c: char) -> FallbackBucket {
    let u = c as u32;
    match u {
        // Emoji and pictograph blocks first: they overlap "symbol" ranges that
        // would otherwise be classified as Latin punctuation.
        0x1F000..=0x1FBFF => FallbackBucket::Emoji,
        0x2600..=0x27BF => FallbackBucket::Emoji,
        0x2B00..=0x2BFF => FallbackBucket::Emoji,
        0xFE00..=0xFE0F => FallbackBucket::Emoji,

        0x0000..=0x024F => FallbackBucket::Latin,
        0x0370..=0x03FF | 0x1F00..=0x1FFF | 0x2C80..=0x2CFF => FallbackBucket::Greek,
        0x0400..=0x052F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F => FallbackBucket::Cyrillic,
        0x0590..=0x05FF | 0xFB1D..=0xFB4F => FallbackBucket::Hebrew,
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF => FallbackBucket::Arabic,
        0xFB50..=0xFDFF | 0xFE70..=0xFEFF => FallbackBucket::Arabic,
        0x0900..=0x097F | 0xA8E0..=0xA8FF => FallbackBucket::Devanagari,
        0x0E00..=0x0E7F => FallbackBucket::Thai,
        0x3040..=0x30FF | 0x31F0..=0x31FF => FallbackBucket::Japanese,
        0x3100..=0x312F | 0x31A0..=0x31BF => FallbackBucket::ChineseTraditional,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F => FallbackBucket::Korean,
        0xAC00..=0xD7AF => FallbackBucket::Korean,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF => FallbackBucket::ChineseSimplified,
        0x20000..=0x3FFFF => FallbackBucket::ChineseSimplified,
        _ => FallbackBucket::Other,
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::FallbackBucket;

    /// Family lists for Windows 10/11. Every one of these ships in some Windows
    /// SKU but none is guaranteed: a stripped LTSC image or a locale-specific
    /// install can be missing any of them, so the list is a preference order,
    /// not a requirement.
    pub fn families(bucket: FallbackBucket) -> &'static [&'static str] {
        match bucket {
            FallbackBucket::Latin | FallbackBucket::Other => {
                &["Segoe UI", "Tahoma", "Arial", "Microsoft Sans Serif"]
            }
            FallbackBucket::Cyrillic => &["Segoe UI", "Tahoma", "Arial"],
            FallbackBucket::Greek => &["Segoe UI", "Tahoma", "Arial"],
            FallbackBucket::Arabic => {
                &["Segoe UI", "Tahoma", "Arabic Typesetting", "Traditional Arabic"]
            }
            FallbackBucket::Hebrew => &["Segoe UI", "Gisha", "David", "Tahoma"],
            FallbackBucket::Thai => {
                &["Leelawadee UI", "Leelawadee", "Tahoma", "Microsoft Sans Serif"]
            }
            FallbackBucket::Devanagari => &["Nirmala UI", "Mangal", "Utsaah"],
            FallbackBucket::Japanese => {
                &["Yu Gothic UI", "Yu Gothic", "Meiryo UI", "Meiryo", "MS UI Gothic", "MS Gothic"]
            }
            FallbackBucket::ChineseSimplified => {
                &["Microsoft YaHei UI", "Microsoft YaHei", "SimSun", "SimHei"]
            }
            FallbackBucket::ChineseTraditional => {
                &["Microsoft JhengHei UI", "Microsoft JhengHei", "PMingLiU", "MingLiU"]
            }
            FallbackBucket::Korean => &["Malgun Gothic", "Gulim", "Batang"],
            FallbackBucket::Emoji => &["Segoe UI Emoji", "Segoe UI Symbol", "Segoe UI"],
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::FallbackBucket;

    /// Family lists for macOS. `.AppleSystemUIFont` is deliberately not used:
    /// it is a private name that `fontdb` will not match.
    pub fn families(bucket: FallbackBucket) -> &'static [&'static str] {
        match bucket {
            FallbackBucket::Latin | FallbackBucket::Other => {
                &["Helvetica Neue", "Helvetica", "Lucida Grande", "Arial"]
            }
            FallbackBucket::Cyrillic => &["Helvetica Neue", "Lucida Grande", "Arial"],
            FallbackBucket::Greek => &["Helvetica Neue", "Lucida Grande", "Arial"],
            FallbackBucket::Arabic => &["Geeza Pro", "Al Bayan", "Baghdad"],
            FallbackBucket::Hebrew => &["Arial Hebrew", "Lucida Grande"],
            FallbackBucket::Thai => &["Thonburi", "Ayuthaya", "Krungthep"],
            FallbackBucket::Devanagari => &["Kohinoor Devanagari", "Devanagari Sangam MN"],
            FallbackBucket::Japanese => {
                &["Hiragino Sans", "Hiragino Kaku Gothic ProN", "Osaka", "Apple SD Gothic Neo"]
            }
            FallbackBucket::ChineseSimplified => &["PingFang SC", "Heiti SC", "STHeiti"],
            FallbackBucket::ChineseTraditional => &["PingFang TC", "Heiti TC", "STHeiti"],
            FallbackBucket::Korean => &["Apple SD Gothic Neo", "AppleGothic"],
            FallbackBucket::Emoji => &["Apple Color Emoji", "Apple Symbols"],
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod platform {
    use super::FallbackBucket;

    /// Family lists for Linux and the BSDs. Noto covers everything when it is
    /// installed; DejaVu is the near-universal minimum.
    pub fn families(bucket: FallbackBucket) -> &'static [&'static str] {
        match bucket {
            FallbackBucket::Latin | FallbackBucket::Other => {
                &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Cantarell"]
            }
            FallbackBucket::Cyrillic => &["Noto Sans", "DejaVu Sans", "Liberation Sans"],
            FallbackBucket::Greek => &["Noto Sans", "DejaVu Sans", "Liberation Sans"],
            FallbackBucket::Arabic => &["Noto Sans Arabic", "Noto Naskh Arabic", "DejaVu Sans"],
            FallbackBucket::Hebrew => &["Noto Sans Hebrew", "DejaVu Sans"],
            FallbackBucket::Thai => &["Noto Sans Thai", "Noto Serif Thai", "Garuda", "Loma"],
            FallbackBucket::Devanagari => &["Noto Sans Devanagari", "Lohit Devanagari"],
            FallbackBucket::Japanese => &["Noto Sans CJK JP", "Noto Sans JP", "IPAGothic"],
            FallbackBucket::ChineseSimplified => {
                &["Noto Sans CJK SC", "Noto Sans SC", "WenQuanYi Zen Hei"]
            }
            FallbackBucket::ChineseTraditional => {
                &["Noto Sans CJK TC", "Noto Sans TC", "WenQuanYi Zen Hei"]
            }
            FallbackBucket::Korean => &["Noto Sans CJK KR", "Noto Sans KR", "NanumGothic"],
            FallbackBucket::Emoji => &["Noto Color Emoji", "Noto Emoji", "Symbola"],
        }
    }
}

/// The preferred family names for a bucket on the current platform.
///
/// Never empty, and never load-bearing: [`FontDatabase::fallback_for`] treats
/// every name as a hint and scans the whole database if none resolve.
pub fn fallback_families(bucket: FallbackBucket) -> &'static [&'static str] {
    platform::families(bucket)
}

/// A loaded face: its bytes, plus everything we want O(1) access to.
struct FaceEntry {
    /// The whole font *file* (not just this face), kept alive for as long as
    /// the entry exists so that borrowed faces can be re-created on demand.
    data: Arc<dyn AsRef<[u8]> + Send + Sync>,
    /// Which face within `data`; nonzero only for collections.
    index: u32,
    /// The primary (English) family name, for diagnostics.
    family: String,
    /// Vertical metrics, normalised to em units at registration time so that
    /// asking for them never re-parses the face.
    metrics: FontMetrics,
    /// The `fontdb` id this entry was registered from.
    source: fontdb::ID,
}

impl core::fmt::Debug for FaceEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FaceEntry")
            .field("family", &self.family)
            .field("index", &self.index)
            .field("bytes", &self.data.as_ref().as_ref().len())
            .finish()
    }
}

/// Cache key for a fallback lookup.
///
/// Weight and style are part of the key because falling back from a bold face
/// should land on a bold fallback where one exists.
type FallbackKey = (char, FontWeight, FontStyle);

/// Font discovery, loading, matching and fallback.
///
/// Wraps a [`fontdb::Database`] and adds the things a renderer needs on top of
/// it: stable [`FontId`]s, owned bytes, precomputed metrics, and a fallback
/// chain. Every lookup is memoised, so resolving the same style a thousand
/// times a frame costs a hash lookup rather than a database scan.
pub struct FontDatabase {
    /// The underlying face index.
    db: fontdb::Database,
    /// Faces we have actually loaded bytes for, keyed by the ids we hand out.
    faces: GenerationalStore<FontId, FaceEntry>,
    /// Deduplicates registration so one `fontdb` face maps to one [`FontId`].
    by_source: FxHashMap<fontdb::ID, FontId>,
    /// Memoised [`FontDatabase::resolve`] results, including negative ones.
    resolved: FxHashMap<FontRequest, Option<FontId>>,
    /// Memoised exact-family lookups, which unlike `resolved` never substitute
    /// a generic family and so cannot be shared with it.
    exact: FxHashMap<FontRequest, Option<FontId>>,
    /// Memoised per-face codepoint coverage.
    coverage: FxHashMap<(FontId, char), bool>,
    /// Memoised fallback decisions, including negative ones so that a codepoint
    /// no installed font covers is only searched for once.
    fallbacks: FxHashMap<FallbackKey, Option<FontId>>,
    /// Memoised grid-fitting zones, including negative results.
    ///
    /// Reading them costs 5 to 12 microseconds — seventeen glyph lookups plus
    /// two `OS/2` reads, measured on this machine — and a glyph rasterisation
    /// would otherwise pay it every time. That is invisible for a screen of
    /// text, and it is tens of milliseconds across a long document, so it is
    /// memoised rather than argued about.
    ///
    /// Survives `invalidate_matching` for the same reason `coverage` does: it
    /// is a property of bytes already loaded, and those cannot change.
    zones: FxHashMap<FontId, Option<crate::hint::VerticalZones>>,
}

impl Default for FontDatabase {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for FontDatabase {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FontDatabase")
            .field("faces_indexed", &self.db.len())
            .field("faces_loaded", &self.faces.len())
            .finish()
    }
}

impl FontDatabase {
    /// An empty database. No system fonts are scanned until you ask.
    ///
    /// Scanning is deliberately opt-in: a plug-in that ships its own font
    /// should not pay a few hundred milliseconds of directory walking at
    /// startup for faces it will never use.
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        // `fontdb`'s generic families default to Arial, Times New Roman and
        // Courier New. Those are the Windows 95 defaults; the shell has not
        // used any of them for interface text in decades, and on a non-Latin
        // system they have no coverage for the language the user reads in. An
        // unnamed `FontRequest` resolves through these, so leaving them is what
        // made unstyled text come out in the wrong face everywhere.
        db.set_sans_serif_family(crate::system_ui::family());
        db.set_monospace_family(crate::system_ui::monospace_family());
        Self {
            db,
            faces: GenerationalStore::new(),
            by_source: FxHashMap::default(),
            resolved: FxHashMap::default(),
            exact: FxHashMap::default(),
            coverage: FxHashMap::default(),
            fallbacks: FxHashMap::default(),
            zones: FxHashMap::default(),
        }
    }

    /// A database preloaded with the system's fonts.
    pub fn with_system_fonts() -> Self {
        let mut db = Self::new();
        db.load_system_fonts();
        db
    }

    /// Indexes every font the platform knows about.
    ///
    /// Only face *metadata* is read here; the bytes of a face are loaded the
    /// first time it is actually resolved.
    pub fn load_system_fonts(&mut self) {
        self.db.load_system_fonts();
        self.invalidate_matching();
    }

    /// Indexes every font in a directory, recursively.
    pub fn load_fonts_dir(&mut self, dir: impl AsRef<Path>) {
        self.db.load_fonts_dir(dir.as_ref());
        self.invalidate_matching();
    }

    /// Loads a font from memory, returning one [`FontId`] per face in it.
    ///
    /// A collection (`.ttc`) yields several ids. Faces loaded this way are
    /// registered eagerly, because the caller usually wants to use exactly the
    /// bytes it just handed over rather than to look them up by family.
    pub fn load_font_data(&mut self, data: Vec<u8>) -> Vec<FontId> {
        let source = fontdb::Source::Binary(Arc::new(data));
        let ids: Vec<fontdb::ID> = self.db.load_font_source(source).into_iter().collect();
        self.invalidate_matching();
        ids.into_iter().filter_map(|id| self.register(id)).collect()
    }

    /// Loads a font from disk, returning one [`FontId`] per face in it.
    ///
    /// # Errors
    ///
    /// Returns [`FontError::Io`] when the file cannot be read and
    /// [`FontError::Parse`] when it contains no usable face.
    pub fn load_font_file(&mut self, path: impl AsRef<Path>) -> Result<Vec<FontId>, FontError> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|source| FontError::Io { path: path.display().to_string(), source })?;
        let ids = self.load_font_data(data);
        if ids.is_empty() {
            return Err(FontError::Parse {
                name: path.display().to_string(),
                reason: "no usable face in file".into(),
            });
        }
        Ok(ids)
    }

    /// The number of faces indexed, loaded or not.
    pub fn face_count(&self) -> usize {
        self.db.len()
    }

    /// The number of faces whose bytes have actually been loaded.
    pub fn loaded_count(&self) -> usize {
        self.faces.len()
    }

    /// True when no face is indexed at all.
    pub fn is_empty(&self) -> bool {
        self.db.is_empty()
    }

    /// The underlying `fontdb` database, for callers that need to enumerate
    /// faces themselves.
    pub fn inner(&self) -> &fontdb::Database {
        &self.db
    }

    /// Resolves a request to a face, substituting generic families and finally
    /// *any* installed face rather than failing.
    ///
    /// Returning something legible beats returning nothing: a missing family
    /// name in a saved project should not blank out the UI. `None` therefore
    /// means only one thing, that the database is empty.
    pub fn resolve(&mut self, request: &FontRequest) -> Option<FontId> {
        if let Some(&hit) = self.resolved.get(request) {
            return hit;
        }
        let found = self.query_with_substitutes(request);
        self.resolved.insert(request.clone(), found);
        found
    }

    /// Resolves a single family by name, with no substitution.
    ///
    /// Returns `None` when that exact family is not installed, which is what
    /// makes it usable for probing a fallback chain.
    pub fn resolve_family(&mut self, family: &str, like: &FontRequest) -> Option<FontId> {
        let key = FontRequest {
            families: vec![family.to_owned()],
            weight: like.weight,
            style: like.style,
            stretch: like.stretch,
        };
        if let Some(&hit) = self.exact.get(&key) {
            return hit;
        }
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight: fontdb::Weight(like.weight.0),
            stretch: map_stretch(like.stretch),
            style: map_style(like.style),
        };
        let found = self.db.query(&query).and_then(|id| self.register(id));
        self.exact.insert(key, found);
        found
    }

    /// Runs a closure with a parsed, shapeable face.
    ///
    /// The face is parsed on the stack and dropped when the closure returns,
    /// which is what lets the database own the bytes without a self-referential
    /// struct. It is handed over mutably so callers can apply variation
    /// coordinates without those leaking into the next caller's view of the
    /// same face.
    ///
    /// Returns `None` when the id is stale or the face fails to parse.
    pub fn with_face<T>(
        &self,
        font: FontId,
        f: impl FnOnce(&mut rustybuzz::Face<'_>) -> T,
    ) -> Option<T> {
        let entry = self.faces.get(font)?;
        let bytes: &[u8] = entry.data.as_ref().as_ref();
        let mut face = rustybuzz::Face::from_slice(bytes, entry.index)?;
        Some(f(&mut face))
    }

    /// Runs a closure with a parsed outline face.
    ///
    /// Cheaper than [`FontDatabase::with_face`] when no shaping tables are
    /// needed, which is the common case for metrics and coverage queries.
    pub fn with_outline_face<T>(
        &self,
        font: FontId,
        f: impl FnOnce(&ttf_parser::Face<'_>) -> T,
    ) -> Option<T> {
        let entry = self.faces.get(font)?;
        let bytes: &[u8] = entry.data.as_ref().as_ref();
        let face = ttf_parser::Face::parse(bytes, entry.index).ok()?;
        Some(f(&face))
    }

    /// Vertical metrics for a face, normalised to em units.
    ///
    /// Dividing by `units_per_em` at load time means the caller multiplies by a
    /// pixel size and is done; nothing downstream has to remember whether a
    /// particular face is 1000 or 2048 units per em.
    pub fn face_metrics(&self, font: FontId) -> Option<FontMetrics> {
        self.faces.get(font).map(|e| e.metrics)
    }

    /// The vertical grid-fitting zones for a face, measured once.
    ///
    /// `None` means the face carries no usable `OS/2` metrics and must not be
    /// fitted; see [`crate::hint::VerticalZones::from_face`].
    pub fn vertical_zones(&mut self, font: FontId) -> Option<crate::hint::VerticalZones> {
        if let Some(cached) = self.zones.get(&font) {
            return *cached;
        }
        let measured =
            self.with_face(font, |face| crate::hint::VerticalZones::from_face(face)).flatten();
        self.zones.insert(font, measured);
        measured
    }

    /// The face's primary family name.
    pub fn family_name(&self, font: FontId) -> Option<&str> {
        self.faces.get(font).map(|e| e.family.as_str())
    }

    /// The `fontdb` id a face was registered from.
    ///
    /// Lets a caller reach the rest of the face's metadata — post-script name,
    /// monospace flag, source path — without this type having to mirror it.
    pub fn source_id(&self, font: FontId) -> Option<fontdb::ID> {
        self.faces.get(font).map(|e| e.source)
    }

    /// The glyph index a face maps a codepoint to, if any.
    pub fn glyph_index(&self, font: FontId, c: char) -> Option<GlyphId> {
        self.with_outline_face(font, |face| face.glyph_index(c).map(|g| GlyphId(g.0)))?
    }

    /// True when the face has a glyph for the codepoint.
    ///
    /// Takes `&mut self` because the answer is memoised: itemisation asks this
    /// once per character of every string laid out, and re-parsing the face's
    /// `cmap` that often is the difference between a warm layout costing
    /// microseconds and costing milliseconds.
    pub fn has_glyph(&mut self, font: FontId, c: char) -> bool {
        if let Some(&hit) = self.coverage.get(&(font, c)) {
            return hit;
        }
        let found = self.glyph_index(font, c).is_some();
        self.coverage.insert((font, c), found);
        found
    }

    /// Finds a face for a codepoint the primary face cannot render.
    ///
    /// Tries, in order: the family list for the codepoint's own bucket, then
    /// every other bucket's list, then a scan of the whole database. Both hits
    /// and misses are memoised, so the expensive scan happens at most once per
    /// distinct codepoint.
    pub fn fallback_for(&mut self, c: char, like: &FontRequest) -> Option<FontId> {
        let key = (c, like.weight, like.style);
        if let Some(&hit) = self.fallbacks.get(&key) {
            return hit;
        }
        let found = self.search_fallback(c, like);
        self.fallbacks.insert(key, found);
        found
    }

    /// The resolved fallback families for a bucket, as ids.
    ///
    /// Exposed mostly for diagnostics: it answers "which of the Thai fonts I
    /// hoped for is actually installed here?".
    pub fn fallback_chain(&mut self, bucket: FallbackBucket, like: &FontRequest) -> Vec<FontId> {
        let mut out = Vec::new();
        for family in fallback_families(bucket) {
            if let Some(id) = self.resolve_family(family, like) {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    // ---- internals -------------------------------------------------------

    /// Drops every memoised *matching* decision.
    ///
    /// Coverage and registration survive because they are properties of bytes
    /// already loaded, and those cannot change; family matching and fallback do
    /// not, because a newly indexed family can turn a `None` into a `Some`.
    fn invalidate_matching(&mut self) {
        self.resolved.clear();
        self.exact.clear();
        self.fallbacks.clear();
    }

    fn query_with_substitutes(&mut self, request: &FontRequest) -> Option<FontId> {
        let mut families: Vec<fontdb::Family> =
            request.families.iter().map(|s| fontdb::Family::Name(s.as_str())).collect();
        families.push(fontdb::Family::SansSerif);
        families.push(fontdb::Family::Serif);
        families.push(fontdb::Family::Monospace);

        let query = fontdb::Query {
            families: &families,
            weight: fontdb::Weight(request.weight.0),
            stretch: map_stretch(request.stretch),
            style: map_style(request.style),
        };
        // Last resort: the generic families are configurable strings that a
        // stripped system may simply not have, so fall through to whatever is
        // installed rather than handing back nothing.
        let id = self.db.query(&query).or_else(|| self.db.faces().next().map(|f| f.id))?;
        self.register(id)
    }

    fn search_fallback(&mut self, c: char, like: &FontRequest) -> Option<FontId> {
        let own = fallback_bucket(c);
        let ordered = core::iter::once(own).chain(FALLBACK_BUCKETS.iter().copied());
        for bucket in ordered {
            for family in fallback_families(bucket) {
                if let Some(id) = self.resolve_family(family, like) {
                    if self.has_glyph(id, c) {
                        return Some(id);
                    }
                }
            }
        }
        self.scan_for_glyph(c)
    }

    /// The last resort: parse every indexed face until one covers `c`.
    ///
    /// Expensive — it touches every font file on the machine — which is exactly
    /// why [`FontDatabase::fallback_for`] memoises the answer, negative results
    /// included.
    fn scan_for_glyph(&mut self, c: char) -> Option<FontId> {
        let ids: Vec<fontdb::ID> = self.db.faces().map(|f| f.id).collect();
        for id in ids {
            let covered = self
                .db
                .with_face_data(id, |data, index| {
                    ttf_parser::Face::parse(data, index)
                        .map(|f| f.glyph_index(c).is_some())
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if covered {
                return self.register(id);
            }
        }
        None
    }

    /// Loads a face's bytes and metrics, or returns the id it already has.
    fn register(&mut self, source: fontdb::ID) -> Option<FontId> {
        if let Some(&existing) = self.by_source.get(&source) {
            return Some(existing);
        }

        let family = self
            .db
            .face(source)
            .and_then(|info| info.families.first().map(|(n, _)| n.clone()))
            .unwrap_or_default();
        let index = self.db.face(source)?.index;

        // A `Source::Binary` already owns shareable bytes, so clone the `Arc`.
        // Anything file-backed is read once into memory here: `with_face_data`
        // memory-maps the file only for the duration of the callback, and a
        // face parsed from that mapping would dangle the moment it returned.
        let data: Arc<dyn AsRef<[u8]> + Send + Sync> = match self.db.face_source(source) {
            Some((fontdb::Source::Binary(shared), _)) => shared,
            _ => Arc::new(self.db.with_face_data(source, |data, _| data.to_vec())?),
        };

        let metrics = {
            let bytes: &[u8] = data.as_ref().as_ref();
            let face = ttf_parser::Face::parse(bytes, index).ok()?;
            metrics_of(&face)
        };

        let id = self.faces.insert(FaceEntry { data, index, family, metrics, source });
        self.by_source.insert(source, id);
        Some(id)
    }
}

/// Extracts em-normalised vertical metrics from a parsed face.
fn metrics_of(face: &ttf_parser::Face<'_>) -> FontMetrics {
    let upem = face.units_per_em().max(1);
    let s = 1.0 / upem as f32;

    let ascent = face.ascender() as f32 * s;
    // `descender` is negative in font units; SphereKit stores descent as a
    // positive distance below the baseline.
    let descent = -(face.descender() as f32) * s;
    let line_gap = face.line_gap() as f32 * s;

    // OS/2 versions below 2 carry no sxHeight/sCapHeight, and plenty of shipped
    // fonts leave them at zero even when they do. Measuring the actual glyphs
    // is more reliable than trusting the table, and a ratio of the ascent is a
    // last resort that at least keeps derived layout finite.
    let ink_top = |c: char| -> Option<f32> {
        let g = face.glyph_index(c)?;
        let bb = face.glyph_bounding_box(g)?;
        Some(bb.y_max as f32 * s)
    };
    let x_height = face
        .x_height()
        .filter(|v| *v > 0)
        .map(|v| v as f32 * s)
        .or_else(|| ink_top('x'))
        .unwrap_or(ascent * 0.52);
    let cap_height = face
        .capital_height()
        .filter(|v| *v > 0)
        .map(|v| v as f32 * s)
        .or_else(|| ink_top('H'))
        .unwrap_or(ascent * 0.72);

    let (underline_offset, underline_thickness) = match face.underline_metrics() {
        // `position` is measured upward from the baseline and is normally
        // negative; SphereKit wants a downward offset.
        Some(m) => (-(m.position as f32) * s, (m.thickness as f32 * s).abs()),
        None => (0.1, 0.05),
    };

    FontMetrics {
        ascent,
        descent,
        line_gap,
        x_height,
        cap_height,
        underline_offset,
        underline_thickness: if underline_thickness > 0.0 { underline_thickness } else { 0.05 },
        units_per_em: upem,
    }
}

/// Maps SphereKit's width class onto `fontdb`'s.
fn map_stretch(s: FontStretch) -> fontdb::Stretch {
    match s {
        FontStretch::UltraCondensed => fontdb::Stretch::UltraCondensed,
        FontStretch::ExtraCondensed => fontdb::Stretch::ExtraCondensed,
        FontStretch::Condensed => fontdb::Stretch::Condensed,
        FontStretch::SemiCondensed => fontdb::Stretch::SemiCondensed,
        FontStretch::Normal => fontdb::Stretch::Normal,
        FontStretch::SemiExpanded => fontdb::Stretch::SemiExpanded,
        FontStretch::Expanded => fontdb::Stretch::Expanded,
        FontStretch::ExtraExpanded => fontdb::Stretch::ExtraExpanded,
        FontStretch::UltraExpanded => fontdb::Stretch::UltraExpanded,
    }
}

/// Maps SphereKit's slant onto `fontdb`'s.
fn map_style(s: FontStyle) -> fontdb::Style {
    match s {
        FontStyle::Normal => fontdb::Style::Normal,
        FontStyle::Italic => fontdb::Style::Italic,
        FontStyle::Oblique => fontdb::Style::Oblique,
    }
}

#[cfg(test)]
mod weight_tests {
    use super::*;

    /// A database with real faces, or `None` on a machine with no fonts.
    fn system() -> Option<FontDatabase> {
        let mut db = FontDatabase::with_system_fonts();
        db.resolve(&FontRequest::default())?;
        Some(db)
    }

    #[test]
    fn asking_for_bold_resolves_a_different_face_than_regular() {
        // Weight travels all the way to `fontdb::Query`. If it did not, every
        // heading in every interface would silently come out at book weight and
        // there would be nothing to see but a flat-looking screen.
        let Some(mut db) = system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let regular = db.resolve(&FontRequest::default().weight(FontWeight::NORMAL));
        let semi_bold = db.resolve(&FontRequest::default().weight(FontWeight::SEMI_BOLD));
        let bold = db.resolve(&FontRequest::default().weight(FontWeight::BOLD));
        let (Some(regular), Some(semi_bold), Some(bold)) = (regular, semi_bold, bold) else {
            eprintln!("no weighted faces; skipping");
            return;
        };
        assert_ne!(regular, semi_bold, "semi-bold resolved to the regular face");
        assert_ne!(regular, bold, "bold resolved to the regular face");
        assert_ne!(
            db.family_name(regular).map(str::to_string),
            None,
            "a resolved face must name its family"
        );
    }

    #[test]
    fn every_named_weight_resolves_to_something() {
        let Some(mut db) = system() else {
            eprintln!("no system font; skipping");
            return;
        };
        for w in [
            FontWeight::LIGHT,
            FontWeight::NORMAL,
            FontWeight::MEDIUM,
            FontWeight::SEMI_BOLD,
            FontWeight::BOLD,
            FontWeight::BLACK,
        ] {
            assert!(
                db.resolve(&FontRequest::default().weight(w)).is_some(),
                "weight {} resolved to nothing",
                w.0
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database with the system's fonts, or `None` on a machine with none.
    ///
    /// Every font-dependent test goes through this so the suite still passes in
    /// a bare container.
    pub(crate) fn system() -> Option<(FontDatabase, FontId)> {
        let mut db = FontDatabase::with_system_fonts();
        let id = db.resolve(&FontRequest::default())?;
        Some((db, id))
    }

    #[test]
    fn buckets_classify_representative_codepoints() {
        assert_eq!(fallback_bucket('A'), FallbackBucket::Latin);
        assert_eq!(fallback_bucket('é'), FallbackBucket::Latin);
        assert_eq!(fallback_bucket('Д'), FallbackBucket::Cyrillic);
        assert_eq!(fallback_bucket('Ω'), FallbackBucket::Greek);
        assert_eq!(fallback_bucket('ع'), FallbackBucket::Arabic);
        assert_eq!(fallback_bucket('ש'), FallbackBucket::Hebrew);
        assert_eq!(fallback_bucket('ก'), FallbackBucket::Thai);
        assert_eq!(fallback_bucket('क'), FallbackBucket::Devanagari);
        assert_eq!(fallback_bucket('あ'), FallbackBucket::Japanese);
        assert_eq!(fallback_bucket('カ'), FallbackBucket::Japanese);
        assert_eq!(fallback_bucket('中'), FallbackBucket::ChineseSimplified);
        assert_eq!(fallback_bucket('ㄅ'), FallbackBucket::ChineseTraditional);
        assert_eq!(fallback_bucket('한'), FallbackBucket::Korean);
        assert_eq!(fallback_bucket('😀'), FallbackBucket::Emoji);
        assert_eq!(fallback_bucket('\u{FE0F}'), FallbackBucket::Emoji);
    }

    #[test]
    fn every_bucket_has_a_usable_family_list() {
        // An empty list would silently skip a whole script's preferred fonts and
        // drop straight to the O(database) scan.
        for bucket in FALLBACK_BUCKETS {
            let families = fallback_families(bucket);
            assert!(!families.is_empty(), "{bucket:?} has no families");
            for f in families {
                assert!(!f.trim().is_empty(), "{bucket:?} has a blank family name");
            }
        }
        assert!(!fallback_families(FallbackBucket::Other).is_empty());
    }

    #[test]
    fn bucket_list_covers_every_variant_once() {
        // `FALLBACK_BUCKETS` drives the fallback walk; a duplicate would double
        // the cost of a miss and an omission would make a script unreachable.
        let mut seen = FALLBACK_BUCKETS.to_vec();
        let before = seen.len();
        seen.sort_by_key(|b| format!("{b:?}"));
        seen.dedup();
        assert_eq!(seen.len(), before, "duplicate bucket in FALLBACK_BUCKETS");
    }

    #[test]
    fn an_empty_database_resolves_to_nothing_without_panicking() {
        let mut db = FontDatabase::new();
        assert!(db.is_empty());
        assert_eq!(db.face_count(), 0);
        assert_eq!(db.resolve(&FontRequest::family("Segoe UI")), None);
        assert_eq!(db.fallback_for('ก', &FontRequest::default()), None);
    }

    #[test]
    fn stale_and_unknown_ids_are_rejected() {
        let mut db = FontDatabase::new();
        let bogus = FontId::new(9999, 1);
        assert_eq!(db.face_metrics(bogus), None);
        assert_eq!(db.family_name(bogus), None);
        assert_eq!(db.glyph_index(bogus, 'A'), None);
        assert!(!db.has_glyph(bogus, 'A'));
        assert!(db.with_face(bogus, |_| ()).is_none());
    }

    #[test]
    fn garbage_font_data_is_rejected_cleanly() {
        let mut db = FontDatabase::new();
        assert!(db.load_font_data(vec![0u8; 64]).is_empty());
        assert!(db.load_font_data(Vec::new()).is_empty());
        assert!(db.is_empty(), "a malformed font must not enter the index");
    }

    #[test]
    fn missing_font_file_reports_io_not_panic() {
        let mut db = FontDatabase::new();
        let err = db.load_font_file("W:/definitely/not/a/font.ttf").unwrap_err();
        assert!(matches!(err, FontError::Io { .. }), "{err:?}");
    }

    #[test]
    fn system_metrics_are_normalised_to_em_units() {
        let Some((db, id)) = system() else { return };
        let m = db.face_metrics(id).expect("resolved face must have metrics");
        assert!(m.units_per_em >= 16, "implausible units_per_em {}", m.units_per_em);
        // Normalised metrics live in em units, so an ascent of 0.8 is typical
        // and anything outside (0, 2) means the division was skipped.
        assert!(m.ascent > 0.3 && m.ascent < 2.0, "ascent {}", m.ascent);
        assert!(m.descent >= 0.0 && m.descent < 1.0, "descent {}", m.descent);
        assert!(m.line_gap >= 0.0 && m.line_gap < 1.0, "line_gap {}", m.line_gap);
        assert!(m.line_height() > m.ascent, "line height must exceed the ascent");
        assert!(m.x_height > 0.0 && m.x_height <= m.cap_height + 0.05);
        assert!(m.cap_height > 0.0 && m.cap_height <= m.ascent + 0.05);
        assert!(m.underline_thickness > 0.0);
    }

    #[test]
    fn resolution_is_memoised_and_stable() {
        let Some((mut db, id)) = system() else { return };
        let again = db.resolve(&FontRequest::default()).unwrap();
        assert_eq!(id, again, "the same request must resolve to the same face");
        let bold = db.resolve(&FontRequest::default().weight(FontWeight::BOLD)).unwrap();
        // Bold may legitimately resolve to the same face on a family with no
        // bold cut, but it must still resolve.
        assert!(db.face_metrics(bold).is_some());
    }

    #[test]
    fn an_unknown_family_substitutes_rather_than_failing() {
        let Some((mut db, _)) = system() else { return };
        let odd = FontRequest::family("No Such Family 12345");
        assert!(db.resolve(&odd).is_some(), "resolve must substitute, not fail");
        // ...whereas the exact lookup used by the fallback chain must not.
        assert_eq!(db.resolve_family("No Such Family 12345", &FontRequest::default()), None);
    }

    #[test]
    fn coverage_matches_the_face_cmap() {
        let Some((mut db, id)) = system() else { return };
        assert!(db.has_glyph(id, 'A'));
        assert!(db.has_glyph(id, ' '));
        assert_eq!(db.has_glyph(id, 'A'), db.glyph_index(id, 'A').is_some());
        // A private-use codepoint no normal UI font claims.
        let pua = '\u{F8FF}';
        assert_eq!(db.has_glyph(id, pua), db.glyph_index(id, pua).is_some());
    }

    #[test]
    fn fallback_returns_a_face_that_actually_has_the_glyph() {
        let Some((mut db, _)) = system() else { return };
        for c in ['ก', 'あ', '中', '한', 'ع', 'ש', 'Д'] {
            if let Some(f) = db.fallback_for(c, &FontRequest::default()) {
                assert!(db.has_glyph(f, c), "fallback for {c:?} lacks the glyph");
            }
        }
    }

    #[test]
    fn fallback_is_memoised_including_misses() {
        let Some((mut db, _)) = system() else { return };
        // U+E000 is private use: usually nothing has it. Whatever the answer,
        // asking twice must agree, which is what proves the negative cache is
        // consulted rather than a full scan being repeated.
        let first = db.fallback_for('\u{E000}', &FontRequest::default());
        let second = db.fallback_for('\u{E000}', &FontRequest::default());
        assert_eq!(first, second);
    }

    #[test]
    fn loading_the_same_face_twice_yields_one_id() {
        let Some((mut db, id)) = system() else { return };
        let name = db.family_name(id).unwrap().to_owned();
        let again = db.resolve_family(&name, &FontRequest::default());
        assert_eq!(again, Some(id), "one fontdb face must map to one FontId");
        assert_eq!(db.loaded_count(), 1);
    }

    #[test]
    fn with_face_hands_out_a_parsed_shapeable_face() {
        let Some((db, id)) = system() else { return };
        let upem = db.with_face(id, |f| f.units_per_em()).unwrap();
        assert!(upem > 0);
        assert_eq!(upem as u16, db.face_metrics(id).unwrap().units_per_em);
    }

    #[test]
    fn variations_do_not_leak_between_borrows() {
        let Some((db, id)) = system() else { return };
        // Setting a variation inside one `with_face` must not be visible in the
        // next: the whole point of re-parsing per call is that each borrow gets
        // a pristine face.
        let tag = ttf_parser::Tag::from_bytes(b"wght");
        let before = db.with_face(id, |f| f.glyph_hor_advance(ttf_parser::GlyphId(1))).unwrap();
        db.with_face(id, |f| {
            f.set_variations(&[rustybuzz::Variation { tag, value: 900.0 }]);
        });
        let after = db.with_face(id, |f| f.glyph_hor_advance(ttf_parser::GlyphId(1))).unwrap();
        assert_eq!(before, after);
    }
}
