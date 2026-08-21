//! Script itemisation, bidi resolution and complex-script shaping.
//!
//! A paragraph goes through four stages before it has glyphs:
//!
//! 1. **Bidi.** [`unicode_bidi`] resolves an embedding level for every byte,
//!    from an explicit base direction or from the first strong character.
//! 2. **Script itemisation.** [`unicode_script`] splits the text into runs of
//!    one script. `Common` and `Inherited` characters — spaces, digits, most
//!    punctuation, combining marks — *continue* the surrounding run instead of
//!    starting a new one. Doing it the other way round splits `"a, b"` into
//!    five runs and destroys kerning and ligatures across every one of them.
//! 3. **Font itemisation.** A run splits again wherever fallback picks a
//!    different face, because a shaping call covers exactly one face.
//! 4. **Shaping.** [`rustybuzz`] turns each item into positioned glyphs.
//!
//! ## Cluster mapping
//!
//! Every [`ShapedGlyph`] carries the byte range of the *original* string it
//! came from, not of the run it was shaped in. That mapping is what caret
//! placement, selection and hit testing are built on, so the run-local cluster
//! values HarfBuzz produces are rebased before they leave this module, and the
//! end of each cluster is recovered from the neighbouring glyph — forwards for
//! left-to-right output, backwards for right-to-left, where HarfBuzz emits
//! glyphs already in visual order and cluster values therefore *descend*.

use core::ops::Range;

use rustybuzz::{BufferFlags, Direction, Feature, UnicodeBuffer, Variation};
use smallvec::SmallVec;
use sphere_core::{FontId, GlyphId, Point, Px};
use ttf_parser::Tag;
use unicode_bidi::{Level, ParagraphBidiInfo};
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

use crate::font::FontDatabase;
use crate::types::{ShapedGlyph, ShapedRun, TextDirection, TextStyle};

/// One itemised run: a maximal stretch of text sharing an embedding level, a
/// script and a face.
#[derive(Clone, Debug, PartialEq)]
pub struct RunItem {
    /// Byte range within the paragraph text that was itemised.
    pub range: Range<usize>,
    /// Bidi embedding level. Odd levels read right to left.
    pub level: u8,
    /// The resolved script, with `Common`/`Inherited` folded into whichever
    /// real script surrounds them.
    pub script: Script,
    /// The face every character in this run will be shaped with.
    pub font: FontId,
}

impl RunItem {
    /// True when the run reads right to left.
    #[inline]
    pub fn is_rtl(&self) -> bool {
        self.level % 2 == 1
    }
}

/// The paragraph embedding level implied by a string and a requested direction.
///
/// Cheap: for an explicit direction it does no work at all, and for
/// [`TextDirection::Auto`] it runs only rules P2/P3 rather than the whole
/// algorithm. A paragraph with no strong character resolves to left-to-right,
/// as P3 requires.
pub fn base_level(text: &str, direction: TextDirection) -> u8 {
    match direction {
        TextDirection::Ltr => 0,
        TextDirection::Rtl => 1,
        TextDirection::Auto => match unicode_bidi::get_base_direction(text) {
            unicode_bidi::Direction::Rtl => 1,
            _ => 0,
        },
    }
}

/// Splits a paragraph into runs of one level, script and face.
///
/// `levels` is the per-byte embedding level array produced by the bidi pass and
/// must be at least as long as `text`; anything past its end falls back to
/// `base`.
pub fn itemize(
    db: &mut FontDatabase,
    text: &str,
    levels: &[u8],
    base: u8,
    style: &TextStyle,
    primary: FontId,
) -> Vec<RunItem> {
    /// The run currently being extended.
    struct Pending {
        start: usize,
        end: usize,
        level: u8,
        script: Script,
        font: FontId,
        /// True while the run has only seen `Common`/`Inherited` characters and
        /// can therefore still adopt the first real script that arrives.
        neutral: bool,
    }

    let mut items: Vec<RunItem> = Vec::new();
    let mut pending: Option<Pending> = None;

    for (i, c) in text.char_indices() {
        let end = i + c.len_utf8();
        let level = levels.get(i).copied().unwrap_or(base);
        let raw = c.script();
        let inherits = matches!(raw, Script::Common | Script::Inherited | Script::Unknown);

        // Keeping a Common/Inherited character on the face the run already uses
        // is what stops a single space from cutting a Thai or CJK run in half
        // and handing the remainder back to the Latin primary.
        let current = pending.as_ref().map(|p| p.font);
        let font = match current {
            Some(f) if inherits && db.has_glyph(f, c) => f,
            _ if db.has_glyph(primary, c) => primary,
            _ => db.fallback_for(c, &style.font).unwrap_or(primary),
        };

        let extend = match &pending {
            Some(p) => {
                p.level == level && p.font == font && (inherits || p.neutral || p.script == raw)
            }
            None => false,
        };

        if extend {
            let p = pending.as_mut().expect("extend implies a pending run");
            p.end = end;
            if !inherits && p.neutral {
                p.script = raw;
                p.neutral = false;
            }
        } else {
            if let Some(p) = pending.take() {
                items.push(RunItem {
                    range: p.start..p.end,
                    level: p.level,
                    script: p.script,
                    font: p.font,
                });
            }
            pending = Some(Pending {
                start: i,
                end,
                level,
                script: if inherits { Script::Common } else { raw },
                font,
                neutral: inherits,
            });
        }
    }

    if let Some(p) = pending {
        items.push(RunItem {
            range: p.start..p.end,
            level: p.level,
            script: p.script,
            font: p.font,
        });
    }
    items
}

/// A paragraph shaped once, ready to be sliced into lines.
///
/// Shaping is the expensive half of layout, so it happens exactly once per
/// paragraph and every line is cut out of the result. That also makes line
/// widths and the widths used to choose break points come from the same
/// numbers, which is what stops a line that "measured as fitting" from
/// overflowing after it is assembled.
pub struct ShapedParagraph<'t> {
    /// The paragraph text: no line separators, no surrounding context.
    text: &'t str,
    /// Byte offset of `text` within the string the caller passed in. Every
    /// range in the public API is in *that* coordinate space.
    offset: usize,
    /// Bidi analysis of `text`, kept so line assembly can apply rules L1 and L2
    /// per line rather than per paragraph.
    bidi: ParagraphBidiInfo<'t>,
    /// Runs in logical order. Glyph `position.x` holds only the glyph's own
    /// shaping offset here; the pen position is added when a line is built.
    runs: Vec<ShapedRun>,
    /// Cumulative advance, in logical pixels, at each byte offset of `text`.
    /// `cumulative[i]` is the total advance of every cluster starting before
    /// `i`, so the width of a range is one subtraction.
    cumulative: Vec<f32>,
    /// The paragraph embedding level.
    base_level: u8,
}

impl core::fmt::Debug for ShapedParagraph<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ShapedParagraph")
            .field("range", &self.range())
            .field("base_level", &self.base_level)
            .field("runs", &self.runs.len())
            .finish()
    }
}

impl<'t> ShapedParagraph<'t> {
    /// Shapes one paragraph.
    ///
    /// `text` must not contain a line separator: hard breaks are the caller's
    /// business, because they also decide whether the break ends a paragraph
    /// for bidi purposes. `offset` is where `text` starts inside the original
    /// string and is added to every byte range this type reports.
    pub fn shape(
        db: &mut FontDatabase,
        text: &'t str,
        offset: usize,
        style: &TextStyle,
        primary: FontId,
    ) -> Self {
        let forced = match style.direction {
            TextDirection::Auto => None,
            TextDirection::Ltr => Some(Level::ltr()),
            TextDirection::Rtl => Some(Level::rtl()),
        };
        let bidi = ParagraphBidiInfo::new(text, forced);
        let base_level = bidi.paragraph_level.number();

        let levels: Vec<u8> = bidi.levels.iter().map(|l| l.number()).collect();
        let items = itemize(db, text, &levels, base_level, style, primary);

        let mut runs = Vec::with_capacity(items.len());
        for item in &items {
            if let Some(run) = shape_item(db, text, offset, item, style) {
                runs.push(run);
            }
        }

        let cumulative = build_cumulative(text.len(), offset, &runs);
        Self { text, offset, bidi, runs, cumulative, base_level }
    }

    /// The paragraph text.
    #[inline]
    pub fn text(&self) -> &'t str {
        self.text
    }

    /// The paragraph's byte range in the original string.
    #[inline]
    pub fn range(&self) -> Range<usize> {
        self.offset..self.offset + self.text.len()
    }

    /// The paragraph embedding level; odd means right to left.
    #[inline]
    pub fn base_level(&self) -> u8 {
        self.base_level
    }

    /// True when the paragraph reads right to left.
    #[inline]
    pub fn is_rtl(&self) -> bool {
        self.base_level % 2 == 1
    }

    /// Total glyph count, for diagnostics and tests.
    pub fn glyph_count(&self) -> usize {
        self.runs.iter().map(|r| r.glyphs.len()).sum()
    }

    /// The advance of a byte range, in original-string coordinates.
    ///
    /// Exact for range ends that fall on a cluster boundary, which every break
    /// opportunity does. Out-of-range values are clamped rather than panicking,
    /// so a caller cannot turn an off-by-one into a crash mid-layout.
    pub fn advance(&self, range: Range<usize>) -> Px {
        let (a, b) = self.local(range);
        Px(self.cumulative[b] - self.cumulative[a])
    }

    /// Builds the runs of one line, in visual order, with pen positions
    /// assigned from `x = 0`.
    ///
    /// Level runs come from [`unicode_bidi`], so rule L1 (resetting trailing
    /// whitespace to the paragraph level) and rule L2 (reversing nested
    /// right-to-left sequences) are both applied per line rather than per
    /// paragraph — which is the only way `"abc עברית"` and `"עברית abc"` can
    /// both come out right.
    pub fn line_runs(&self, range: Range<usize>) -> Vec<ShapedRun> {
        let (a, b) = self.local(range);
        if a >= b {
            return Vec::new();
        }
        let (levels, level_runs) = self.bidi.visual_runs(a..b);

        let mut out: Vec<ShapedRun> = Vec::new();
        for lr in level_runs {
            let rtl = levels[lr.start].is_rtl();
            let lo = lr.start + self.offset;
            let hi = lr.end + self.offset;
            let first = out.len();
            out.extend(self.runs.iter().filter_map(|r| slice_run(r, lo..hi)));
            if rtl {
                // Within a right-to-left level run the *later* logical piece is
                // drawn further left, so the script/font sub-runs reverse too.
                out[first..].reverse();
            }
        }
        assign_positions(&mut out);
        out
    }

    /// Clamps a range in original coordinates to paragraph-local bounds.
    ///
    /// The ends are also snapped outwards to character boundaries: `unicode_bidi`
    /// slices the paragraph text to apply rule L1, so a range landing inside a
    /// multi-byte character would panic there rather than in anything this
    /// module can see. Snapping cannot change a measured advance, because every
    /// cluster starts on a character boundary and the prefix-sum table is flat
    /// between them.
    fn local(&self, range: Range<usize>) -> (usize, usize) {
        let n = self.text.len();
        let mut a = range.start.saturating_sub(self.offset).min(n);
        while a > 0 && !self.text.is_char_boundary(a) {
            a -= 1;
        }
        let mut b = range.end.saturating_sub(self.offset).clamp(a, n);
        while b < n && !self.text.is_char_boundary(b) {
            b += 1;
        }
        (a, b)
    }
}

/// Shifts every glyph in a set of runs horizontally.
///
/// Used by layout for alignment and for pushing an ellipsis to the end of a
/// line; kept here because it has to agree with how [`ShapedParagraph`]
/// assigns positions in the first place.
pub fn offset_runs(runs: &mut [ShapedRun], dx: Px) {
    if dx == Px::ZERO {
        return;
    }
    for run in runs {
        for g in &mut run.glyphs {
            g.position.x += dx;
        }
    }
}

/// Total advance of a set of runs.
pub fn total_advance(runs: &[ShapedRun]) -> Px {
    runs.iter().flat_map(|r| r.glyphs.iter()).map(|g| g.advance).sum()
}

// ---- internals -----------------------------------------------------------

/// Turns the glyph offsets stored during shaping into pen positions.
fn assign_positions(runs: &mut [ShapedRun]) -> Px {
    let mut pen = 0.0f32;
    for run in runs.iter_mut() {
        for g in run.glyphs.iter_mut() {
            g.position.x = Px(pen + g.position.x.get());
            pen += g.advance.get();
        }
    }
    Px(pen)
}

/// Extracts the part of a run that falls inside a byte range.
///
/// Membership is decided by `cluster.start` alone, so a cluster that straddles
/// the boundary — a ligature broken by a line break, say — belongs wholly to
/// the earlier slice and no glyph is ever duplicated or dropped.
fn slice_run(run: &ShapedRun, range: Range<usize>) -> Option<ShapedRun> {
    if run.source.start >= range.end || run.source.end <= range.start {
        return None;
    }
    let glyphs: Vec<ShapedGlyph> = run
        .glyphs
        .iter()
        .filter(|g| g.cluster.start >= range.start && g.cluster.start < range.end)
        .cloned()
        .collect();
    if glyphs.is_empty() {
        return None;
    }
    let source = run.source.start.max(range.start)..run.source.end.min(range.end);
    Some(ShapedRun { font: run.font, font_size: run.font_size, rtl: run.rtl, glyphs, source })
}

/// Builds the prefix-sum advance table a paragraph is measured with.
fn build_cumulative(len: usize, offset: usize, runs: &[ShapedRun]) -> Vec<f32> {
    let mut cumulative = vec![0.0f32; len + 1];
    // Charge each cluster's whole advance to its first byte, so that summing up
    // to a cluster boundary is exact.
    for run in runs {
        for g in &run.glyphs {
            let i = g.cluster.start.saturating_sub(offset);
            if i < len {
                cumulative[i] += g.advance.get();
            }
        }
    }
    // In-place exclusive prefix sum: slot `i` ends up holding the advance of
    // everything before byte `i`, and the final slot the paragraph total.
    let mut acc = 0.0f32;
    for slot in cumulative.iter_mut() {
        let here = *slot;
        *slot = acc;
        acc += here;
    }
    cumulative
}

/// Shapes one itemised run.
fn shape_item(
    db: &mut FontDatabase,
    text: &str,
    offset: usize,
    item: &RunItem,
    style: &TextStyle,
) -> Option<ShapedRun> {
    let sub = &text[item.range.clone()];
    if sub.is_empty() {
        return None;
    }
    let rtl = item.is_rtl();

    let features: SmallVec<[Feature; 8]> = style
        .features
        .iter()
        .map(|(tag, value)| Feature::new(Tag::from_bytes(tag), *value, ..))
        .collect();
    let variations: SmallVec<[Variation; 4]> = style
        .variations
        .iter()
        .map(|v| Variation { tag: Tag::from_bytes(&v.tag), value: v.value })
        .collect();

    let mut flags = BufferFlags::empty();
    if item.range.start == 0 {
        flags |= BufferFlags::BEGINNING_OF_TEXT;
    }
    if item.range.end == text.len() {
        flags |= BufferFlags::END_OF_TEXT;
    }

    let font_size = style.font_size.get();
    let base = offset + item.range.start;

    let mut glyphs = db.with_face(item.font, |face| {
        if !variations.is_empty() {
            face.set_variations(&variations);
        }
        // Optical sizing in Apple fonts keys off the point size; feeding it the
        // real one costs nothing and is wrong to omit.
        face.set_points_per_em(Some(font_size));

        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(sub);
        buffer.set_direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight });
        if let Some(script) = harfbuzz_script(item.script) {
            buffer.set_script(script);
        }
        if let Some(lang) = language_for(item.script) {
            buffer.set_language(lang);
        }
        buffer.set_flags(flags);

        let shaped = rustybuzz::shape(face, &features, buffer);
        let upem = face.units_per_em().max(1) as f32;
        let scale = font_size / upem;

        let infos = shaped.glyph_infos();
        let positions = shaped.glyph_positions();
        let n = infos.len().min(positions.len());

        let mut out: Vec<ShapedGlyph> = Vec::with_capacity(n);
        for i in 0..n {
            let start = infos[i].cluster as usize;
            out.push(ShapedGlyph {
                glyph: GlyphId(infos[i].glyph_id as u16),
                font: item.font,
                // HarfBuzz measures y upward from the baseline; Sphere's screen
                // space runs downward, hence the negation.
                position: Point::new(
                    Px(positions[i].x_offset as f32 * scale),
                    Px(-(positions[i].y_offset as f32) * scale),
                ),
                advance: Px(positions[i].x_advance as f32 * scale),
                cluster: (base + start)..(base + start),
                rtl,
            });
        }
        fill_cluster_ends(&mut out, base, sub.len(), rtl);
        out
    })?;

    if glyphs.is_empty() {
        return None;
    }
    apply_spacing(&mut glyphs, sub, base, style);

    Some(ShapedRun {
        font: item.font,
        font_size: style.font_size,
        rtl,
        glyphs,
        source: base..(offset + item.range.end),
    })
}

/// Recovers the end of every cluster from its neighbours.
///
/// HarfBuzz reports only where a cluster *starts*. A cluster ends where the
/// next one begins, and "next" means the following glyph for left-to-right
/// output but the *preceding* glyph for right-to-left, where output is already
/// in visual order and cluster values descend.
fn fill_cluster_ends(glyphs: &mut [ShapedGlyph], base: usize, sub_len: usize, rtl: bool) {
    let end_of_text = base + sub_len;
    let mut previous_start: Option<usize> = None;
    let mut current_end = end_of_text;

    let n = glyphs.len();
    for k in 0..n {
        // Walk in the direction cluster values *ascend*: forwards for
        // right-to-left output, backwards for left-to-right.
        let i = if rtl { k } else { n - 1 - k };
        let start = glyphs[i].cluster.start;
        if previous_start != Some(start) {
            current_end = previous_start.unwrap_or(end_of_text);
            previous_start = Some(start);
        }
        // Clamp defensively: a font with non-monotone clusters would otherwise
        // produce an inverted range that panics the first time it is used to
        // slice the source string.
        glyphs[i].cluster.end = current_end.max(start).min(end_of_text);
    }
}

/// Applies `letter_spacing` and `word_spacing` to already-shaped glyphs.
///
/// Letter spacing is added *after* a grapheme cluster, never inside one. Two
/// consequences matter:
///
/// * A base letter and its combining marks share one cluster, so the mark never
///   drifts away from the letter it belongs to.
/// * A ligature is a single glyph whose cluster spans several graphemes, and
///   spacing lands after the whole ligature rather than inside it, because
///   there is no "inside" to put it in. This is also why the check is on the
///   cluster's *end* rather than on each character.
///
/// Word spacing is added once per cluster that contains a space.
fn apply_spacing(glyphs: &mut [ShapedGlyph], sub: &str, base: usize, style: &TextStyle) {
    let letter = style.letter_spacing.get();
    let word = style.word_spacing.get();
    if letter == 0.0 && word == 0.0 {
        return;
    }

    // Only pay for the grapheme scan when letter spacing is actually in play.
    let boundaries: SmallVec<[usize; 64]> = if letter != 0.0 {
        sub.grapheme_indices(true).map(|(i, _)| i).chain(core::iter::once(sub.len())).collect()
    } else {
        SmallVec::new()
    };

    let mut i = 0;
    while i < glyphs.len() {
        let start = glyphs[i].cluster.start;
        let end = glyphs[i].cluster.end;
        let mut j = i + 1;
        while j < glyphs.len() && glyphs[j].cluster.start == start {
            j += 1;
        }

        let mut extra = 0.0;
        if letter != 0.0 && boundaries.binary_search(&(end - base)).is_ok() {
            extra += letter;
        }
        if word != 0.0 {
            let lo = start - base;
            let hi = (end - base).min(sub.len());
            if lo < hi && sub[lo..hi].contains(' ') {
                extra += word;
            }
        }
        if extra != 0.0 {
            glyphs[j - 1].advance += Px(extra);
        }
        i = j;
    }
}

/// Maps a Unicode script to the tag HarfBuzz wants.
fn harfbuzz_script(script: Script) -> Option<rustybuzz::Script> {
    let short = script.short_name().as_bytes();
    let tag: [u8; 4] = short.try_into().ok()?;
    rustybuzz::Script::from_iso15924_tag(Tag::from_bytes(&tag))
}

/// A language tag for a script, where the script implies one.
///
/// Only scripts with a single dominant language get a tag. Guessing `en` for
/// everything would be worse than guessing nothing: language-sensitive features
/// such as Turkish dotless-i or Serbian Cyrillic italics key off this, and a
/// wrong tag actively selects the wrong glyphs where an absent one does not.
fn language_for(script: Script) -> Option<rustybuzz::Language> {
    use core::str::FromStr;
    let tag = match script {
        Script::Thai => "th",
        Script::Hiragana | Script::Katakana => "ja",
        Script::Hangul => "ko",
        Script::Hebrew => "he",
        Script::Arabic => "ar",
        Script::Devanagari => "hi",
        Script::Greek => "el",
        Script::Armenian => "hy",
        Script::Georgian => "ka",
        Script::Khmer => "km",
        Script::Lao => "lo",
        Script::Myanmar => "my",
        _ => return None,
    };
    rustybuzz::Language::from_str(tag).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FontRequest, TextAlign, WrapMode};
    use sphere_core::px;

    fn style() -> TextStyle {
        TextStyle { font_size: px(16.0), ..Default::default() }
    }

    /// A system database plus its default face, or `None` on a bare machine.
    fn system() -> Option<(FontDatabase, FontId)> {
        let mut db = FontDatabase::with_system_fonts();
        let id = db.resolve(&FontRequest::default())?;
        Some((db, id))
    }

    #[test]
    fn base_level_follows_the_first_strong_character() {
        assert_eq!(base_level("abc", TextDirection::Auto), 0);
        assert_eq!(base_level("עברית", TextDirection::Auto), 1);
        assert_eq!(base_level("العربية", TextDirection::Auto), 1);
        // Leading neutrals are skipped, not treated as strong.
        assert_eq!(base_level("  ...עברית", TextDirection::Auto), 1);
        assert_eq!(base_level("123 abc", TextDirection::Auto), 0);
        // A paragraph with no strong character is left-to-right, per rule P3.
        assert_eq!(base_level("123 ...", TextDirection::Auto), 0);
        assert_eq!(base_level("", TextDirection::Auto), 0);
    }

    #[test]
    fn explicit_direction_overrides_content() {
        assert_eq!(base_level("עברית", TextDirection::Ltr), 0);
        assert_eq!(base_level("abc", TextDirection::Rtl), 1);
    }

    #[test]
    fn harfbuzz_script_tags_round_trip() {
        assert_eq!(harfbuzz_script(Script::Latin), Some(rustybuzz::script::LATIN));
        assert_eq!(harfbuzz_script(Script::Thai), Some(rustybuzz::script::THAI));
        assert_eq!(harfbuzz_script(Script::Arabic), Some(rustybuzz::script::ARABIC));
        assert_eq!(harfbuzz_script(Script::Han), Some(rustybuzz::script::HAN));
        assert_eq!(harfbuzz_script(Script::Hiragana), Some(rustybuzz::script::HIRAGANA));
    }

    #[test]
    fn language_is_only_guessed_where_a_script_implies_one() {
        assert!(language_for(Script::Thai).is_some());
        assert!(language_for(Script::Hiragana).is_some());
        // Latin and Cyrillic are shared by dozens of languages; guessing is
        // worse than leaving the tag unset.
        assert!(language_for(Script::Latin).is_none());
        assert!(language_for(Script::Cyrillic).is_none());
        assert!(language_for(Script::Common).is_none());
    }

    #[test]
    fn cumulative_table_is_a_prefix_sum() {
        let run = ShapedRun {
            font: FontId::new(0, 1),
            font_size: px(10.0),
            rtl: false,
            glyphs: vec![
                ShapedGlyph {
                    glyph: GlyphId(1),
                    font: FontId::new(0, 1),
                    position: Point::new(Px::ZERO, Px::ZERO),
                    advance: px(5.0),
                    cluster: 0..1,
                    rtl: false,
                },
                ShapedGlyph {
                    glyph: GlyphId(2),
                    font: FontId::new(0, 1),
                    position: Point::new(Px::ZERO, Px::ZERO),
                    advance: px(7.0),
                    cluster: 1..3,
                    rtl: false,
                },
            ],
            source: 0..3,
        };
        let c = build_cumulative(3, 0, core::slice::from_ref(&run));
        assert_eq!(c, vec![0.0, 5.0, 12.0, 12.0]);
    }

    #[test]
    fn slicing_a_run_never_duplicates_or_drops_a_glyph() {
        let mk = |cluster: Range<usize>| ShapedGlyph {
            glyph: GlyphId(1),
            font: FontId::new(0, 1),
            position: Point::new(Px::ZERO, Px::ZERO),
            advance: px(4.0),
            cluster,
            rtl: false,
        };
        let run = ShapedRun {
            font: FontId::new(0, 1),
            font_size: px(10.0),
            rtl: false,
            glyphs: vec![mk(0..1), mk(1..2), mk(2..4)],
            source: 0..4,
        };
        let left = slice_run(&run, 0..2).unwrap();
        let right = slice_run(&run, 2..4).unwrap();
        assert_eq!(left.glyphs.len(), 2);
        assert_eq!(right.glyphs.len(), 1);
        assert_eq!(left.glyphs.len() + right.glyphs.len(), run.glyphs.len());
        assert_eq!(left.source, 0..2);
        assert_eq!(right.source, 2..4);
        // A range that touches no cluster start yields nothing rather than an
        // empty run that would later be positioned.
        assert!(slice_run(&run, 4..4).is_none());
    }

    #[test]
    fn cluster_ends_are_recovered_in_both_directions() {
        let mk = |start: usize| ShapedGlyph {
            glyph: GlyphId(1),
            font: FontId::new(0, 1),
            position: Point::new(Px::ZERO, Px::ZERO),
            advance: px(1.0),
            cluster: start..start,
            rtl: false,
        };
        // Left to right: clusters ascend, so each ends where the next starts.
        let mut ltr = vec![mk(0), mk(1), mk(1), mk(3)];
        fill_cluster_ends(&mut ltr, 0, 5, false);
        assert_eq!(ltr[0].cluster, 0..1);
        assert_eq!(ltr[1].cluster, 1..3);
        assert_eq!(ltr[2].cluster, 1..3);
        assert_eq!(ltr[3].cluster, 3..5);

        // Right to left: HarfBuzz emits visual order, so clusters descend.
        let mut rtl = vec![mk(3), mk(1), mk(1), mk(0)];
        fill_cluster_ends(&mut rtl, 0, 5, true);
        assert_eq!(rtl[0].cluster, 3..5);
        assert_eq!(rtl[1].cluster, 1..3);
        assert_eq!(rtl[2].cluster, 1..3);
        assert_eq!(rtl[3].cluster, 0..1);
    }

    #[test]
    fn cluster_ends_survive_a_rebased_run() {
        let mk = |start: usize| ShapedGlyph {
            glyph: GlyphId(1),
            font: FontId::new(0, 1),
            position: Point::new(Px::ZERO, Px::ZERO),
            advance: px(1.0),
            cluster: start..start,
            rtl: false,
        };
        // A run starting at byte 10 of a two-byte slice must end at 12, not 2.
        let mut g = vec![mk(10), mk(11)];
        fill_cluster_ends(&mut g, 10, 2, false);
        assert_eq!(g[0].cluster, 10..11);
        assert_eq!(g[1].cluster, 11..12);
    }

    #[test]
    fn common_characters_do_not_split_a_run() {
        let Some((mut db, id)) = system() else { return };
        let text = "a, b. c";
        let levels = vec![0u8; text.len()];
        let items = itemize(&mut db, text, &levels, 0, &style(), id);
        assert_eq!(items.len(), 1, "punctuation and spaces split the run: {items:?}");
        assert_eq!(items[0].range, 0..text.len());
        assert_eq!(items[0].script, Script::Latin);
    }

    #[test]
    fn a_leading_neutral_run_adopts_the_first_real_script() {
        let Some((mut db, id)) = system() else { return };
        let text = "  abc";
        let levels = vec![0u8; text.len()];
        let items = itemize(&mut db, text, &levels, 0, &style(), id);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].script, Script::Latin, "leading spaces stranded the run as Common");
    }

    #[test]
    fn script_change_starts_a_new_item() {
        let Some((mut db, id)) = system() else { return };
        let text = "abc ไทย";
        let levels = vec![0u8; text.len()];
        let items = itemize(&mut db, text, &levels, 0, &style(), id);
        assert!(items.len() >= 2, "Latin and Thai must not share a run: {items:?}");
        let scripts: Vec<Script> = items.iter().map(|i| i.script).collect();
        assert!(scripts.contains(&Script::Thai), "{scripts:?}");
        // The whole string is covered exactly once, in order.
        let mut at = 0;
        for item in &items {
            assert_eq!(item.range.start, at);
            at = item.range.end;
        }
        assert_eq!(at, text.len());
    }

    #[test]
    fn level_change_starts_a_new_item() {
        let Some((mut db, id)) = system() else { return };
        let text = "abcdef";
        let mut levels = vec![0u8; text.len()];
        levels[3..].fill(1);
        let items = itemize(&mut db, text, &levels, 0, &style(), id);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].range, 0..3);
        assert_eq!(items[1].range, 3..6);
        assert!(items[1].is_rtl());
    }

    #[test]
    fn ascii_shaping_produces_one_glyph_per_character() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        let para = ShapedParagraph::shape(&mut db, "Hello", 0, &s, id);
        assert_eq!(para.glyph_count(), 5);
        let runs = para.line_runs(0..5);
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].rtl);
        let w = total_advance(&runs);
        // 16 px text: five characters cannot be free, nor five ems wide.
        assert!(w > px(10.0) && w < px(80.0), "{w:?}");
        assert_eq!(w, para.advance(0..5));
    }

    #[test]
    fn cluster_ranges_are_monotonic_and_cover_the_string() {
        let Some((mut db, id)) = system() else { return };
        let text = "Hello, world!";
        let s = style();
        let para = ShapedParagraph::shape(&mut db, text, 0, &s, id);
        let runs = para.line_runs(0..text.len());
        let mut covered = vec![false; text.len()];
        for run in &runs {
            for g in &run.glyphs {
                assert!(g.cluster.start <= g.cluster.end, "inverted cluster {:?}", g.cluster);
                assert!(g.cluster.end <= text.len());
                for b in g.cluster.clone() {
                    covered[b] = true;
                }
            }
        }
        assert!(covered.iter().all(|b| *b), "some bytes map to no glyph");
    }

    #[test]
    fn clusters_are_rebased_onto_the_original_string() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        // Paragraph starting at byte 7 of a larger document.
        let para = ShapedParagraph::shape(&mut db, "abc", 7, &s, id);
        assert_eq!(para.range(), 7..10);
        let runs = para.line_runs(7..10);
        let first = &runs[0].glyphs[0];
        assert_eq!(first.cluster.start, 7, "cluster left in run-local coordinates");
        assert_eq!(runs[0].source, 7..10);
    }

    #[test]
    fn positions_advance_monotonically_in_an_ltr_line() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        let para = ShapedParagraph::shape(&mut db, "abcdef", 0, &s, id);
        let runs = para.line_runs(0..6);
        let mut last = -1.0f32;
        for g in runs.iter().flat_map(|r| r.glyphs.iter()) {
            assert!(g.position.x.get() > last, "positions must increase left to right");
            last = g.position.x.get();
        }
    }

    #[test]
    fn an_rtl_run_is_laid_out_right_to_left() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        let text = "עברית";
        let para = ShapedParagraph::shape(&mut db, text, 0, &s, id);
        assert!(para.is_rtl(), "a Hebrew paragraph must resolve to RTL");
        let runs = para.line_runs(0..text.len());
        assert!(runs.iter().all(|r| r.rtl));
        // Visual order means x increases while the source index decreases.
        let glyphs: Vec<&ShapedGlyph> = runs.iter().flat_map(|r| r.glyphs.iter()).collect();
        if glyphs.len() >= 2 {
            assert!(
                glyphs[0].cluster.start > glyphs[glyphs.len() - 1].cluster.start,
                "RTL glyphs were not reordered into visual order"
            );
        }
    }

    #[test]
    fn mixed_ltr_and_rtl_come_out_in_visual_order() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        let text = "abc العربية def";
        let para = ShapedParagraph::shape(&mut db, text, 0, &s, id);
        assert!(!para.is_rtl(), "the first strong character is Latin");
        let runs = para.line_runs(0..text.len());
        let glyphs: Vec<&ShapedGlyph> = runs.iter().flat_map(|r| r.glyphs.iter()).collect();
        assert!(!glyphs.is_empty());

        // Leftmost glyph is the 'a'; rightmost is the final 'f'.
        assert_eq!(glyphs[0].cluster.start, 0, "the Latin prefix must be leftmost");
        assert_eq!(
            glyphs[glyphs.len() - 1].cluster.end,
            text.len(),
            "the Latin suffix must be rightmost"
        );

        // The Arabic middle must itself read right to left: within it, x rises
        // as the source index falls.
        let arabic = text.find('ا').unwrap();
        let arabic_end = text.rfind(' ').unwrap();
        let inner: Vec<&&ShapedGlyph> = glyphs
            .iter()
            .filter(|g| g.cluster.start >= arabic && g.cluster.start < arabic_end)
            .collect();
        assert!(inner.len() >= 2, "no Arabic glyphs were produced");
        assert!(
            inner[0].cluster.start > inner[inner.len() - 1].cluster.start,
            "the Arabic segment was not reversed"
        );
        // ...and every Arabic glyph sits between the Latin ones.
        let left = glyphs[0].position.x;
        let right = glyphs[glyphs.len() - 1].position.x;
        for g in &inner {
            assert!(g.position.x > left && g.position.x < right, "Arabic escaped the middle");
        }
    }

    #[test]
    fn rtl_paragraph_with_embedded_latin_keeps_the_latin_ltr() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        let text = "עברית abc עברית";
        let para = ShapedParagraph::shape(&mut db, text, 0, &s, id);
        assert!(para.is_rtl());
        let runs = para.line_runs(0..text.len());
        let latin_start = text.find('a').unwrap();
        let latin: Vec<&ShapedGlyph> = runs
            .iter()
            .flat_map(|r| r.glyphs.iter())
            .filter(|g| g.cluster.start >= latin_start && g.cluster.start < latin_start + 3)
            .collect();
        assert_eq!(latin.len(), 3);
        // Inside an RTL paragraph the Latin island still reads left to right.
        assert!(latin[0].cluster.start < latin[2].cluster.start);
        assert!(latin[0].position.x < latin[2].position.x);
    }

    #[test]
    fn letter_spacing_widens_by_one_gap_per_grapheme() {
        let Some((mut db, id)) = system() else { return };
        let text = "abcd";
        let plain = ShapedParagraph::shape(&mut db, text, 0, &style(), id).advance(0..4);
        let spaced = TextStyle { letter_spacing: px(3.0), ..style() };
        let wide = ShapedParagraph::shape(&mut db, text, 0, &spaced, id).advance(0..4);
        assert!((wide - plain - px(12.0)).abs() < px(0.01), "{plain:?} -> {wide:?}");
    }

    #[test]
    fn letter_spacing_lands_after_a_grapheme_not_inside_it() {
        let Some((mut db, id)) = system() else { return };
        // "e" + combining acute is one grapheme, so it gets one gap, not two.
        let text = "e\u{0301}x";
        let plain = ShapedParagraph::shape(&mut db, text, 0, &style(), id).advance(0..text.len());
        let spaced = TextStyle { letter_spacing: px(5.0), ..style() };
        let wide = ShapedParagraph::shape(&mut db, text, 0, &spaced, id).advance(0..text.len());
        let gaps = ((wide - plain).get() / 5.0).round() as i32;
        assert_eq!(gaps, 2, "expected one gap per grapheme, got {gaps}");
    }

    #[test]
    fn word_spacing_only_touches_spaces() {
        let Some((mut db, id)) = system() else { return };
        let text = "a b c";
        let plain = ShapedParagraph::shape(&mut db, text, 0, &style(), id).advance(0..5);
        let spaced = TextStyle { word_spacing: px(4.0), ..style() };
        let wide = ShapedParagraph::shape(&mut db, text, 0, &spaced, id).advance(0..5);
        assert!((wide - plain - px(8.0)).abs() < px(0.01), "{plain:?} -> {wide:?}");
    }

    #[test]
    fn spacing_is_a_no_op_when_zero() {
        let Some((mut db, id)) = system() else { return };
        let text = "hello";
        let a = ShapedParagraph::shape(&mut db, text, 0, &style(), id).advance(0..5);
        let s = TextStyle { letter_spacing: Px::ZERO, word_spacing: Px::ZERO, ..style() };
        let b = ShapedParagraph::shape(&mut db, text, 0, &s, id).advance(0..5);
        assert_eq!(a, b);
    }

    #[test]
    fn thai_shapes_through_fallback_without_tofu() {
        let Some((mut db, id)) = system() else { return };
        let text = "สวัสดีครับ";
        let s = style();
        let para = ShapedParagraph::shape(&mut db, text, 0, &s, id);
        let runs = para.line_runs(0..text.len());
        assert!(!runs.is_empty(), "Thai produced no runs");
        let glyphs: Vec<&ShapedGlyph> = runs.iter().flat_map(|r| r.glyphs.iter()).collect();
        assert!(!glyphs.is_empty());
        // Glyph 0 is `.notdef` in every TrueType font: an all-tofu result means
        // fallback failed to find a Thai-capable face.
        assert!(glyphs.iter().any(|g| g.glyph.0 != 0), "Thai fell back to .notdef");
        assert!(para.advance(0..text.len()) > Px::ZERO);
    }

    #[test]
    fn japanese_shapes_through_fallback_without_tofu() {
        let Some((mut db, id)) = system() else { return };
        let text = "こんにちは世界";
        let s = style();
        let para = ShapedParagraph::shape(&mut db, text, 0, &s, id);
        let runs = para.line_runs(0..text.len());
        let glyphs: Vec<&ShapedGlyph> = runs.iter().flat_map(|r| r.glyphs.iter()).collect();
        assert!(!glyphs.is_empty());
        assert!(glyphs.iter().any(|g| g.glyph.0 != 0), "Japanese fell back to .notdef");
        // CJK is close to square, so seven characters at 16 px cannot be narrow.
        assert!(para.advance(0..text.len()) > px(50.0));
    }

    #[test]
    fn empty_and_degenerate_input_is_handled() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        let para = ShapedParagraph::shape(&mut db, "", 0, &s, id);
        assert_eq!(para.glyph_count(), 0);
        assert_eq!(para.advance(0..0), Px::ZERO);
        assert!(para.line_runs(0..0).is_empty());
        // Out-of-range queries clamp rather than panic.
        assert!(para.line_runs(0..99).is_empty());
        assert_eq!(para.advance(50..99), Px::ZERO);
    }

    #[test]
    fn out_of_range_slices_are_clamped_not_panicking() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        let para = ShapedParagraph::shape(&mut db, "abc", 100, &s, id);
        assert_eq!(para.advance(0..1000), para.advance(100..103));
        assert!(para.advance(0..50) == Px::ZERO);
        assert_eq!(para.line_runs(0..1000).len(), 1);
    }

    #[test]
    fn a_range_inside_a_character_is_snapped_not_panicked_on() {
        let Some((mut db, id)) = system() else { return };
        let s = style();
        // Every character here is three bytes, so 1, 2, 4 and 5 all land inside
        // one. `unicode_bidi` slices the text when it applies rule L1, so an
        // unsnapped range panics there.
        let text = "こんにちは";
        let para = ShapedParagraph::shape(&mut db, text, 0, &s, id);
        let whole = para.advance(0..text.len());
        for (a, b) in [(1usize, 5usize), (0, 1), (2, 3), (1, 2), (4, text.len()), (0, 14)] {
            let runs = para.line_runs(a..b);
            assert!(runs.iter().all(|r| !r.glyphs.is_empty()));
            assert!(para.advance(a..b) <= whole);
        }
        // Snapping must not change what a boundary-aligned query measures.
        assert_eq!(para.advance(0..3), para.advance(0..1));
        assert_eq!(para.advance(0..text.len()), whole);
    }

    #[test]
    fn style_knobs_that_do_not_affect_shaping_are_ignored() {
        // A guard against accidentally letting layout-only fields leak into the
        // shaping key: alignment and wrap must not change glyph advances.
        let Some((mut db, id)) = system() else { return };
        let a = ShapedParagraph::shape(&mut db, "hello", 0, &style(), id).advance(0..5);
        let s = TextStyle { align: TextAlign::Center, wrap: WrapMode::Grapheme, ..style() };
        let b = ShapedParagraph::shape(&mut db, "hello", 0, &s, id).advance(0..5);
        assert_eq!(a, b);
    }
}
