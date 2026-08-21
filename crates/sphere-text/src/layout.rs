//! Line breaking and paragraph layout.
//!
//! [`layout_text`] turns a string plus a [`TextStyle`] into a [`TextLayout`]:
//! lines of visually ordered runs, with baselines, widths and byte ranges that
//! together cover the whole input.
//!
//! ## Shape once, cut many
//!
//! Each paragraph is shaped exactly once, in logical order, and every line is
//! sliced out of that single result. The alternative — shaping a candidate
//! prefix for each break opportunity — is quadratic, and shaping each line
//! again after breaking makes the width a line was *chosen* for differ from the
//! width it *ends up* with, which is how a line that "fits" overflows by a
//! fraction of a pixel. Here both numbers come from the same glyph advances.
//!
//! ## Breaking
//!
//! Hard breaks are found first, because they also end a bidi paragraph. Within
//! a paragraph, [`WrapMode::Word`] uses UAX #14 break opportunities,
//! [`WrapMode::Grapheme`] uses UAX #29 boundaries — which is the only way to
//! wrap Thai or Japanese, where words are not separated by spaces — and
//! [`WrapMode::None`] produces one line however wide it gets.
//!
//! The greedy loop is written so that it *cannot* fail to advance: the chosen
//! end is always a break opportunity strictly greater than the line start, so a
//! single word wider than the box overflows its line rather than looping
//! forever. That bug is the classic one in this algorithm and it is tested for.

use core::ops::Range;

use sphere_core::{Px, Size};
use unicode_segmentation::UnicodeSegmentation;

use crate::font::FontDatabase;
use crate::shape::{ShapedParagraph, offset_runs, total_advance};
use crate::types::{Overflow, ShapedRun, TextAlign, TextLayout, TextLine, TextStyle, WrapMode};

/// The character an elided tail is replaced with.
const ELLIPSIS: &str = "\u{2026}";

/// Fraction of the font size used as a line height when a face reports none.
const FALLBACK_LINE_HEIGHT: f32 = 1.2;

/// One paragraph: the text between two hard breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The paragraph's own bytes, excluding the break that terminates it.
    ///
    /// This is what gets shaped: a line separator has no glyph and would only
    /// confuse bidi and shaping if it were included.
    pub content: Range<usize>,
    /// The paragraph including its terminating break characters.
    ///
    /// The last line of a paragraph reports *this* end, so that the line ranges
    /// of a layout tile the whole input with no gaps — a caret placed after the
    /// newline has to belong to some line.
    pub total: Range<usize>,
}

/// True for the characters that unconditionally end a line.
///
/// The set is UAX #14's mandatory-break class BK/CR/LF/NL, so that a paragraph
/// handed to the line breaker never contains a mandatory break of its own.
fn is_hard_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// Splits text at hard breaks.
///
/// Always returns at least one segment, so an empty string still lays out as a
/// single empty line and a caret has somewhere to sit. A trailing break
/// produces a trailing empty segment for the same reason: after typing Enter,
/// the new empty line has to exist.
pub fn split_hard_breaks(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        let c = match text[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        let n = c.len_utf8();
        if is_hard_break(c) {
            let mut brk = i + n;
            // CR LF is one break, not two empty lines.
            if c == '\r' && text[brk..].starts_with('\n') {
                brk += 1;
            }
            out.push(Segment { content: start..i, total: start..brk });
            start = brk;
            i = brk;
        } else {
            i += n;
        }
    }
    out.push(Segment { content: start..text.len(), total: start..text.len() });
    out
}

/// Byte offsets at which a line may end, ascending, always ending with
/// `text.len()`.
///
/// Offsets are "the index of the character after the break", matching
/// `unicode_linebreak`. An empty string has no opportunities at all, which the
/// caller handles by emitting one empty line.
pub fn break_candidates(text: &str, wrap: WrapMode) -> Vec<usize> {
    let len = text.len();
    if len == 0 {
        return Vec::new();
    }
    let mut out: Vec<usize> = match wrap {
        WrapMode::None => vec![len],
        WrapMode::Word => unicode_linebreak::linebreaks(text).map(|(i, _)| i).collect(),
        // Every grapheme boundary is a legal break. This is what makes Thai and
        // Japanese wrap at all: neither puts spaces between words, so UAX #14
        // alone would hand back one enormous line.
        WrapMode::Grapheme => text.grapheme_indices(true).map(|(i, _)| i).collect(),
    };
    out.retain(|&c| c > 0 && c <= len);
    out.sort_unstable();
    out.dedup();
    if out.last() != Some(&len) {
        out.push(len);
    }
    out
}

/// The horizontal offset that satisfies an alignment.
///
/// `box_width` is the width alignment resolves against — the constraint when
/// there is one, otherwise the widest line, so that centring a paragraph with
/// no maximum width still centres its short lines against its long ones.
///
/// The result is not clamped: a line wider than the box gets a negative offset
/// under [`TextAlign::Right`], which is what makes overflowing right-aligned
/// text run off the left edge exactly as CSS specifies.
pub fn align_offset(
    align: TextAlign,
    base_rtl: bool,
    box_width: Px,
    line_width: Px,
    last_line: bool,
) -> Px {
    let slack = box_width - line_width;
    match align {
        TextAlign::Left => Px::ZERO,
        TextAlign::Right => slack,
        TextAlign::Center => slack * 0.5,
        TextAlign::Start => {
            if base_rtl {
                slack
            } else {
                Px::ZERO
            }
        }
        TextAlign::End => {
            if base_rtl {
                Px::ZERO
            } else {
                slack
            }
        }
        // A justified line is stretched, not moved; only the last line of a
        // paragraph falls back to start alignment.
        TextAlign::Justify => {
            if last_line && base_rtl {
                slack
            } else {
                Px::ZERO
            }
        }
    }
}

/// A line before alignment has been resolved.
struct Draft {
    runs: Vec<ShapedRun>,
    /// Advance of the line excluding trailing whitespace.
    width: Px,
    source: Range<usize>,
    /// True for the last line of its paragraph, which is not justified.
    last_in_paragraph: bool,
    /// The direction of the paragraph this line came from. Alignment resolves
    /// per paragraph, so an RTL quote inside an LTR document still hangs off
    /// its own start edge.
    rtl: bool,
}

/// Lays out a paragraph of text.
///
/// `max_width` constrains wrapping, alignment and elision. Passing `None` means
/// unconstrained: the text becomes one line per hard break, however wide.
///
/// Returns an empty layout when the database holds no usable face; callers that
/// need to distinguish "no text" from "no font" should check
/// [`FontDatabase::is_empty`] first.
pub fn layout_text(
    db: &mut FontDatabase,
    text: &str,
    style: &TextStyle,
    max_width: Option<Px>,
) -> TextLayout {
    let Some(primary) = db.resolve(&style.font) else {
        return TextLayout { lines: Vec::new(), size: Size::ZERO, max_width };
    };

    let metrics = db.face_metrics(primary).unwrap_or_default().scaled(style.font_size);
    let line_height = style.line_height.unwrap_or_else(|| {
        let h = metrics.line_height();
        if h > Px::ZERO { h } else { style.font_size * FALLBACK_LINE_HEIGHT }
    });
    // CSS half-leading: the difference between the line box and the font's own
    // extent is split evenly above and below, which keeps a line of 20 px text
    // in a 30 px box optically centred instead of hugging the top.
    let extent = metrics.ascent + metrics.descent;
    let ascent_in_line = (line_height - extent) * 0.5 + metrics.ascent;

    let mut drafts: Vec<Draft> = Vec::new();
    // The ellipsis is shaped at most once per layout, not once per elided line.
    let mut ellipsis: Option<(Vec<ShapedRun>, Px)> = None;

    for segment in split_hard_breaks(text) {
        let paragraph_text = &text[segment.content.clone()];
        let paragraph =
            ShapedParagraph::shape(db, paragraph_text, segment.content.start, style, primary);
        let rtl = paragraph.is_rtl();

        let ranges = wrap_paragraph(&paragraph, paragraph_text, style, max_width);
        let count = ranges.len();
        for (index, local) in ranges.into_iter().enumerate() {
            let last_in_paragraph = index + 1 == count;
            let start = segment.content.start + local.start;
            let end = segment.content.start + local.end;
            let source = start..if last_in_paragraph { segment.total.end } else { end };

            let (mut runs, mut width) = build_line(&paragraph, paragraph_text, &local, start..end);

            if style.overflow == Overflow::Ellipsis {
                if let Some(max) = max_width {
                    if width > max {
                        if ellipsis.is_none() {
                            let e = ShapedParagraph::shape(db, ELLIPSIS, 0, style, primary);
                            let proto = e.line_runs(0..ELLIPSIS.len());
                            let w = total_advance(&proto);
                            ellipsis = Some((proto, w));
                        }
                        let (proto, ewidth) = ellipsis.as_ref().expect("just populated");
                        let (r, w) =
                            elide(&paragraph, paragraph_text, &local, max, proto, *ewidth, rtl);
                        runs = r;
                        width = w;
                    }
                }
            }

            drafts.push(Draft { runs, width, source, last_in_paragraph, rtl });
        }
    }

    // Alignment resolves against the constraint if there is one, otherwise
    // against the widest line so that the block still reads as a block.
    let widest = drafts.iter().fold(Px::ZERO, |a, d| a.max(d.width));
    let box_width = max_width.unwrap_or(widest);

    let mut lines = Vec::with_capacity(drafts.len());
    let mut top = Px::ZERO;
    let mut extent_right = Px::ZERO;
    for mut draft in drafts {
        if style.align == TextAlign::Justify && !draft.last_in_paragraph {
            let slack = box_width - draft.width;
            if slack > Px::ZERO && justify(&mut draft.runs, text, slack) {
                draft.width = box_width;
            }
        }
        let dx =
            align_offset(style.align, draft.rtl, box_width, draft.width, draft.last_in_paragraph);
        offset_runs(&mut draft.runs, dx);

        extent_right = extent_right.max(dx + draft.width);
        lines.push(TextLine {
            runs: draft.runs,
            baseline: top + ascent_in_line,
            height: line_height,
            width: draft.width,
            source: draft.source,
        });
        top += line_height;
    }

    TextLayout { lines, size: Size::new(extent_right.max(Px::ZERO), top), max_width }
}

// ---- internals -----------------------------------------------------------

/// A one-element line list. Spelled with an iterator rather than `vec![a..b]`,
/// which reads as "a vector of that range's elements" to both clippy and a
/// human skimming the line.
fn one_line(range: Range<usize>) -> Vec<Range<usize>> {
    core::iter::once(range).collect()
}

/// Greedy line breaking within one paragraph, in paragraph-local coordinates.
fn wrap_paragraph(
    paragraph: &ShapedParagraph<'_>,
    text: &str,
    style: &TextStyle,
    max_width: Option<Px>,
) -> Vec<Range<usize>> {
    let len = text.len();
    if len == 0 {
        return one_line(0..0);
    }
    let max = match (style.wrap, max_width) {
        (WrapMode::None, _) | (_, None) => return one_line(0..len),
        (_, Some(m)) => m,
    };
    let candidates = break_candidates(text, style.wrap);
    if candidates.is_empty() {
        return one_line(0..len);
    }

    let base = paragraph.range().start;
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < len {
        let first = candidates.partition_point(|&c| c <= start);
        let rest = &candidates[first..];
        if rest.is_empty() {
            lines.push(start..len);
            break;
        }
        // Trailing whitespace hangs past the edge rather than forcing a break,
        // so it is excluded from the fit test. Widths are non-decreasing in the
        // candidate, which is what makes the binary search valid.
        let fits = |c: &usize| {
            let trimmed = start + text[start..*c].trim_end().len();
            paragraph.advance((base + start)..(base + trimmed)) <= max
        };
        let k = rest.partition_point(fits);
        // `rest` holds only offsets strictly greater than `start`, so either
        // branch advances and the loop always terminates — including for a
        // single word wider than the whole box, which simply overflows.
        let end = if k > 0 { rest[k - 1] } else { rest[0] };
        debug_assert!(end > start, "line breaking failed to advance");
        lines.push(start..end);
        start = end;
    }
    lines
}

/// Assembles one line's runs and its visible width.
fn build_line(
    paragraph: &ShapedParagraph<'_>,
    text: &str,
    local: &Range<usize>,
    source: Range<usize>,
) -> (Vec<ShapedRun>, Px) {
    let mut runs = paragraph.line_runs(source.clone());
    let base = paragraph.range().start;
    let trimmed = local.start + text[local.clone()].trim_end().len();
    let width = paragraph.advance((base + local.start)..(base + trimmed));
    let trailing = total_advance(&runs) - width;

    // In a right-to-left paragraph the trailing whitespace is on the visual
    // *left*, so leaving it in place would push the visible text right by its
    // width. Pull the whole line back instead.
    if paragraph.is_rtl() && trailing > Px::ZERO {
        offset_runs(&mut runs, -trailing);
    }
    (runs, width)
}

/// Replaces the tail of an over-wide line with an ellipsis that fits.
///
/// The cut lands on a grapheme boundary so a combining mark is never separated
/// from its base, and it is chosen by the same advance table the line was
/// measured with, so the result provably fits: `content + ellipsis <= max`.
fn elide(
    paragraph: &ShapedParagraph<'_>,
    text: &str,
    local: &Range<usize>,
    max: Px,
    proto: &[ShapedRun],
    ellipsis_width: Px,
    rtl: bool,
) -> (Vec<ShapedRun>, Px) {
    let base = paragraph.range().start;
    let line_text = &text[local.clone()];
    let budget = max - ellipsis_width;

    let mut cut = local.start;
    if budget > Px::ZERO {
        let mut boundaries: Vec<usize> =
            line_text.grapheme_indices(true).map(|(i, _)| i).filter(|&i| i > 0).collect();
        boundaries.push(line_text.len());
        let k = boundaries.partition_point(|&c| {
            paragraph.advance((base + local.start)..(base + local.start + c)) <= budget
        });
        if k > 0 {
            cut = local.start + boundaries[k - 1];
        }
    }

    let mut runs = paragraph.line_runs((base + local.start)..(base + cut));
    let kept = total_advance(&runs);

    // The ellipsis stands in for everything after the cut, so that clicking it
    // resolves to the start of the hidden tail rather than to nowhere.
    let mut tail: Vec<ShapedRun> = proto.to_vec();
    let hidden = (base + cut)..(base + local.end);
    for run in &mut tail {
        run.source = hidden.clone();
        for g in &mut run.glyphs {
            g.cluster = hidden.clone();
        }
    }

    if rtl {
        // The ellipsis belongs at the visual end, which for RTL is the left.
        offset_runs(&mut runs, ellipsis_width);
        tail.append(&mut runs);
        (tail, kept + ellipsis_width)
    } else {
        offset_runs(&mut tail, kept);
        runs.append(&mut tail);
        (runs, kept + ellipsis_width)
    }
}

/// Distributes slack across the inter-word gaps of one line.
///
/// Returns false when the line has no gap to stretch — a single unbroken word,
/// or CJK with no spaces — in which case it is left alone rather than having
/// letters pulled apart.
fn justify(runs: &mut [ShapedRun], text: &str, slack: Px) -> bool {
    let total: usize = runs.iter().map(|r| r.glyphs.len()).sum();
    if total == 0 {
        return false;
    }
    let is_gap = |g: &crate::types::ShapedGlyph| -> bool {
        text.get(g.cluster.clone()).is_some_and(|s| s.contains(' '))
    };

    // The final glyph of the line is never a gap: stretching it would push the
    // line past its own end without moving anything.
    let mut gaps = 0usize;
    let mut seen = 0usize;
    for run in runs.iter() {
        for g in run.glyphs.iter() {
            seen += 1;
            if seen < total && is_gap(g) {
                gaps += 1;
            }
        }
    }
    if gaps == 0 {
        return false;
    }

    let share = slack / gaps as f32;
    let mut shift = Px::ZERO;
    seen = 0;
    for run in runs.iter_mut() {
        for g in run.glyphs.iter_mut() {
            g.position.x += shift;
            seen += 1;
            if seen < total && text.get(g.cluster.clone()).is_some_and(|s| s.contains(' ')) {
                g.advance += share;
                shift += share;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FontRequest, TextDirection};
    use sphere_core::{FontId, Point, px};

    fn style() -> TextStyle {
        TextStyle { font_size: px(16.0), ..Default::default() }
    }

    /// A system database plus its default face, or `None` on a bare machine.
    fn system() -> Option<(FontDatabase, FontId)> {
        let mut db = FontDatabase::with_system_fonts();
        db.resolve(&FontRequest::default()).map(|id| (db, id))
    }

    /// Every byte of the input must belong to exactly one line, in order.
    fn assert_covers(layout: &TextLayout, text: &str) {
        assert!(!layout.lines.is_empty());
        let mut at = 0usize;
        for line in &layout.lines {
            assert_eq!(line.source.start, at, "gap or overlap between lines");
            assert!(line.source.end >= line.source.start);
            at = line.source.end;
        }
        assert_eq!(at, text.len(), "lines do not reach the end of the text");
    }

    // ---- pure logic: no font required ------------------------------------

    #[test]
    fn hard_breaks_split_and_tile_the_input() {
        let segs = split_hard_breaks("a\nb");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0], Segment { content: 0..1, total: 0..2 });
        assert_eq!(segs[1], Segment { content: 2..3, total: 2..3 });
    }

    #[test]
    fn a_trailing_break_leaves_an_empty_final_paragraph() {
        // After pressing Enter the new line has to exist, or the caret has
        // nowhere to go.
        let segs = split_hard_breaks("a\n");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1], Segment { content: 2..2, total: 2..2 });
    }

    #[test]
    fn crlf_is_a_single_break() {
        let segs = split_hard_breaks("a\r\nb");
        assert_eq!(segs.len(), 2, "CR LF produced a spurious empty line");
        assert_eq!(segs[0], Segment { content: 0..1, total: 0..3 });
        assert_eq!(segs[1].content, 3..4);
    }

    #[test]
    fn consecutive_breaks_produce_empty_paragraphs() {
        let segs = split_hard_breaks("\n\n");
        assert_eq!(segs.len(), 3);
        assert!(segs.iter().all(|s| s.content.is_empty()));
    }

    #[test]
    fn empty_input_is_still_one_paragraph() {
        assert_eq!(split_hard_breaks(""), vec![Segment { content: 0..0, total: 0..0 }]);
    }

    #[test]
    fn other_mandatory_break_characters_are_recognised() {
        // Vertical tab, form feed, NEL and the Unicode separators are all
        // mandatory breaks; leaving them to the line breaker instead would put
        // a mandatory break inside a paragraph, where nothing handles it.
        for c in ['\u{000B}', '\u{000C}', '\u{0085}', '\u{2028}', '\u{2029}'] {
            let s = format!("a{c}b");
            assert_eq!(split_hard_breaks(&s).len(), 2, "{c:?} did not break");
        }
    }

    #[test]
    fn word_break_candidates_follow_uax14() {
        assert_eq!(break_candidates("hello world", WrapMode::Word), vec![6, 11]);
        assert_eq!(break_candidates("hello", WrapMode::Word), vec![5]);
        // A hyphen is a break opportunity; a full stop mid-word is not.
        assert!(break_candidates("mid-word", WrapMode::Word).contains(&4));
    }

    #[test]
    fn grapheme_candidates_break_anywhere() {
        assert_eq!(break_candidates("abc", WrapMode::Grapheme), vec![1, 2, 3]);
        // A combining mark never becomes a break opportunity of its own.
        let text = "e\u{0301}x";
        assert_eq!(break_candidates(text, WrapMode::Grapheme), vec![3, 4]);
    }

    #[test]
    fn no_wrap_offers_only_the_end() {
        assert_eq!(break_candidates("hello world", WrapMode::None), vec![11]);
    }

    #[test]
    fn candidates_always_end_at_the_string_end() {
        for wrap in [WrapMode::Word, WrapMode::Grapheme, WrapMode::None] {
            for text in ["a", "hello world", "ไทย", "a b "] {
                let c = break_candidates(text, wrap);
                assert_eq!(c.last(), Some(&text.len()), "{wrap:?} {text:?}");
                assert!(c.windows(2).all(|w| w[0] < w[1]), "not strictly ascending");
                assert!(c.iter().all(|&i| text.is_char_boundary(i)));
            }
        }
        assert!(break_candidates("", WrapMode::Word).is_empty());
    }

    #[test]
    fn alignment_offsets_resolve_against_the_base_direction() {
        let (box_w, line_w) = (px(100.0), px(40.0));
        let slack = px(60.0);
        assert_eq!(align_offset(TextAlign::Left, false, box_w, line_w, false), Px::ZERO);
        assert_eq!(align_offset(TextAlign::Right, false, box_w, line_w, false), slack);
        assert_eq!(align_offset(TextAlign::Center, false, box_w, line_w, false), px(30.0));
        // Start/End flip with the paragraph direction; Left/Right never do.
        assert_eq!(align_offset(TextAlign::Start, false, box_w, line_w, false), Px::ZERO);
        assert_eq!(align_offset(TextAlign::Start, true, box_w, line_w, false), slack);
        assert_eq!(align_offset(TextAlign::End, false, box_w, line_w, false), slack);
        assert_eq!(align_offset(TextAlign::End, true, box_w, line_w, false), Px::ZERO);
        assert_eq!(align_offset(TextAlign::Left, true, box_w, line_w, false), Px::ZERO);
        assert_eq!(align_offset(TextAlign::Right, true, box_w, line_w, false), slack);
    }

    #[test]
    fn justify_moves_only_the_last_line() {
        let (box_w, line_w) = (px(100.0), px(40.0));
        assert_eq!(align_offset(TextAlign::Justify, false, box_w, line_w, false), Px::ZERO);
        assert_eq!(align_offset(TextAlign::Justify, false, box_w, line_w, true), Px::ZERO);
        // ...except in RTL, where the last line falls back to the right edge.
        assert_eq!(align_offset(TextAlign::Justify, true, box_w, line_w, true), px(60.0));
    }

    #[test]
    fn an_overflowing_line_gets_a_negative_right_offset() {
        // CSS does not clamp: right-aligned text wider than its box runs off the
        // left edge, and clamping here would silently truncate on the right.
        let dx = align_offset(TextAlign::Right, false, px(50.0), px(80.0), false);
        assert_eq!(dx, px(-30.0));
    }

    #[test]
    fn layout_without_a_font_is_empty_not_a_panic() {
        let mut db = FontDatabase::new();
        let layout = layout_text(&mut db, "hello", &style(), Some(px(100.0)));
        assert!(layout.is_empty());
        assert_eq!(layout.size, Size::ZERO);
        assert_eq!(layout.max_width, Some(px(100.0)));
    }

    // ---- with a real font -------------------------------------------------

    #[test]
    fn ascii_width_is_plausible() {
        let Some((mut db, _)) = system() else { return };
        let layout = layout_text(&mut db, "Hello", &style(), None);
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(layout.glyph_count(), 5);
        let w = layout.size.width;
        // Five 16 px glyphs: never zero, never five full ems.
        assert!(w > px(15.0) && w < px(80.0), "{w:?}");
        assert!(layout.size.height > px(10.0) && layout.size.height < px(40.0));
        assert_covers(&layout, "Hello");
    }

    #[test]
    fn width_scales_with_font_size() {
        let Some((mut db, _)) = system() else { return };
        let small =
            layout_text(&mut db, "Hello", &TextStyle { font_size: px(10.0), ..style() }, None);
        let large =
            layout_text(&mut db, "Hello", &TextStyle { font_size: px(30.0), ..style() }, None);
        let ratio = large.size.width / small.size.width;
        assert!((ratio - 3.0).abs() < 0.05, "width did not scale linearly: {ratio}");
    }

    #[test]
    fn wrapping_happens_at_the_break_opportunity() {
        let Some((mut db, _)) = system() else { return };
        let text = "hello world";
        let full = layout_text(&mut db, text, &style(), None).size.width;
        // Constrain to just under the full width so exactly one break is needed.
        let layout = layout_text(&mut db, text, &style(), Some(full - px(4.0)));
        assert_eq!(layout.lines.len(), 2, "expected exactly one break");
        assert_eq!(layout.lines[0].source, 0..6, "break must land after the space");
        assert_eq!(layout.lines[1].source, 6..11);
        assert_covers(&layout, text);
        // The trailing space hangs: line one is measured without it.
        assert!(layout.lines[0].width <= full - px(4.0));
    }

    #[test]
    fn explicit_newlines_always_break() {
        let Some((mut db, _)) = system() else { return };
        let text = "one\ntwo\nthree";
        let layout = layout_text(&mut db, text, &style(), None);
        assert_eq!(layout.lines.len(), 3);
        assert_covers(&layout, text);
        // Baselines step down by exactly one line height.
        let step = layout.lines[1].baseline - layout.lines[0].baseline;
        assert_eq!(step, layout.lines[0].height);
        assert_eq!(layout.lines[2].baseline - layout.lines[1].baseline, step);
        assert!(layout.lines[0].baseline > Px::ZERO, "baseline must sit below the top");
        assert!(layout.lines[0].baseline < layout.lines[0].height);
    }

    #[test]
    fn a_newline_survives_wrapping() {
        let Some((mut db, _)) = system() else { return };
        let text = "hello world\nagain";
        let layout = layout_text(&mut db, text, &style(), Some(px(40.0)));
        assert!(layout.lines.len() >= 3);
        assert_covers(&layout, text);
    }

    #[test]
    fn the_empty_string_lays_out_as_one_empty_line() {
        let Some((mut db, _)) = system() else { return };
        let layout = layout_text(&mut db, "", &style(), Some(px(100.0)));
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(layout.glyph_count(), 0);
        assert_eq!(layout.lines[0].width, Px::ZERO);
        assert_eq!(layout.lines[0].source, 0..0);
        // The line still has height, or a caret in an empty field would be
        // invisible.
        assert!(layout.lines[0].height > Px::ZERO);
        assert_eq!(layout.size.width, Px::ZERO);
        assert_eq!(layout.size.height, layout.lines[0].height);
    }

    #[test]
    fn a_string_of_only_spaces_has_no_visible_width() {
        let Some((mut db, _)) = system() else { return };
        let text = "     ";
        let layout = layout_text(&mut db, text, &style(), Some(px(200.0)));
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(layout.lines[0].width, Px::ZERO, "trailing spaces must not add width");
        // The space glyphs still exist so a caret can be placed among them.
        assert_eq!(layout.glyph_count(), 5);
        assert_covers(&layout, text);
    }

    #[test]
    fn a_word_wider_than_the_box_terminates_and_overflows() {
        let Some((mut db, _)) = system() else { return };
        // The classic infinite-loop case: no break opportunity fits.
        let text = "supercalifragilisticexpialidocious";
        let layout = layout_text(&mut db, text, &style(), Some(px(10.0)));
        assert_eq!(layout.lines.len(), 1, "word wrapping must not split a word");
        assert!(layout.lines[0].width > px(10.0), "the line is expected to overflow");
        assert_covers(&layout, text);
    }

    #[test]
    fn a_long_word_breaks_under_grapheme_wrapping() {
        let Some((mut db, _)) = system() else { return };
        let text = "supercalifragilisticexpialidocious";
        let s = TextStyle { wrap: WrapMode::Grapheme, ..style() };
        let layout = layout_text(&mut db, text, &s, Some(px(40.0)));
        assert!(layout.lines.len() > 3, "grapheme wrapping must break inside a word");
        assert_covers(&layout, text);
        for line in &layout.lines {
            assert!(!line.source.is_empty(), "a zero-length line means the loop stalled");
        }
    }

    #[test]
    fn a_pathologically_narrow_box_still_terminates() {
        let Some((mut db, _)) = system() else { return };
        let s = TextStyle { wrap: WrapMode::Grapheme, ..style() };
        // Zero width: every character overflows, and every line must still
        // consume at least one grapheme.
        let layout = layout_text(&mut db, "abcdef", &s, Some(Px::ZERO));
        assert_eq!(layout.lines.len(), 6);
        assert_covers(&layout, "abcdef");
    }

    #[test]
    fn cluster_ranges_cover_the_whole_string_monotonically() {
        let Some((mut db, _)) = system() else { return };
        let text = "Hi there, world";
        let layout = layout_text(&mut db, text, &style(), Some(px(60.0)));
        assert_covers(&layout, text);
        let mut covered = vec![false; text.len()];
        for line in &layout.lines {
            for run in &line.runs {
                for g in &run.glyphs {
                    assert!(g.cluster.start <= g.cluster.end);
                    assert!(g.cluster.end <= text.len());
                    for b in g.cluster.clone() {
                        covered[b] = true;
                    }
                }
            }
        }
        assert!(covered.iter().all(|b| *b), "some source bytes map to no glyph");
    }

    #[test]
    fn hit_testing_a_laid_out_line_returns_a_source_index() {
        let Some((mut db, _)) = system() else { return };
        let text = "Hello";
        let layout = layout_text(&mut db, text, &style(), None);
        let mid = Point::new(layout.size.width * 0.5, layout.lines[0].baseline);
        let hit = layout.hit_test(mid).expect("a point inside the text must hit");
        assert!(hit <= text.len());
        assert_eq!(layout.hit_test(Point::new(px(-100.0), px(0.0))), Some(0));
    }

    #[test]
    fn mixed_direction_text_lays_out_in_visual_order() {
        let Some((mut db, _)) = system() else { return };
        let text = "abc العربية def";
        let layout = layout_text(&mut db, text, &style(), None);
        assert_eq!(layout.lines.len(), 1);
        let glyphs: Vec<_> = layout.lines[0].runs.iter().flat_map(|r| r.glyphs.iter()).collect();
        assert_eq!(glyphs[0].cluster.start, 0);
        assert_eq!(glyphs[glyphs.len() - 1].cluster.end, text.len());
        assert!(glyphs.iter().any(|g| g.rtl), "no RTL run was produced");
        assert!(glyphs.iter().any(|g| !g.rtl), "no LTR run was produced");
    }

    #[test]
    fn an_rtl_paragraph_starts_at_the_right_edge() {
        let Some((mut db, _)) = system() else { return };
        let text = "עברית";
        let s = TextStyle { direction: TextDirection::Rtl, ..style() };
        let narrow = layout_text(&mut db, text, &s, None).size.width;
        let layout = layout_text(&mut db, text, &s, Some(narrow + px(50.0)));
        // Start alignment in an RTL paragraph means the right edge.
        let leftmost = layout.lines[0]
            .runs
            .iter()
            .flat_map(|r| r.glyphs.iter())
            .fold(Px::INFINITY, |a, g| a.min(g.position.x));
        assert!(leftmost > px(40.0), "RTL text was not pushed to the right: {leftmost:?}");
    }

    #[test]
    fn thai_wraps_under_grapheme_mode_instead_of_one_giant_line() {
        let Some((mut db, _)) = system() else { return };
        // Thai has no spaces between words, so UAX #14 offers almost no
        // opportunities and word wrapping legitimately yields one line.
        let text = "สวัสดีครับผมชื่อสเฟียร์และนี่คือการทดสอบการตัดบรรทัด";
        let wide = layout_text(&mut db, text, &style(), None).size.width;
        assert!(wide > px(80.0), "Thai produced no measurable text");

        let s = TextStyle { wrap: WrapMode::Grapheme, ..style() };
        let layout = layout_text(&mut db, text, &s, Some(wide / 3.0));
        assert!(layout.lines.len() >= 3, "Thai did not wrap: {} lines", layout.lines.len());
        assert_covers(&layout, text);
        for line in &layout.lines {
            assert!(text.is_char_boundary(line.source.start));
            assert!(text.is_char_boundary(line.source.end));
        }
    }

    #[test]
    fn japanese_wraps_and_stays_on_character_boundaries() {
        let Some((mut db, _)) = system() else { return };
        let text = "こんにちは世界これはテキストレイアウトのテストです";
        let wide = layout_text(&mut db, text, &style(), None).size.width;
        let s = TextStyle { wrap: WrapMode::Grapheme, ..style() };
        let layout = layout_text(&mut db, text, &s, Some(wide / 4.0));
        assert!(layout.lines.len() >= 4, "{} lines", layout.lines.len());
        assert_covers(&layout, text);
        for line in &layout.lines {
            assert!(text.is_char_boundary(line.source.start));
        }
    }

    #[test]
    fn centering_offsets_by_half_the_slack() {
        let Some((mut db, _)) = system() else { return };
        let text = "Hi";
        let left = layout_text(&mut db, text, &style(), Some(px(200.0)));
        let s = TextStyle { align: TextAlign::Center, ..style() };
        let centered = layout_text(&mut db, text, &s, Some(px(200.0)));

        let x = |l: &TextLayout| l.lines[0].runs[0].glyphs[0].position.x;
        let expected = (px(200.0) - left.lines[0].width) * 0.5;
        assert!((x(&centered) - x(&left) - expected).abs() < px(0.01));
        // Widths themselves are unchanged by alignment.
        assert_eq!(centered.lines[0].width, left.lines[0].width);
    }

    #[test]
    fn right_alignment_puts_the_line_end_at_the_box_edge() {
        let Some((mut db, _)) = system() else { return };
        let s = TextStyle { align: TextAlign::Right, ..style() };
        let layout = layout_text(&mut db, "Hi", &s, Some(px(200.0)));
        let line = &layout.lines[0];
        let right = line
            .runs
            .iter()
            .flat_map(|r| r.glyphs.iter())
            .fold(Px::ZERO, |a, g| a.max(g.position.x + g.advance));
        assert!((right - px(200.0)).abs() < px(0.5), "{right:?}");
    }

    #[test]
    fn justification_stretches_every_line_but_the_last() {
        let Some((mut db, _)) = system() else { return };
        let text = "alpha beta gamma delta epsilon zeta";
        let s = TextStyle { align: TextAlign::Justify, ..style() };
        let max = px(120.0);
        let layout = layout_text(&mut db, text, &s, Some(max));
        assert!(layout.lines.len() >= 2, "need several lines to justify");
        for line in &layout.lines[..layout.lines.len() - 1] {
            assert!((line.width - max).abs() < px(0.5), "unjustified line {:?}", line.width);
        }
        // The last line keeps its natural width.
        assert!(layout.lines.last().unwrap().width < max);
        assert_covers(&layout, text);
    }

    #[test]
    fn ellipsis_actually_fits_within_the_constraint() {
        let Some((mut db, _)) = system() else { return };
        let text = "a very long single line of text that will not fit";
        let s = TextStyle { wrap: WrapMode::None, overflow: Overflow::Ellipsis, ..style() };
        for max in [px(30.0), px(60.0), px(120.0)] {
            let layout = layout_text(&mut db, text, &s, Some(max));
            assert_eq!(layout.lines.len(), 1);
            let w = layout.lines[0].width;
            assert!(w <= max + px(0.01), "elided line is {w:?}, max {max:?}");
            assert!(layout.glyph_count() > 0, "elision removed everything");
            // The line still claims the whole source, so selection and caret
            // movement over hidden text keep working.
            assert_eq!(layout.lines[0].source, 0..text.len());
        }
    }

    #[test]
    fn ellipsis_is_a_no_op_when_the_text_already_fits() {
        let Some((mut db, _)) = system() else { return };
        let plain = TextStyle { wrap: WrapMode::None, ..style() };
        let elided = TextStyle { overflow: Overflow::Ellipsis, ..plain.clone() };
        let a = layout_text(&mut db, "Hi", &plain, Some(px(500.0)));
        let b = layout_text(&mut db, "Hi", &elided, Some(px(500.0)));
        assert_eq!(a.lines[0].width, b.lines[0].width);
        assert_eq!(a.glyph_count(), b.glyph_count());
    }

    #[test]
    fn a_hopeless_constraint_still_produces_a_line() {
        let Some((mut db, _)) = system() else { return };
        let s = TextStyle { wrap: WrapMode::None, overflow: Overflow::Ellipsis, ..style() };
        // Narrower than the ellipsis itself: there is nothing sensible to show,
        // but it must not panic or produce a negative width.
        let layout = layout_text(&mut db, "hello world", &s, Some(px(1.0)));
        assert_eq!(layout.lines.len(), 1);
        assert!(layout.lines[0].width >= Px::ZERO);
    }

    #[test]
    fn clip_and_visible_leave_the_line_alone() {
        let Some((mut db, _)) = system() else { return };
        let text = "a very long single line of text";
        let base = TextStyle { wrap: WrapMode::None, ..style() };
        let visible = layout_text(&mut db, text, &base, Some(px(40.0)));
        let clip = layout_text(
            &mut db,
            text,
            &TextStyle { overflow: Overflow::Clip, ..base.clone() },
            Some(px(40.0)),
        );
        assert_eq!(visible.glyph_count(), clip.glyph_count());
        assert_eq!(visible.lines[0].width, clip.lines[0].width);
        assert!(clip.lines[0].width > px(40.0), "clipping is the caller's job");
    }

    #[test]
    fn an_explicit_line_height_overrides_the_font() {
        let Some((mut db, _)) = system() else { return };
        let s = TextStyle { line_height: Some(px(40.0)), ..style() };
        let layout = layout_text(&mut db, "a\nb", &s, None);
        assert_eq!(layout.lines[0].height, px(40.0));
        assert_eq!(layout.lines[1].baseline - layout.lines[0].baseline, px(40.0));
        assert_eq!(layout.size.height, px(80.0));
        // Half-leading keeps the baseline inside the taller box.
        assert!(layout.lines[0].baseline > px(10.0) && layout.lines[0].baseline < px(40.0));
    }

    #[test]
    fn letter_spacing_reaches_the_layout() {
        let Some((mut db, _)) = system() else { return };
        let plain = layout_text(&mut db, "abcd", &style(), None).size.width;
        let s = TextStyle { letter_spacing: px(2.0), ..style() };
        let spaced = layout_text(&mut db, "abcd", &s, None).size.width;
        assert!((spaced - plain - px(8.0)).abs() < px(0.01));
    }

    #[test]
    fn wrapping_is_stable_across_repeated_calls() {
        // Caches inside the font database must not change the answer.
        let Some((mut db, _)) = system() else { return };
        let text = "the quick brown fox jumps over the lazy dog";
        let first = layout_text(&mut db, text, &style(), Some(px(100.0)));
        let second = layout_text(&mut db, text, &style(), Some(px(100.0)));
        assert_eq!(first.lines.len(), second.lines.len());
        assert_eq!(first.size, second.size);
        for (a, b) in first.lines.iter().zip(&second.lines) {
            assert_eq!(a.source, b.source);
            assert_eq!(a.width, b.width);
        }
    }
}
