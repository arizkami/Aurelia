//! Turning byte offsets into pixels and back.
//!
//! A text field needs three questions answered, and none of them is answerable
//! from a [`TextLayout`] without walking it carefully:
//!
//! * **Where does the caret go for byte offset *n*?** — after every cursor move.
//! * **Which byte did the user click on?** — on every press and drag.
//! * **Which rectangles cover a selected range?** — every frame it is painted.
//!
//! All three are built on [`ShapedGlyph::cluster`], the byte range in the
//! *original* string that shaping records for every glyph. That mapping is the
//! only thing that makes any of this possible, which is why it is tested for
//! monotonicity and full coverage where it is produced.
//!
//! # Bidirectional text
//!
//! A logically contiguous range is not necessarily visually contiguous. Select
//! four characters spanning the boundary of an Arabic word inside an English
//! sentence and the highlight is genuinely two rectangles, not one.
//! [`TextLayout::selection_rects`] therefore appends however many rectangles a
//! range actually needs, and never assumes one per line.
//!
//! Caret placement has the matching subtlety: the caret sits at the *leading*
//! edge of the cluster it precedes, and leading means left in a left-to-right
//! run and right in a right-to-left one.

use crate::types::{TextLayout, TextLine};
use core::ops::Range;
use sphere_core::{Point, Px, Rect, Size};

/// Where a caret sits, relative to the layout's origin.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Caret {
    /// Index of the line the caret is on.
    pub line: usize,
    /// Horizontal offset of the caret's centre line.
    pub x: Px,
    /// Top edge of the caret.
    pub top: Px,
    /// Height of the caret, which is the line's height.
    pub height: Px,
}

impl Caret {
    /// The caret as a zero-width rectangle, ready to be outset into a bar.
    #[inline]
    pub fn rect(&self) -> Rect<Px> {
        Rect::new(Point::new(self.x, self.top), Size::new(Px::ZERO, self.height))
    }
}

impl TextLayout {
    /// The line containing a byte offset.
    ///
    /// An offset past the end belongs to the last line, which is what makes
    /// "caret at the end of the text" work without a special case at every call
    /// site.
    pub fn line_at_byte(&self, byte: usize) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        // A byte on a line boundary belongs to the line that *starts* there, so
        // that pressing End then Right moves onto the next line rather than
        // sticking. The exception is the final line, which owns everything after
        // it as well.
        let found = self.lines.iter().position(|l| byte < l.source.end);
        Some(found.unwrap_or(self.lines.len() - 1))
    }

    /// Where the caret goes for a byte offset.
    ///
    /// Returns `None` only when nothing was laid out at all; a caller with an
    /// empty string has to supply its own line height, because an empty layout
    /// has no line to take one from.
    pub fn caret(&self, byte: usize) -> Option<Caret> {
        let index = self.line_at_byte(byte)?;
        let line = &self.lines[index];
        Some(Caret {
            line: index,
            x: caret_x_in_line(line, byte),
            top: line.baseline - line.height * BASELINE_RATIO,
            height: line.height,
        })
    }

    /// The byte offset nearest a point, for click and drag.
    ///
    /// Clamps rather than failing: a click above the text lands at offset zero
    /// and a click below it lands at the end, which is what dragging a selection
    /// past the edge of a field has to do.
    pub fn hit(&self, point: Point<Px>) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let index = self
            .lines
            .iter()
            .position(|l| point.y < l.baseline - l.height * BASELINE_RATIO + l.height)
            .unwrap_or(self.lines.len() - 1);
        let line = &self.lines[index];

        let mut best = line.source.start;
        let mut best_distance = f32::INFINITY;
        // The end of the line is a candidate in its own right, or a click past
        // the last glyph would snap back to that glyph's leading edge.
        let mut consider = |byte: usize, x: Px| {
            let d = (x.get() - point.x.get()).abs();
            if d < best_distance {
                best_distance = d;
                best = byte;
            }
        };

        for run in &line.runs {
            for g in &run.glyphs {
                let (leading, trailing) = if g.rtl {
                    (g.position.x + g.advance, g.position.x)
                } else {
                    (g.position.x, g.position.x + g.advance)
                };
                consider(g.cluster.start, leading);
                consider(g.cluster.end, trailing);
            }
        }
        best
    }

    /// Appends the rectangles covering a byte range.
    ///
    /// Appends rather than returns, because selection painting happens every
    /// frame and allocating a fresh `Vec` there would be a per-frame allocation
    /// in a hot path. Emits nothing for an empty range.
    pub fn selection_rects(&self, range: Range<usize>, out: &mut Vec<Rect<Px>>) {
        if range.start >= range.end {
            return;
        }
        for line in &self.lines {
            let top = line.baseline - line.height * BASELINE_RATIO;
            // One rectangle per *visually* contiguous span. A logical range that
            // straddles a direction change is genuinely more than one box.
            let mut span: Option<(Px, Px)> = None;
            let flush = |span: &mut Option<(Px, Px)>, out: &mut Vec<Rect<Px>>| {
                if let Some((lo, hi)) = span.take()
                    && hi > lo
                {
                    out.push(Rect::new(Point::new(lo, top), Size::new(hi - lo, line.height)));
                }
            };

            for run in &line.runs {
                for g in &run.glyphs {
                    // Membership by cluster start, matching how runs are sliced
                    // elsewhere, so a cluster is never half-selected.
                    let inside = g.cluster.start < range.end && g.cluster.end > range.start;
                    if !inside {
                        flush(&mut span, out);
                        continue;
                    }
                    let lo = g.position.x;
                    let hi = g.position.x + g.advance;
                    match &mut span {
                        Some((_, end)) if (end.get() - lo.get()).abs() < JOIN_TOLERANCE => {
                            *end = hi;
                        }
                        Some(_) => {
                            flush(&mut span, out);
                            span = Some((lo, hi));
                        }
                        None => span = Some((lo, hi)),
                    }
                }
            }
            flush(&mut span, out);
        }
    }
}

/// How much of a line's height sits above its baseline.
///
/// The layout engine does not record ascent and descent per line, and deriving
/// the split from the font would need the face here. Four fifths is close enough
/// for a caret and a selection box across the faces this engine targets, and it
/// is a constant rather than a magic number at three call sites.
const BASELINE_RATIO: f32 = 0.8;

/// Two spans closer than this are treated as adjacent and merged.
///
/// Adjacent glyph advances do not sum to exactly the next position in floating
/// point, and without a tolerance a selection would be drawn as one rectangle
/// per glyph with hairline seams between them.
const JOIN_TOLERANCE: f32 = 0.01;

/// The caret's x for a byte offset within one line.
fn caret_x_in_line(line: &TextLine, byte: usize) -> Px {
    if byte <= line.source.start {
        return leading_edge_of_line(line);
    }
    if byte >= line.source.end {
        return trailing_edge_of_line(line);
    }

    for run in &line.runs {
        for g in &run.glyphs {
            if byte < g.cluster.start || byte >= g.cluster.end {
                continue;
            }
            let span = g.cluster.end.saturating_sub(g.cluster.start);
            if span <= 1 || byte == g.cluster.start {
                return if g.rtl { g.position.x + g.advance } else { g.position.x };
            }
            // Inside a multi-byte cluster — a ligature, or a character whose
            // UTF-8 length exceeds one. There is no glyph boundary to land on,
            // so the offset is interpolated across the cluster's advance. That
            // is what lets a caret move through "ffi" rather than jumping it.
            let t = (byte - g.cluster.start) as f32 / span as f32;
            return if g.rtl {
                g.position.x + g.advance - g.advance * t
            } else {
                g.position.x + g.advance * t
            };
        }
    }
    // Offsets that no cluster claims — inside a run of whitespace that shaped to
    // nothing, say — fall back to the end of the line rather than to zero, since
    // an unclaimed offset is almost always a trailing one.
    trailing_edge_of_line(line)
}

/// The x a caret takes at the very start of a line, in logical order.
fn leading_edge_of_line(line: &TextLine) -> Px {
    match line.runs.first().and_then(|r| r.glyphs.first()) {
        Some(g) if g.rtl => g.position.x + g.advance,
        Some(g) => g.position.x,
        None => Px::ZERO,
    }
}

/// The x a caret takes at the very end of a line, in logical order.
fn trailing_edge_of_line(line: &TextLine) -> Px {
    match line.runs.last().and_then(|r| r.glyphs.last()) {
        Some(g) if g.rtl => g.position.x,
        Some(g) => g.position.x + g.advance,
        None => Px::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ShapedGlyph, ShapedRun};
    use sphere_core::{FontId, GlyphId, px};

    /// A left-to-right run of one-byte characters, each `advance` wide.
    fn ltr(start: usize, count: usize, advance: f32, x0: f32) -> ShapedRun {
        ShapedRun {
            font: FontId::new(0, 1),
            font_size: px(12.0),
            rtl: false,
            glyphs: (0..count)
                .map(|i| ShapedGlyph {
                    glyph: GlyphId(1),
                    font: FontId::new(0, 1),
                    position: Point::new(px(x0 + advance * i as f32), Px::ZERO),
                    advance: px(advance),
                    cluster: (start + i)..(start + i + 1),
                    rtl: false,
                })
                .collect(),
            source: start..(start + count),
        }
    }

    fn one_line(runs: Vec<ShapedRun>, source: Range<usize>) -> TextLayout {
        let width = runs.iter().flat_map(|r| r.glyphs.iter()).map(|g| g.advance).sum();
        TextLayout {
            lines: vec![TextLine { runs, baseline: px(12.0), height: px(15.0), width, source }],
            size: Size::new(width, px(15.0)),
            max_width: None,
        }
    }

    #[test]
    fn a_caret_sits_at_the_leading_edge_of_the_cluster_it_precedes() {
        let layout = one_line(vec![ltr(0, 4, 10.0, 0.0)], 0..4);
        assert_eq!(layout.caret(0).unwrap().x, px(0.0));
        assert_eq!(layout.caret(2).unwrap().x, px(20.0));
        // Past the last cluster the caret goes to the trailing edge, which is
        // the only way "caret at end of text" can work.
        assert_eq!(layout.caret(4).unwrap().x, px(40.0));
    }

    #[test]
    fn a_caret_past_the_end_clamps_rather_than_failing() {
        let layout = one_line(vec![ltr(0, 2, 10.0, 0.0)], 0..2);
        assert_eq!(layout.caret(99).unwrap().x, px(20.0));
    }

    #[test]
    fn an_empty_layout_has_no_caret_to_report() {
        // Deliberately `None` rather than a zero caret: the caller has to supply
        // a line height from its style, and silently returning zero height would
        // paint an invisible caret in an empty field.
        assert_eq!(TextLayout::default().caret(0), None);
    }

    #[test]
    fn a_caret_inside_a_multi_byte_cluster_interpolates() {
        // One glyph covering four bytes, as a ligature or a wide codepoint does.
        let mut run = ltr(0, 1, 40.0, 0.0);
        run.glyphs[0].cluster = 0..4;
        run.source = 0..4;
        let layout = one_line(vec![run], 0..4);
        // Halfway through the cluster is halfway across its advance, so a caret
        // walks through a ligature instead of jumping over it.
        assert_eq!(layout.caret(2).unwrap().x, px(20.0));
    }

    #[test]
    fn hit_testing_snaps_to_the_nearer_edge() {
        let layout = one_line(vec![ltr(0, 4, 10.0, 0.0)], 0..4);
        assert_eq!(layout.hit(Point::new(px(1.0), px(5.0))), 0);
        assert_eq!(layout.hit(Point::new(px(9.0), px(5.0))), 1);
        assert_eq!(layout.hit(Point::new(px(24.0), px(5.0))), 2);
        // Past the end clamps to the end rather than wrapping to zero.
        assert_eq!(layout.hit(Point::new(px(500.0), px(5.0))), 4);
        assert_eq!(layout.hit(Point::new(px(-500.0), px(5.0))), 0);
    }

    #[test]
    fn a_selection_over_adjacent_glyphs_is_one_rectangle() {
        let layout = one_line(vec![ltr(0, 4, 10.0, 0.0)], 0..4);
        let mut rects = Vec::new();
        layout.selection_rects(1..3, &mut rects);
        assert_eq!(rects.len(), 1, "{rects:?}");
        assert_eq!(rects[0].min_x(), px(10.0));
        assert_eq!(rects[0].max_x(), px(30.0));
    }

    #[test]
    fn an_empty_range_selects_nothing() {
        let layout = one_line(vec![ltr(0, 4, 10.0, 0.0)], 0..4);
        let mut rects = Vec::new();
        layout.selection_rects(2..2, &mut rects);
        assert!(rects.is_empty());
    }

    #[test]
    fn a_visually_split_selection_is_more_than_one_rectangle() {
        // Two runs whose logical order and visual order disagree, which is what
        // a bidi paragraph produces. The selected bytes are contiguous; the
        // pixels are not, and one rectangle would highlight text that is not
        // selected.
        let left = ltr(0, 2, 10.0, 0.0);
        let mut right = ltr(4, 2, 10.0, 40.0);
        right.source = 4..6;
        let layout = one_line(vec![left, right], 0..6);
        let mut rects = Vec::new();
        layout.selection_rects(0..6, &mut rects);
        assert_eq!(rects.len(), 2, "a gap in the middle must break the box: {rects:?}");
    }

    #[test]
    fn the_caret_rect_is_zero_width_and_a_full_line_tall() {
        let layout = one_line(vec![ltr(0, 4, 10.0, 0.0)], 0..4);
        let r = layout.caret(1).unwrap().rect();
        assert_eq!(r.width(), Px::ZERO);
        assert_eq!(r.height(), px(15.0));
    }
}
