//! An editable single-line text field.
//!
//! The widget half of [`crate::edit::TextEdit`]: it owns no state of its own
//! beyond the frame, turns pixels into byte offsets and back, and routes
//! platform input-method events into the buffer.
//!
//! ## Where the state lives
//!
//! Elements in this engine are rebuilt every frame, so a field cannot own its
//! own text. It takes a [`TextEdit`] by value each frame, edits that copy, and
//! hands it back through [`TextField::on_change`]; the application stores it and
//! passes it in again next frame. That is the same shape as
//! [`crate::widgets::ValueControl`] taking an `f32` and reporting one, and it
//! keeps the retained tree free of hidden state.
//!
//! ## Composition
//!
//! An input method needs two things from the application, and both are easy to
//! omit and hard to notice missing:
//!
//! 1. **Permission.** Nothing composes until the window is told an editable
//!    thing has focus.
//! 2. **A caret rectangle.** The candidate list has to open under the caret.
//!    Without it, a CJK candidate window opens in a screen corner, which makes
//!    the feature useless for the languages that need it most.
//!
//! A focused field asks for both through [`PaintContext::request_ime`], the tree
//! collects the request, and the application applies it. See `docs/platform.md`.

use crate::edit::{Motion, TextEdit};
use crate::element::{Element, EventContext, PaintContext, Styled};
use crate::event::{EventFlow, ImeEvent, Key, MouseButton, UiEvent};
use crate::semantics::{Role, Semantics};
use crate::style::{Cursor, FocusRing, PaintStyle};
use crate::theme::Theme;
use spherekit_core::{Color, Corners, ElementId, Point, Px, Rect, Size, px, relative};
use spherekit_layout::Style;
use spherekit_render::TextRasterMode;
use spherekit_text::{TextLayout, TextStyle, TextSystem, WrapMode};

/// Horizontal padding inside the field's box.
const PAD_X: f32 = 8.0;
/// Vertical padding inside the field's box.
const PAD_Y: f32 = 4.0;
/// Default height, matching [`crate::widgets::Button`] so a field and a button
/// sitting in the same row line up.
const HEIGHT: f32 = 28.0;
/// Caret width in logical pixels.
const CARET_W: f32 = 1.0;
/// How far the caret is kept from the right edge while scrolling.
const CARET_MARGIN: f32 = 2.0;
/// Thickness of the line under composing text.
const UNDERLINE: f32 = 1.0;

/// Scratch slot holding whether a selection drag is in progress.
const SCRATCH_DRAGGING: usize = 0;
/// Scratch slot holding the byte offset the drag was anchored at.
const SCRATCH_ANCHOR: usize = 1;

/// An editable single-line text field.
pub struct TextField {
    id: Option<ElementId>,
    edit: TextEdit,
    style: Style,
    /// Caller overrides, layered under the field's own appearance.
    paint: PaintStyle,
    placeholder: String,
    disabled: bool,
    /// Replaces every grapheme with a bullet when painting.
    mask: bool,
    on_change: Option<Box<dyn FnMut(&TextEdit)>>,
    on_submit: Option<Box<dyn FnMut(&str)>>,
}

/// Creates a [`TextField`] over an existing buffer.
pub fn text_field(edit: TextEdit) -> TextField {
    TextField {
        id: None,
        edit,
        style: Style::default(),
        paint: PaintStyle::default(),
        placeholder: String::new(),
        disabled: false,
        mask: false,
        on_change: None,
        on_submit: None,
    }
}

impl TextField {
    /// Sets the stable identity used to reconcile this field across frames.
    ///
    /// A field without one cannot hold focus across a rebuild, which in practice
    /// means it loses focus on the first keystroke.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Text shown when the buffer is empty.
    pub fn placeholder(mut self, text: impl Into<String>) -> Self {
        self.placeholder = text.into();
        self
    }

    /// Greys the field out and stops it taking focus or input.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Masks the content, for a password.
    ///
    /// Masking happens at paint time only; the buffer keeps the real text, which
    /// is what makes the caret land in the right place.
    pub fn mask(mut self, mask: bool) -> Self {
        self.mask = mask;
        self
    }

    /// Called whenever the buffer changes, including during composition.
    ///
    /// Read [`TextEdit::committed_text`] rather than [`TextEdit::text`] if the
    /// value feeds anything but the field itself: while an input method is
    /// composing, `text` contains provisional syllables the user has not chosen.
    pub fn on_change(mut self, f: impl FnMut(&TextEdit) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }

    /// Called when Enter is pressed, with the committed text.
    pub fn on_submit(mut self, f: impl FnMut(&str) + 'static) -> Self {
        self.on_submit = Some(Box::new(f));
        self
    }

    /// The text style a field lays out with.
    ///
    /// One function, used by paint *and* by event handling. A click is turned
    /// into a caret index by laying the string out again, and if that used a
    /// different size from the paint the caret would land on the wrong
    /// character — visibly so at the end of a long string.
    fn text_style(theme: &Theme) -> TextStyle {
        TextStyle {
            font: spherekit_text::FontRequest {
                weight: theme.typography.weight,
                ..Default::default()
            },
            font_size: theme.typography.md,
            wrap: WrapMode::None,
            ..Default::default()
        }
    }

    /// The content box, inside the padding.
    fn inner(bounds: Rect<Px>) -> Rect<Px> {
        Rect::new(
            Point::new(bounds.min_x() + px(PAD_X), bounds.min_y() + px(PAD_Y)),
            Size::new(
                (bounds.width() - px(PAD_X * 2.0)).max(Px::ZERO),
                (bounds.height() - px(PAD_Y * 2.0)).max(Px::ZERO),
            ),
        )
    }

    /// What gets laid out, which is not always what is stored.
    fn display_text(&self) -> std::borrow::Cow<'_, str> {
        if !self.mask {
            return std::borrow::Cow::Borrowed(self.edit.text());
        }
        // One bullet per grapheme, not per byte: masking per byte would make a
        // password field leak the byte length of the text it is hiding.
        use unicode_segmentation::UnicodeSegmentation;
        std::borrow::Cow::Owned(self.edit.text().graphemes(true).map(|_| '\u{2022}').collect())
    }

    /// How far the text is scrolled left, in logical pixels.
    ///
    /// A pure function of the caret, the text width and the box width, because
    /// paint is required to be a pure function of state and the offset is needed
    /// during paint. The cost of having no history is that the caret sits
    /// against the right edge once the text overflows, rather than the box
    /// scrolling by the smallest amount that would reveal it. It never hides the
    /// caret, which is the property that actually matters.
    fn scroll_offset(caret_x: Px, text_width: Px, inner_width: Px) -> Px {
        if text_width <= inner_width {
            return Px::ZERO;
        }
        let want = caret_x.get() - inner_width.get() + CARET_MARGIN;
        let max = (text_width.get() - inner_width.get()).max(0.0);
        Px(want.clamp(0.0, max))
    }

    /// Lays the field out and returns the layout with its drawing origin.
    ///
    /// The one place the geometry is derived, so paint and hit testing cannot
    /// disagree about it.
    fn geometry(
        &self,
        bounds: Rect<Px>,
        text: &mut TextSystem,
        theme: &Theme,
    ) -> (TextLayout, Point<Px>) {
        let inner = Self::inner(bounds);
        let style = Self::text_style(theme);
        let display = self.display_text();
        let layout = text.layout(&display, &style, None);
        let caret_x = layout.caret(self.edit.caret()).map(|c| c.x).unwrap_or(Px::ZERO);
        let offset = Self::scroll_offset(caret_x, layout.size.width, inner.width());
        // Vertically centred on the box rather than on the baseline, which is
        // what makes a field with a taller-than-usual font still look centred.
        let top = inner.min_y() + (inner.height() - layout.size.height) * 0.5;
        (layout, Point::new(inner.min_x() - offset, top))
    }

    /// Turns a window position into a byte offset.
    fn byte_at(&self, position: Point<Px>, cx: &mut EventContext<'_>) -> Option<usize> {
        let theme = cx.theme;
        let bounds = cx.bounds;
        let text = cx.text.as_deref_mut()?;
        let (layout, origin) = self.geometry(bounds, text, theme);
        Some(layout.hit(Point::new(position.x - origin.x, position.y - origin.y)))
    }

    /// Reports the buffer upward and asks for a repaint.
    fn changed(&mut self, cx: &mut EventContext<'_>) {
        if let Some(f) = self.on_change.as_mut() {
            f(&self.edit);
        }
        // A repaint, never a relayout: the field's box does not depend on its
        // contents, so typing must not touch the layout engine.
        cx.notify();
    }

    /// Handles one key press. Returns whether it was consumed.
    fn handle_key(&mut self, key: &crate::event::KeyEvent) -> bool {
        let shift = key.modifiers.shift;
        // Ctrl on Windows and Linux, Command on macOS; the platform layer has
        // already resolved which one the user pressed.
        let word = key.modifiers.control || key.modifiers.alt;
        let accel = key.modifiers.control || key.modifiers.meta;

        match &key.key {
            Key::Left => {
                self.edit.move_caret(if word { Motion::WordLeft } else { Motion::Left }, shift);
            }
            Key::Right => {
                self.edit.move_caret(if word { Motion::WordRight } else { Motion::Right }, shift);
            }
            Key::Home => self.edit.move_caret(Motion::Home, shift),
            Key::End => self.edit.move_caret(Motion::End, shift),
            Key::Backspace => {
                if !self.edit.backspace() {
                    return true;
                }
            }
            Key::Delete => {
                if !self.edit.delete_forward() {
                    return true;
                }
            }
            Key::Escape => {
                // Escape cancels a composition and otherwise does not belong to
                // the field: a dialog above it wants to close on Escape, and
                // swallowing it here would break that.
                return self.edit.cancel_composition();
            }
            Key::Enter => {
                if self.edit.is_composing() {
                    return true;
                }
                if let Some(f) = self.on_submit.as_mut() {
                    f(&self.edit.committed_text());
                }
                return true;
            }
            Key::Character(c) if accel && c.eq_ignore_ascii_case("a") => {
                self.edit.select_all();
            }
            _ => return false,
        }
        true
    }
}

impl Styled for TextField {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }
    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for TextField {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if matches!(style.size.height, spherekit_core::Length::Auto) {
            style.size.height = spherekit_core::Length::Px(px(HEIGHT));
        }
        // Fills its slot by default. A field sized to its content would resize
        // as the user types, which no text field anywhere does.
        if matches!(style.size.width, spherekit_core::Length::Auto) {
            style.size.width = relative(1.0);
        }
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let c = cx.theme.colors;
        let bounds = cx.bounds;
        cx.keep_interactive();

        let mut style = PaintStyle {
            background: Some(c.elevated.into()),
            corner_radii: Corners::all(cx.theme.radii.md),
            border_width: px(1.0),
            border_color: c.border,
            focus_ring: Some(FocusRing { color: c.focus, ..FocusRing::default() }),
            ..Default::default()
        };
        if self.disabled {
            style.opacity = 0.45;
        }
        let mut state = cx.state;
        state.disabled = self.disabled;
        style.paint_box(cx.canvas, bounds, state);

        let inner = Self::inner(bounds);
        if inner.is_empty() {
            return;
        }

        // Placeholder: only when there is genuinely nothing, composition
        // included. Showing it behind a composition would double-print.
        if self.edit.is_empty() {
            if !self.placeholder.is_empty() {
                let layout = cx.text.layout(&self.placeholder, &Self::text_style(cx.theme), None);
                let top = inner.min_y() + (inner.height() - layout.size.height) * 0.5;
                crate::text::draw_layout(
                    cx.canvas,
                    &layout,
                    Point::new(inner.min_x(), top),
                    c.text_muted,
                    TextRasterMode::Auto,
                    (Px::ZERO, Color::TRANSPARENT),
                    spherekit_render::coverage_contrast_for(c.text_muted, c.background),
                );
            }
            if cx.state.focused && !self.disabled {
                let caret = Rect::new(
                    Point::new(inner.min_x(), inner.min_y()),
                    Size::new(px(CARET_W), inner.height()),
                );
                cx.canvas.fill_rect(caret, c.accent);
                cx.request_ime(caret);
            }
            return;
        }

        let (layout, origin) = {
            let theme = cx.theme;
            self.geometry(bounds, cx.text, theme)
        };

        // Everything below is clipped to the content box: a long string scrolls
        // under the padding rather than over the border.
        cx.canvas.save();
        cx.canvas.clip_rect(inner);

        if self.edit.has_selection() {
            let mut rects = Vec::new();
            layout.selection_rects(self.edit.selection(), &mut rects);
            for r in rects {
                cx.canvas.fill_rect(
                    r.translate(Size::new(origin.x, origin.y)),
                    c.accent.with_alpha(0.35),
                );
            }
        }

        crate::text::draw_layout(
            cx.canvas,
            &layout,
            origin,
            if self.disabled { c.text_muted } else { c.text },
            TextRasterMode::Auto,
            (Px::ZERO, Color::TRANSPARENT),
            spherekit_render::coverage_contrast_for(
                if self.disabled { c.text_muted } else { c.text },
                c.background,
            ),
        );

        // Composing text is underlined, which is the convention every platform
        // uses to say "this is not committed yet".
        if let Some(pre) = self.edit.preedit() {
            let mut rects = Vec::new();
            layout.selection_rects(pre.range.clone(), &mut rects);
            for r in rects {
                let r = r.translate(Size::new(origin.x, origin.y));
                cx.canvas.fill_rect(
                    Rect::new(
                        Point::new(r.min_x(), r.max_y() - px(UNDERLINE)),
                        Size::new(r.width(), px(UNDERLINE)),
                    ),
                    c.accent,
                );
            }
        }

        if cx.state.focused && !self.disabled {
            // A composition whose cursor the platform asked to hide gets no
            // caret. Drawing one anyway makes it flicker at the start of every
            // composition, which is where a hidden cursor is reported.
            let hidden = self.edit.preedit().is_some_and(|p| p.cursor.is_none());
            if let Some(caret) = layout.caret(self.edit.caret()) {
                let bar = Rect::new(
                    Point::new(origin.x + caret.x, origin.y + caret.top),
                    Size::new(px(CARET_W), caret.height),
                );
                if !hidden {
                    cx.canvas.fill_rect(bar, c.accent);
                }
                // Requested even when the caret is hidden: that is exactly when
                // a candidate window is open and most needs to be placed.
                cx.request_ime(bar);
            }
        }

        cx.canvas.restore();
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.disabled {
            return EventFlow::Continue;
        }
        cx.set_cursor(Cursor::Text);

        match cx.event {
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                cx.focus();
                cx.capture();
                match e.click_count {
                    1 => {
                        if let Some(byte) = self.byte_at(e.position, cx) {
                            self.edit.set_caret(byte);
                            cx.scratch[SCRATCH_ANCHOR] = byte as f32;
                        }
                    }
                    2 => {
                        // Word select: put the caret on the click, then walk out
                        // to the word boundaries either side of it.
                        if let Some(byte) = self.byte_at(e.position, cx) {
                            self.edit.set_caret(byte);
                            self.edit.move_caret(Motion::WordLeft, false);
                            let start = self.edit.caret();
                            self.edit.move_caret(Motion::WordRight, true);
                            cx.scratch[SCRATCH_ANCHOR] = start as f32;
                        }
                    }
                    _ => self.edit.select_all(),
                }
                cx.scratch[SCRATCH_DRAGGING] = 1.0;
                self.changed(cx);
                EventFlow::Stop
            }
            UiEvent::MouseMove(e) if cx.scratch[SCRATCH_DRAGGING] != 0.0 => {
                if let Some(byte) = self.byte_at(e.position, cx) {
                    let anchor = cx.scratch[SCRATCH_ANCHOR] as usize;
                    self.edit.set_selection(anchor, byte);
                    self.changed(cx);
                }
                EventFlow::Stop
            }
            UiEvent::MouseUp(_) => {
                if cx.scratch[SCRATCH_DRAGGING] == 0.0 {
                    return EventFlow::Continue;
                }
                cx.scratch[SCRATCH_DRAGGING] = 0.0;
                cx.release();
                EventFlow::Stop
            }
            UiEvent::Key(key) if key.state.is_pressed() => {
                if self.handle_key(key) {
                    self.changed(cx);
                    EventFlow::Stop
                } else {
                    EventFlow::Continue
                }
            }
            UiEvent::TextInput(t) => {
                // Refused while composing: the input method owns those bytes,
                // and some platforms deliver both a pre-edit and a key event for
                // the same keystroke.
                if self.edit.is_composing() || t.text.is_empty() {
                    return EventFlow::Stop;
                }
                self.edit.insert(&t.text);
                self.changed(cx);
                EventFlow::Stop
            }
            UiEvent::Ime(ime) => {
                match ime {
                    // `Start` allocates nothing: composition begins with the
                    // first pre-edit, and some platforms send `Start` for a
                    // composition that never produces one.
                    ImeEvent::Start => return EventFlow::Stop,
                    ImeEvent::Preedit { text, cursor } => self.edit.set_preedit(text, *cursor),
                    ImeEvent::Commit { text } => self.edit.commit(text),
                    ImeEvent::End => {
                        self.edit.cancel_composition();
                    }
                }
                self.changed(cx);
                EventFlow::Stop
            }
            _ => EventFlow::Continue,
        }
    }

    fn focusable(&self) -> bool {
        !self.disabled
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::new(Role::TextField, self.edit.committed_text().into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{IntoElement, ParentElement, div};
    use crate::event::{ElementState, Modifiers, MouseButtonEvent, MouseMoveEvent, TextInputEvent};
    use crate::tree::UiTree;
    use spherekit_core::{ScaleFactor, size};
    use spherekit_render::{Canvas, Scene};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn viewport() -> Size<Px> {
        size(px(400.0), px(300.0))
    }

    #[test]
    fn a_field_inherits_the_theme_font_weight() {
        let mut theme = Theme::dark();
        theme.typography.weight = spherekit_text::FontWeight::BOLD;
        assert_eq!(TextField::text_style(&theme).font.weight, spherekit_text::FontWeight::BOLD);
    }

    /// A text system with a real face, or `None` on a machine with no fonts.
    fn text_system() -> Option<TextSystem> {
        let mut system = TextSystem::with_system_fonts();
        system.fonts_mut().resolve(&spherekit_text::FontRequest::default())?;
        Some(system)
    }

    fn mount(widget: crate::element::AnyElement) -> UiTree {
        let mut tree = UiTree::new();
        tree.build(div().w(relative(1.0)).h(relative(1.0)).child(widget).into_element());
        tree.compute_layout(viewport()).unwrap();
        tree
    }

    fn paint_with(tree: &mut UiTree, text: &mut TextSystem) -> Scene {
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, text, viewport(), 0.0);
        }
        scene
    }

    fn key(k: Key, shift: bool) -> UiEvent {
        UiEvent::Key(crate::event::KeyEvent {
            key: k,
            state: ElementState::Pressed,
            repeat: false,
            modifiers: Modifiers { shift, ..Modifiers::NONE },
        })
    }

    fn press_at(x: f32, y: f32, count: u8) -> UiEvent {
        UiEvent::MouseDown(MouseButtonEvent {
            position: Point::new(px(x), px(y)),
            button: MouseButton::Primary,
            state: ElementState::Pressed,
            click_count: count,
            modifiers: Modifiers::NONE,
        })
    }

    /// A field wired to a shared buffer, the way an application holds one.
    fn field_over(state: &Rc<RefCell<TextEdit>>) -> crate::element::AnyElement {
        let write = Rc::clone(state);
        text_field(state.borrow().clone())
            .id("field")
            .on_change(move |e| *write.borrow_mut() = e.clone())
            .into_element()
    }

    #[test]
    fn typing_inserts_into_the_buffer() {
        let state = Rc::new(RefCell::new(TextEdit::new()));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&UiEvent::TextInput(TextInputEvent { text: "hi".into() }));
        assert_eq!(state.borrow().text(), "hi");
    }

    #[test]
    fn a_field_is_in_the_tab_order() {
        let state = Rc::new(RefCell::new(TextEdit::new()));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        assert!(tree.focus().focused().is_some(), "a field must be reachable by Tab");
    }

    #[test]
    fn a_disabled_field_refuses_input() {
        let state = Rc::new(RefCell::new(TextEdit::new()));
        let write = Rc::clone(&state);
        let mut tree = mount(
            text_field(TextEdit::new())
                .id("f")
                .disabled(true)
                .on_change(move |e| *write.borrow_mut() = e.clone())
                .into_element(),
        );
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&UiEvent::TextInput(TextInputEvent { text: "x".into() }));
        assert_eq!(state.borrow().text(), "");
    }

    #[test]
    fn arrow_keys_move_the_caret_without_changing_the_text() {
        let state = Rc::new(RefCell::new(TextEdit::from_text("hello")));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&key(Key::Left, false));
        assert_eq!(state.borrow().caret(), 4);
        assert_eq!(state.borrow().text(), "hello");
        tree.dispatch(&key(Key::Home, false));
        assert_eq!(state.borrow().caret(), 0);
    }

    #[test]
    fn shift_arrow_selects() {
        let state = Rc::new(RefCell::new(TextEdit::from_text("hello")));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&key(Key::Left, true));
        assert_eq!(state.borrow().selection(), 4..5);
    }

    #[test]
    fn backspace_deletes_and_enter_submits() {
        let submitted = Rc::new(RefCell::new(String::new()));
        let seen = Rc::clone(&submitted);
        let state = Rc::new(RefCell::new(TextEdit::from_text("ab")));
        let write = Rc::clone(&state);
        let mut tree = mount(
            text_field(state.borrow().clone())
                .id("f")
                .on_change(move |e| *write.borrow_mut() = e.clone())
                .on_submit(move |t| *seen.borrow_mut() = t.to_string())
                .into_element(),
        );
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&key(Key::Backspace, false));
        assert_eq!(state.borrow().text(), "a");
        tree.dispatch(&key(Key::Enter, false));
        assert_eq!(submitted.borrow().as_str(), "a");
    }

    // ------------------------------------------------------- input method

    #[test]
    fn a_composition_runs_start_to_commit_through_the_tree() {
        // The whole IME path, in the order the platform actually delivers it.
        let state = Rc::new(RefCell::new(TextEdit::new()));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);

        tree.dispatch(&UiEvent::Ime(ImeEvent::Start));
        tree.dispatch(&UiEvent::Ime(ImeEvent::Preedit {
            text: "\u{304B}".into(),
            cursor: Some((0, 3)),
        }));
        assert_eq!(state.borrow().text(), "\u{304B}", "provisional text is visible");
        assert_eq!(state.borrow().committed_text(), "", "but is not committed");
        assert!(state.borrow().is_composing());

        tree.dispatch(&UiEvent::Ime(ImeEvent::Preedit {
            text: "\u{304B}\u{306A}".into(),
            cursor: Some((6, 6)),
        }));
        assert_eq!(state.borrow().text(), "\u{304B}\u{306A}", "replaced, not appended");

        tree.dispatch(&UiEvent::Ime(ImeEvent::Commit { text: "\u{4EEE}\u{540D}".into() }));
        assert_eq!(state.borrow().text(), "\u{4EEE}\u{540D}");
        assert_eq!(state.borrow().committed_text(), "\u{4EEE}\u{540D}");
        assert!(!state.borrow().is_composing());
    }

    #[test]
    fn ending_a_composition_discards_it_rather_than_committing() {
        let state = Rc::new(RefCell::new(TextEdit::from_text("ab")));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&UiEvent::Ime(ImeEvent::Preedit { text: "\u{304B}".into(), cursor: None }));
        tree.dispatch(&UiEvent::Ime(ImeEvent::End));
        assert_eq!(state.borrow().text(), "ab", "half-composed text must not survive");
    }

    #[test]
    fn text_input_is_ignored_while_composing() {
        // Some platforms deliver both a pre-edit and a key event for one
        // keystroke; taking both would double every character.
        let state = Rc::new(RefCell::new(TextEdit::new()));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        tree.dispatch(&UiEvent::Ime(ImeEvent::Preedit { text: "\u{304B}".into(), cursor: None }));
        tree.dispatch(&UiEvent::TextInput(TextInputEvent { text: "k".into() }));
        assert_eq!(state.borrow().text(), "\u{304B}");
    }

    #[test]
    fn a_focused_field_asks_for_an_input_method_and_says_where() {
        let Some(mut text) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let state = Rc::new(RefCell::new(TextEdit::from_text("hello")));
        let mut tree = mount(field_over(&state));

        paint_with(&mut tree, &mut text);
        assert_eq!(tree.ime(), None, "an unfocused field must not enable an input method");

        tree.navigate_focus(crate::focus::FocusDirection::Next);
        paint_with(&mut tree, &mut text);
        let ime = tree.ime().expect("a focused field must ask for an input method");
        // The caret must be inside the window, or the candidate list opens
        // somewhere unrelated to what the user is typing into.
        assert!(ime.caret.height() > Px::ZERO, "{:?}", ime.caret);
        assert!(ime.caret.min_y() >= Px::ZERO && ime.caret.max_y() <= px(300.0), "{:?}", ime.caret);
    }

    #[test]
    fn the_request_is_withdrawn_when_focus_leaves() {
        let Some(mut text) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let state = Rc::new(RefCell::new(TextEdit::from_text("hello")));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        paint_with(&mut tree, &mut text);
        assert!(tree.ime().is_some());

        tree.focus_mut().focus(None);
        paint_with(&mut tree, &mut text);
        assert_eq!(tree.ime(), None, "a blurred field must switch the input method off");
    }

    #[test]
    fn an_empty_focused_field_still_places_the_caret() {
        // An empty layout has no line to take a height from, so this is the case
        // that would otherwise report no caret at all.
        let Some(mut text) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let state = Rc::new(RefCell::new(TextEdit::new()));
        let mut tree = mount(field_over(&state));
        tree.navigate_focus(crate::focus::FocusDirection::Next);
        paint_with(&mut tree, &mut text);
        let ime = tree.ime().expect("an empty field still accepts text");
        assert!(ime.caret.height() > Px::ZERO);
    }

    // ------------------------------------------------------------ pointer

    #[test]
    fn clicking_places_the_caret_at_the_click() {
        let Some(mut text) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let state = Rc::new(RefCell::new(TextEdit::from_text("hello world")));
        let mut tree = mount(field_over(&state));
        paint_with(&mut tree, &mut text);

        tree.dispatch_with_text(&press_at(380.0, 14.0, 1), &mut text);
        assert_eq!(state.borrow().caret(), 11, "a click past the text lands at the end");

        tree.dispatch_with_text(&press_at(1.0, 14.0, 1), &mut text);
        assert_eq!(state.borrow().caret(), 0, "a click before the text lands at the start");
    }

    #[test]
    fn a_triple_click_selects_everything() {
        let Some(mut text) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let state = Rc::new(RefCell::new(TextEdit::from_text("hello world")));
        let mut tree = mount(field_over(&state));
        paint_with(&mut tree, &mut text);
        tree.dispatch_with_text(&press_at(40.0, 14.0, 3), &mut text);
        assert_eq!(state.borrow().selection(), 0..11);
    }

    #[test]
    fn dragging_extends_the_selection_from_where_it_started() {
        let Some(mut text) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let state = Rc::new(RefCell::new(TextEdit::from_text("hello world")));
        let mut tree = mount(field_over(&state));
        paint_with(&mut tree, &mut text);

        tree.dispatch_with_text(&press_at(9.0, 14.0, 1), &mut text);
        let anchor = state.borrow().caret();
        tree.dispatch_with_text(
            &UiEvent::MouseMove(MouseMoveEvent {
                position: Point::new(px(380.0), px(14.0)),
                delta: Size::new(px(371.0), Px::ZERO),
                buttons: smallvec::smallvec![MouseButton::Primary],
                modifiers: Modifiers::NONE,
            }),
            &mut text,
        );
        let sel = state.borrow().selection();
        assert!(sel.end > sel.start, "dragging produced no selection");
        assert!(sel.start <= anchor && anchor <= sel.end, "the drag lost its anchor");
    }

    #[test]
    fn a_click_without_a_text_system_does_not_panic() {
        // `dispatch` with no text is a supported path; a field simply cannot hit
        // test on it, and must leave the caret alone rather than crash.
        let state = Rc::new(RefCell::new(TextEdit::from_text("hello")));
        let mut tree = mount(field_over(&state));
        tree.dispatch(&press_at(40.0, 14.0, 1));
        assert_eq!(state.borrow().text(), "hello");
    }

    #[test]
    fn a_masked_field_keeps_its_real_text() {
        let Some(mut text) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let state = Rc::new(RefCell::new(TextEdit::from_text("secret")));
        let write = Rc::clone(&state);
        let mut tree = mount(
            text_field(state.borrow().clone())
                .id("f")
                .mask(true)
                .on_change(move |e| *write.borrow_mut() = e.clone())
                .into_element(),
        );
        let scene = paint_with(&mut tree, &mut text);
        assert!(!scene.is_empty(), "a masked field still draws something");
        assert_eq!(state.borrow().text(), "secret", "the buffer keeps the real text");
    }

    #[test]
    fn scrolling_keeps_the_caret_inside_the_box() {
        let inner = px(100.0);
        assert_eq!(TextField::scroll_offset(px(50.0), px(80.0), inner), Px::ZERO);
        let far = TextField::scroll_offset(px(500.0), px(520.0), inner);
        assert!(far > Px::ZERO, "a caret past the right edge must scroll the text");
        assert!(px(500.0) - far <= inner, "the caret ended up outside the box");
        assert!(far <= px(520.0) - inner, "scrolled past the end of the text");
    }
}

#[cfg(test)]
mod caption_tests {
    use super::*;
    use crate::element::{IntoElement, ParentElement, div};
    use crate::tree::UiTree;
    use spherekit_core::{ScaleFactor, size};
    use spherekit_render::{Canvas, Scene};

    fn viewport() -> Size<Px> {
        size(px(400.0), px(300.0))
    }

    /// An interactive widget in a custom title bar must be excluded from the
    /// caption, or the platform's modal move loop swallows its press and it
    /// receives no click ever.
    #[test]
    fn interactive_widgets_declare_themselves_during_paint() {
        let mut tree = UiTree::new();
        tree.build(
            div()
                .w(relative(1.0))
                .h(relative(1.0))
                .child(crate::widgets::button("Apply").id("apply"))
                .child(text_field(TextEdit::new()).id("field"))
                .into_element(),
        );
        tree.compute_layout(viewport()).unwrap();

        let mut text = TextSystem::new();
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }

        let exclusions = tree.caption_exclusions();
        assert!(
            exclusions.len() >= 2,
            "a button and a field must both declare themselves: {exclusions:?}"
        );
        assert!(exclusions.iter().all(|r| !r.is_empty()), "a zero-area exclusion excludes nothing");
    }

    #[test]
    fn the_exclusion_list_is_rebuilt_on_every_paint() {
        // Kept across frames it would grow without bound, and a widget that has
        // scrolled out of the caption would go on being excluded from it.
        let mut tree = UiTree::new();
        tree.build(crate::widgets::button("Apply").id("apply").into_element());
        tree.compute_layout(viewport()).unwrap();

        let mut text = TextSystem::new();
        let mut paint = |tree: &mut UiTree| {
            let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        };
        paint(&mut tree);
        let first = tree.caption_exclusions().len();
        paint(&mut tree);
        paint(&mut tree);
        assert_eq!(tree.caption_exclusions().len(), first, "the list accumulated across frames");
    }
}
