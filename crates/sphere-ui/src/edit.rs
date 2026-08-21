//! An editable string with a selection and an input-method composition.
//!
//! Deliberately free of any widget, any layout and any pixels. Everything here
//! is byte offsets into a `String`, which means the whole of it is testable
//! without a font, a GPU or a window — and text editing has enough edge cases
//! that anything less would be untestable in practice.
//!
//! # Composition, and why the preedit lives in the buffer
//!
//! An input method composes text over several keystrokes before committing it.
//! While that is happening the user must *see* what they are composing, in
//! place, with the surrounding text reflowing around it. So the provisional
//! text is inserted into [`TextEdit::text`] like any other text, and
//! [`TextEdit::preedit`] records the byte range it occupies.
//!
//! That has two consequences worth stating outright:
//!
//! * [`TextEdit::text`] is **not** the committed value while composing. Read
//!   [`TextEdit::committed_text`] if you want the value without the provisional
//!   part — a form validating as the user types wants that one.
//! * Every edit that is not part of the composition has to be refused while a
//!   composition is open, or it would splice into a range the input method
//!   believes it owns. [`TextEdit::is_composing`] is the check;
//!   [`crate::widgets::TextField`] performs it.
//!
//! # What the platform sends
//!
//! | Platform event | Call |
//! |---|---|
//! | `ImeEvent::Start` | nothing; composition begins on the first pre-edit |
//! | `ImeEvent::Preedit { text, cursor }` | [`TextEdit::set_preedit`] |
//! | `ImeEvent::Commit { text }` | [`TextEdit::commit`] |
//! | `ImeEvent::End` | [`TextEdit::cancel_composition`] |
//!
//! `End` discards rather than commits. An input method that wanted the text
//! kept sends `Commit` first, and treating `End` as a commit would insert
//! half-composed text every time the user pressed Escape.

use core::ops::Range;
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

/// The provisional text an input method is composing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preedit {
    /// Byte range within [`TextEdit::text`] holding the provisional text.
    pub range: Range<usize>,
    /// The input method's own cursor, as byte offsets **relative to
    /// `range.start`**.
    ///
    /// `None` means the platform asked for the caret to be hidden, which some
    /// input methods do while a candidate window is open. It is not the same as
    /// a caret at zero, and a widget that renders it as one will flicker a caret
    /// at the start of every composition.
    pub cursor: Option<(usize, usize)>,
}

impl Preedit {
    /// Length of the provisional text in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.range.end - self.range.start
    }

    /// True when the composition occupies no bytes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

/// How far a cursor movement travels.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Motion {
    /// One grapheme cluster left. A cluster, not a `char`: `é` written as `e` +
    /// combining acute is two `char`s and one press of the left arrow.
    Left,
    /// One grapheme cluster right.
    Right,
    /// To the start of the previous word.
    WordLeft,
    /// To the start of the next word.
    WordRight,
    /// To the start of the text.
    Home,
    /// To the end of the text.
    End,
}

/// An editable string.
///
/// Single-line: there is no vertical motion and no newline handling, because a
/// multi-line editor needs the laid-out lines to move a caret up and down and
/// this type deliberately knows nothing about layout. See the roadmap.
#[derive(Clone, Debug, Default)]
pub struct TextEdit {
    text: String,
    /// Where the selection was anchored, in bytes.
    anchor: usize,
    /// Where the caret is, in bytes. Equal to `anchor` when nothing is selected.
    focus: usize,
    preedit: Option<Preedit>,
    max_len: Option<usize>,
}

impl TextEdit {
    /// An empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer holding `text`, with the caret at the end.
    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self { text, anchor: end, focus: end, preedit: None, max_len: None }
    }

    /// Caps the length in bytes. Insertions that would exceed it are truncated
    /// at a character boundary rather than refused, so a paste of too much text
    /// still puts as much in as fits.
    pub fn with_max_len(mut self, max: usize) -> Self {
        self.max_len = Some(max);
        self
    }

    /// The full buffer, provisional composition text included.
    #[inline]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The buffer without any provisional composition text.
    ///
    /// This is the value to read when something outside the editor consumes it —
    /// validation, a search-as-you-type query, a bound model field. Using
    /// [`TextEdit::text`] there would feed it half-composed syllables.
    pub fn committed_text(&self) -> std::borrow::Cow<'_, str> {
        match &self.preedit {
            Some(p) if !p.is_empty() => {
                let mut out = String::with_capacity(self.text.len() - p.len());
                out.push_str(&self.text[..p.range.start]);
                out.push_str(&self.text[p.range.end..]);
                std::borrow::Cow::Owned(out)
            }
            _ => std::borrow::Cow::Borrowed(&self.text),
        }
    }

    /// Byte length of the buffer.
    #[inline]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// True when the buffer holds nothing.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The composition in progress, if any.
    #[inline]
    pub fn preedit(&self) -> Option<&Preedit> {
        self.preedit.as_ref()
    }

    /// True while an input method owns part of the buffer.
    ///
    /// Every edit that did not come from the input method must be refused while
    /// this holds, or it splices into a range the input method believes it owns
    /// and the next pre-edit replaces the wrong bytes.
    #[inline]
    pub fn is_composing(&self) -> bool {
        self.preedit.is_some()
    }

    /// The selection as an ordered byte range. Empty when it is a bare caret.
    #[inline]
    pub fn selection(&self) -> Range<usize> {
        if self.anchor <= self.focus { self.anchor..self.focus } else { self.focus..self.anchor }
    }

    /// Where the caret is.
    #[inline]
    pub fn caret(&self) -> usize {
        self.focus
    }

    /// True when more than a caret is selected.
    #[inline]
    pub fn has_selection(&self) -> bool {
        self.anchor != self.focus
    }

    /// The selected text.
    #[inline]
    pub fn selected_text(&self) -> &str {
        let r = self.selection();
        &self.text[r]
    }

    /// Replaces the whole buffer and puts the caret at the end.
    ///
    /// Drops any composition: the text an input method was composing against no
    /// longer exists.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.truncate_to_max();
        self.preedit = None;
        self.anchor = self.text.len();
        self.focus = self.anchor;
    }

    /// Moves the caret and collapses the selection, clamped to a boundary.
    pub fn set_caret(&mut self, byte: usize) {
        let b = self.clamp_boundary(byte);
        self.anchor = b;
        self.focus = b;
    }

    /// Sets both ends of the selection, each clamped to a boundary.
    pub fn set_selection(&mut self, anchor: usize, focus: usize) {
        self.anchor = self.clamp_boundary(anchor);
        self.focus = self.clamp_boundary(focus);
    }

    /// Selects everything.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.focus = self.text.len();
    }

    /// Moves the caret, extending the selection when `extend` is set.
    pub fn move_caret(&mut self, motion: Motion, extend: bool) {
        // Collapsing to an edge rather than moving is what a plain arrow key
        // does to a selection in every text field anywhere, and getting it wrong
        // is immediately noticeable.
        if !extend && self.has_selection() {
            let r = self.selection();
            let to = match motion {
                Motion::Left | Motion::WordLeft => Some(r.start),
                Motion::Right | Motion::WordRight => Some(r.end),
                _ => None,
            };
            if let Some(to) = to {
                self.set_caret(to);
                return;
            }
        }

        let target = match motion {
            Motion::Left => self.prev_grapheme(self.focus),
            Motion::Right => self.next_grapheme(self.focus),
            Motion::WordLeft => self.prev_word(self.focus),
            Motion::WordRight => self.next_word(self.focus),
            Motion::Home => 0,
            Motion::End => self.text.len(),
        };
        self.focus = target;
        if !extend {
            self.anchor = target;
        }
    }

    /// Replaces the selection with `insert`, or inserts at the caret.
    ///
    /// Refused outright while composing; see [`TextEdit::is_composing`].
    /// Returns whether anything changed.
    pub fn insert(&mut self, insert: &str) -> bool {
        if self.is_composing() {
            return false;
        }
        let range = self.selection();
        if insert.is_empty() && range.is_empty() {
            return false;
        }
        self.replace_range(range, insert);
        true
    }

    /// Deletes the selection, or one grapheme before the caret.
    pub fn backspace(&mut self) -> bool {
        if self.is_composing() {
            return false;
        }
        let range = self.selection();
        let range =
            if range.is_empty() { self.prev_grapheme(self.focus)..self.focus } else { range };
        if range.is_empty() {
            return false;
        }
        self.replace_range(range, "");
        true
    }

    /// Deletes the selection, or one grapheme after the caret.
    pub fn delete_forward(&mut self) -> bool {
        if self.is_composing() {
            return false;
        }
        let range = self.selection();
        let range =
            if range.is_empty() { self.focus..self.next_grapheme(self.focus) } else { range };
        if range.is_empty() {
            return false;
        }
        self.replace_range(range, "");
        true
    }

    // ------------------------------------------------------- input method

    /// Replaces the provisional composition text.
    ///
    /// The first call replaces the current selection, which is what makes
    /// composing over selected text behave like typing over it. Later calls
    /// replace the previous provisional text, so a composition that grows a
    /// character at a time does not accumulate.
    ///
    /// An empty `text` ends the composition without committing anything: that is
    /// how a platform signals that the user backed out of it.
    pub fn set_preedit(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        let range = match &self.preedit {
            Some(p) => p.range.clone(),
            None => self.selection(),
        };

        if text.is_empty() {
            self.text.replace_range(range.clone(), "");
            self.preedit = None;
            self.set_caret(range.start);
            return;
        }

        self.text.replace_range(range.clone(), text);
        let start = range.start;
        let end = start + text.len();
        self.preedit = Some(Preedit { range: start..end, cursor });

        // The platform reports its cursor relative to the pre-edit string. A
        // hidden cursor still needs the caret parked somewhere sane, and the end
        // of the composition is where the next keystroke will land.
        match cursor {
            Some((a, b)) => {
                self.anchor = self.clamp_boundary(start + a);
                self.focus = self.clamp_boundary(start + b);
            }
            None => {
                self.anchor = end;
                self.focus = end;
            }
        }
    }

    /// Commits text from the input method.
    ///
    /// Replaces the provisional text if there is any, and the selection if there
    /// is not — a commit can arrive with no preceding pre-edit, which is how
    /// several platforms deliver a single-keystroke conversion.
    pub fn commit(&mut self, text: &str) {
        let range = match self.preedit.take() {
            Some(p) => p.range,
            None => self.selection(),
        };
        self.replace_range(range, text);
    }

    /// Abandons the composition and removes its provisional text.
    ///
    /// Not a commit. An input method that wanted the text kept sends a commit
    /// first, and treating this as one inserts half-composed text every time the
    /// user presses Escape.
    pub fn cancel_composition(&mut self) -> bool {
        let Some(p) = self.preedit.take() else { return false };
        self.text.replace_range(p.range.clone(), "");
        self.set_caret(p.range.start);
        true
    }

    // ------------------------------------------------------------ internals

    /// Splices `with` into `range` and puts the caret after it.
    fn replace_range(&mut self, range: Range<usize>, with: &str) {
        let with = self.fit(range.clone(), with);
        let caret = range.start + with.len();
        self.text.replace_range(range, &with);
        self.anchor = caret;
        self.focus = caret;
    }

    /// Trims an insertion to whatever the length cap leaves room for.
    ///
    /// Truncated at a character boundary, not a byte one, so capping a field
    /// cannot produce invalid UTF-8 out of a multi-byte paste.
    fn fit(&self, range: Range<usize>, with: &str) -> String {
        let Some(max) = self.max_len else { return with.to_string() };
        let after_removal = self.text.len() - (range.end - range.start);
        let room = max.saturating_sub(after_removal);
        if with.len() <= room {
            return with.to_string();
        }
        let mut cut = room;
        while cut > 0 && !with.is_char_boundary(cut) {
            cut -= 1;
        }
        with[..cut].to_string()
    }

    fn truncate_to_max(&mut self) {
        let Some(max) = self.max_len else { return };
        if self.text.len() <= max {
            return;
        }
        let mut cut = max;
        while cut > 0 && !self.text.is_char_boundary(cut) {
            cut -= 1;
        }
        self.text.truncate(cut);
    }

    /// Clamps to the nearest character boundary at or below `byte`.
    fn clamp_boundary(&self, byte: usize) -> usize {
        let mut b = byte.min(self.text.len());
        while b > 0 && !self.text.is_char_boundary(b) {
            b -= 1;
        }
        b
    }

    fn prev_grapheme(&self, from: usize) -> usize {
        let from = self.clamp_boundary(from);
        let mut cursor = GraphemeCursor::new(from, self.text.len(), true);
        cursor.prev_boundary(&self.text, 0).ok().flatten().unwrap_or(0)
    }

    fn next_grapheme(&self, from: usize) -> usize {
        let from = self.clamp_boundary(from);
        let mut cursor = GraphemeCursor::new(from, self.text.len(), true);
        cursor.next_boundary(&self.text, 0).ok().flatten().unwrap_or(self.text.len())
    }

    /// Start of the word before `from`.
    ///
    /// Skips the whitespace immediately behind the caret first, so pressing
    /// Ctrl+Left at the start of a word lands on the previous word rather than
    /// stopping on the space between them.
    fn prev_word(&self, from: usize) -> usize {
        let from = self.clamp_boundary(from);
        self.text[..from]
            .split_word_bound_indices()
            .rfind(|(_, w)| !w.chars().all(char::is_whitespace))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Start of the word after `from`.
    fn next_word(&self, from: usize) -> usize {
        let from = self.clamp_boundary(from);
        self.text[from..]
            .split_word_bound_indices()
            .find(|(i, w)| *i > 0 && !w.chars().all(char::is_whitespace))
            .map(|(i, _)| from + i)
            .unwrap_or(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_buffer_is_empty_with_the_caret_at_zero() {
        let e = TextEdit::new();
        assert!(e.is_empty());
        assert_eq!(e.caret(), 0);
        assert!(!e.has_selection());
        assert!(!e.is_composing());
    }

    #[test]
    fn from_text_puts_the_caret_at_the_end() {
        let e = TextEdit::from_text("hello");
        assert_eq!(e.caret(), 5);
        assert_eq!(e.text(), "hello");
    }

    #[test]
    fn insert_replaces_the_selection() {
        let mut e = TextEdit::from_text("hello world");
        e.set_selection(6, 11);
        assert_eq!(e.selected_text(), "world");
        e.insert("there");
        assert_eq!(e.text(), "hello there");
        assert_eq!(e.caret(), 11);
        assert!(!e.has_selection());
    }

    #[test]
    fn backspace_removes_a_whole_grapheme_cluster() {
        // `e` + combining acute is two chars and three bytes, and one press of
        // backspace. Deleting by `char` would leave a stranded combining mark.
        let mut e = TextEdit::from_text("e\u{301}");
        assert_eq!(e.len(), 3);
        e.backspace();
        assert_eq!(e.text(), "", "the combining mark went with its base");
    }

    #[test]
    fn arrow_keys_move_by_grapheme_not_by_char() {
        let mut e = TextEdit::from_text("a\u{301}b");
        e.set_caret(0);
        e.move_caret(Motion::Right, false);
        assert_eq!(e.caret(), 3, "one press crosses the whole cluster");
    }

    #[test]
    fn a_plain_arrow_collapses_a_selection_to_its_edge() {
        let mut e = TextEdit::from_text("hello");
        e.set_selection(1, 4);
        e.move_caret(Motion::Left, false);
        assert_eq!(e.caret(), 1, "collapses to the start, does not step left from it");
        assert!(!e.has_selection());

        e.set_selection(1, 4);
        e.move_caret(Motion::Right, false);
        assert_eq!(e.caret(), 4);
    }

    #[test]
    fn shift_arrow_extends_from_the_anchor() {
        let mut e = TextEdit::from_text("hello");
        e.set_caret(2);
        e.move_caret(Motion::Right, true);
        assert_eq!(e.selection(), 2..3);
        e.move_caret(Motion::Right, true);
        assert_eq!(e.selection(), 2..4);
        e.move_caret(Motion::Left, true);
        assert_eq!(e.selection(), 2..3);
    }

    #[test]
    fn word_motion_skips_the_space_behind_the_caret() {
        let mut e = TextEdit::from_text("alpha beta gamma");
        e.set_caret(11); // start of "gamma"
        e.move_caret(Motion::WordLeft, false);
        assert_eq!(e.caret(), 6, "landed on `beta`, not on the space before it");
        e.move_caret(Motion::WordRight, false);
        assert_eq!(e.caret(), 11);
    }

    #[test]
    fn home_and_end_go_to_the_extremes() {
        let mut e = TextEdit::from_text("hello");
        e.move_caret(Motion::Home, false);
        assert_eq!(e.caret(), 0);
        e.move_caret(Motion::End, false);
        assert_eq!(e.caret(), 5);
    }

    // ------------------------------------------------------ composition

    #[test]
    fn a_preedit_is_visible_in_the_buffer_but_not_in_the_committed_text() {
        // The whole point: the user has to see what they are composing, in
        // place, while a consumer of the value must not.
        let mut e = TextEdit::from_text("ab");
        e.set_caret(1);
        e.set_preedit("\u{304B}", None);
        assert_eq!(e.text(), "a\u{304B}b");
        assert_eq!(e.committed_text(), "ab");
        assert!(e.is_composing());
    }

    #[test]
    fn successive_preedits_replace_rather_than_accumulate() {
        let mut e = TextEdit::new();
        e.set_preedit("\u{304B}", None);
        e.set_preedit("\u{304B}\u{306A}", None);
        assert_eq!(e.text(), "\u{304B}\u{306A}", "the second replaced the first");
        assert_eq!(e.preedit().unwrap().range, 0..6);
    }

    #[test]
    fn the_first_preedit_replaces_the_selection() {
        let mut e = TextEdit::from_text("hello");
        e.set_selection(0, 5);
        e.set_preedit("\u{304B}", None);
        assert_eq!(e.text(), "\u{304B}");
    }

    #[test]
    fn an_empty_preedit_ends_the_composition_without_committing() {
        let mut e = TextEdit::from_text("ab");
        e.set_caret(1);
        e.set_preedit("\u{304B}", None);
        e.set_preedit("", None);
        assert_eq!(e.text(), "ab", "provisional text was removed");
        assert!(!e.is_composing());
        assert_eq!(e.caret(), 1);
    }

    #[test]
    fn commit_replaces_the_provisional_text_and_ends_composition() {
        let mut e = TextEdit::from_text("ab");
        e.set_caret(1);
        e.set_preedit("\u{304B}", None);
        e.commit("\u{9593}");
        assert_eq!(e.text(), "a\u{9593}b");
        assert!(!e.is_composing());
        assert_eq!(e.caret(), 1 + "\u{9593}".len());
        assert_eq!(e.committed_text(), "a\u{9593}b");
    }

    #[test]
    fn a_commit_with_no_preedit_replaces_the_selection() {
        // Several platforms deliver a single-keystroke conversion this way.
        let mut e = TextEdit::from_text("hello");
        e.set_selection(0, 5);
        e.commit("\u{9593}");
        assert_eq!(e.text(), "\u{9593}");
    }

    #[test]
    fn cancelling_a_composition_discards_it_rather_than_committing() {
        // Escape during composition must not leave half-composed syllables.
        let mut e = TextEdit::from_text("ab");
        e.set_caret(1);
        e.set_preedit("\u{304B}\u{306A}", None);
        assert!(e.cancel_composition());
        assert_eq!(e.text(), "ab");
        assert!(!e.is_composing());
        assert_eq!(e.caret(), 1);
    }

    #[test]
    fn cancelling_with_no_composition_does_nothing() {
        let mut e = TextEdit::from_text("ab");
        assert!(!e.cancel_composition());
        assert_eq!(e.text(), "ab");
    }

    #[test]
    fn the_preedit_cursor_is_mapped_out_of_preedit_space_into_the_buffer() {
        let mut e = TextEdit::from_text("ab");
        e.set_caret(1);
        // Two 3-byte codepoints; the IME selects the second one.
        e.set_preedit("\u{304B}\u{306A}", Some((3, 6)));
        assert_eq!(e.selection(), 4..7, "offsets are relative to the preedit start");
    }

    #[test]
    fn a_hidden_preedit_cursor_parks_the_caret_at_the_end() {
        let mut e = TextEdit::new();
        e.set_preedit("\u{304B}", None);
        assert_eq!(e.caret(), 3);
        assert_eq!(e.preedit().unwrap().cursor, None, "hidden is preserved, not turned into zero");
    }

    #[test]
    fn ordinary_edits_are_refused_while_composing() {
        // They would splice into a range the input method believes it owns, and
        // the next pre-edit would then replace the wrong bytes.
        let mut e = TextEdit::new();
        e.set_preedit("\u{304B}", None);
        assert!(!e.insert("x"));
        assert!(!e.backspace());
        assert!(!e.delete_forward());
        assert_eq!(e.text(), "\u{304B}");
    }

    // ------------------------------------------------------------ limits

    #[test]
    fn a_length_cap_truncates_at_a_character_boundary() {
        // Cutting at a byte boundary would produce invalid UTF-8 and panic on
        // the next slice.
        let mut e = TextEdit::new().with_max_len(4);
        e.insert("\u{304B}\u{306A}");
        assert_eq!(e.text(), "\u{304B}", "3 bytes fit, 6 do not, and the cut is legal");
        assert!(e.text().is_char_boundary(e.len()));
    }

    #[test]
    fn a_capped_field_still_accepts_a_replacement_of_the_same_size() {
        let mut e = TextEdit::from_text("abcd").with_max_len(4);
        e.select_all();
        e.insert("wxyz");
        assert_eq!(e.text(), "wxyz");
    }

    #[test]
    fn setting_text_drops_any_composition() {
        let mut e = TextEdit::new();
        e.set_preedit("\u{304B}", None);
        e.set_text("replaced");
        assert!(!e.is_composing());
        assert_eq!(e.text(), "replaced");
    }

    #[test]
    fn offsets_are_clamped_to_character_boundaries() {
        let mut e = TextEdit::from_text("\u{304B}");
        e.set_caret(1); // inside the 3-byte codepoint
        assert_eq!(e.caret(), 0, "clamped down to a boundary rather than panicking");
        e.set_caret(99);
        assert_eq!(e.caret(), 3);
    }

    #[test]
    fn select_all_covers_the_whole_buffer() {
        let mut e = TextEdit::from_text("hello");
        e.select_all();
        assert_eq!(e.selection(), 0..5);
        assert_eq!(e.selected_text(), "hello");
    }
}
