//! An on-screen keyboard.
//!
//! For the machines that have a touchscreen and no keys: a kiosk, a control
//! surface, a tablet running a plug-in editor. It is a normal widget — it takes
//! no focus, owns no text, and reports presses through a callback — so the
//! application decides what a key does to what.
//!
//! ```ignore
//! let editing = self.editing.clone();
//! virtual_keyboard()
//!     .id("keys")
//!     .on_key(move |press| {
//!         press.apply(&mut editing.borrow_mut());
//!     })
//! ```
//!
//! ## Why it does not take focus
//!
//! A keyboard that took keyboard focus would take it *from the field it is
//! typing into*, and the field would stop drawing its caret the moment the
//! first key was pressed. So the whole widget is unfocusable, and every press
//! is delivered to whatever the application is already editing. This is the
//! same reason a system on-screen keyboard is a separate, non-activating
//! window.
//!
//! ## Why it paints its own keys
//!
//! Thirty-odd keys as thirty-odd child elements would be thirty-odd layout
//! nodes rebuilt every frame, each with its own callback, for a grid whose
//! geometry is a division. The rows are data, the rects come from one function
//! that both painting and hit testing call, and a key can never be drawn
//! somewhere other than where it can be pressed.

use crate::element::{Element, EventContext, PaintContext};
use crate::event::{EventFlow, MouseButton, UiEvent};
use crate::semantics::{Role, Semantics};
use crate::style::PaintStyle;
use spherekit_core::{Color, Corners, ElementId, Point, Px, Rect, RoundedRect, Size, px, relative};
use spherekit_layout::Style;
use spherekit_render::TextRasterMode;

/// Which set of keys is showing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum KeyboardLayer {
    /// Letters, with a shift key.
    #[default]
    Letters,
    /// Punctuation and the symbols a password field tends to need.
    Symbols,
    /// Digits alone, in a phone-style block.
    Numeric,
}

/// One key on the on-screen keyboard.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VirtualKey {
    /// A character key, carrying both cases.
    ///
    /// Both, rather than one plus a case transform, because the shifted form of
    /// a key is not always its uppercase: `,` shifts to `!` on this layout, and
    /// `char::to_uppercase` has no opinion about that.
    Char {
        /// What the key produces unshifted.
        lower: &'static str,
        /// What it produces with shift held or caps on.
        upper: &'static str,
    },
    /// A space.
    Space,
    /// Delete backwards.
    Backspace,
    /// Commit the line.
    Enter,
    /// Toggle shift; pressed twice quickly, latch caps.
    Shift,
    /// Switch to another layer.
    Layer(KeyboardLayer),
    /// Move the caret one grapheme left.
    Left,
    /// Move the caret one grapheme right.
    Right,
    /// Ask the application to put the keyboard away.
    Hide,
}

/// What a press means to the text being edited.
///
/// Deliberately not a [`crate::event::Key`]: a key event has a physical key, a
/// repeat flag and a modifier set, none of which an on-screen key has. Handing
/// back something that pretended to be a real key press would invite an
/// application to route it through a shortcut table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyPress {
    /// Insert this text. Always a complete grapheme.
    Text(String),
    /// Delete backwards.
    Backspace,
    /// The commit key.
    Enter,
    /// Caret one grapheme left.
    Left,
    /// Caret one grapheme right.
    Right,
    /// The user asked for the keyboard to go away.
    Hide,
}

impl KeyPress {
    /// Applies this press to an editable string.
    ///
    /// Returns whether the text or the caret actually changed, which is what
    /// decides whether a repaint is owed. [`KeyPress::Enter`] and
    /// [`KeyPress::Hide`] change nothing here — they are the application's to
    /// interpret — and report `false`.
    pub fn apply(&self, edit: &mut crate::edit::TextEdit) -> bool {
        match self {
            KeyPress::Text(text) => edit.insert(text),
            KeyPress::Backspace => edit.backspace(),
            KeyPress::Left => {
                edit.move_caret(crate::edit::Motion::Left, false);
                true
            }
            KeyPress::Right => {
                edit.move_caret(crate::edit::Motion::Right, false);
                true
            }
            KeyPress::Enter | KeyPress::Hide => false,
        }
    }
}

/// A key and how many units of the row it takes.
type Key = (VirtualKey, f32);

/// Shorthand for a letter whose shifted form is its uppercase.
const fn letter(lower: &'static str, upper: &'static str) -> Key {
    (VirtualKey::Char { lower, upper }, 1.0)
}

/// The letter layer, in the arrangement every phone uses.
const LETTER_ROWS: &[&[Key]] = &[
    &[
        letter("q", "Q"),
        letter("w", "W"),
        letter("e", "E"),
        letter("r", "R"),
        letter("t", "T"),
        letter("y", "Y"),
        letter("u", "U"),
        letter("i", "I"),
        letter("o", "O"),
        letter("p", "P"),
    ],
    &[
        letter("a", "A"),
        letter("s", "S"),
        letter("d", "D"),
        letter("f", "F"),
        letter("g", "G"),
        letter("h", "H"),
        letter("j", "J"),
        letter("k", "K"),
        letter("l", "L"),
    ],
    &[
        (VirtualKey::Shift, 1.5),
        letter("z", "Z"),
        letter("x", "X"),
        letter("c", "C"),
        letter("v", "V"),
        letter("b", "B"),
        letter("n", "N"),
        letter("m", "M"),
        (VirtualKey::Backspace, 1.5),
    ],
    &[
        (VirtualKey::Layer(KeyboardLayer::Symbols), 1.5),
        letter(",", "!"),
        (VirtualKey::Space, 4.0),
        letter(".", "?"),
        (VirtualKey::Left, 1.0),
        (VirtualKey::Right, 1.0),
        (VirtualKey::Enter, 1.5),
    ],
];

/// The symbol layer.
const SYMBOL_ROWS: &[&[Key]] = &[
    &[
        letter("1", "1"),
        letter("2", "2"),
        letter("3", "3"),
        letter("4", "4"),
        letter("5", "5"),
        letter("6", "6"),
        letter("7", "7"),
        letter("8", "8"),
        letter("9", "9"),
        letter("0", "0"),
    ],
    &[
        letter("-", "_"),
        letter("/", "\\"),
        letter(":", ";"),
        letter("(", ")"),
        letter("$", "&"),
        letter("@", "#"),
        letter("\"", "'"),
        letter("*", "+"),
        letter("=", "%"),
    ],
    &[
        (VirtualKey::Shift, 1.5),
        letter("[", "{"),
        letter("]", "}"),
        letter("<", ">"),
        letter("^", "~"),
        letter("|", "`"),
        letter("!", "?"),
        letter(".", ","),
        (VirtualKey::Backspace, 1.5),
    ],
    &[
        (VirtualKey::Layer(KeyboardLayer::Letters), 1.5),
        (VirtualKey::Layer(KeyboardLayer::Numeric), 1.0),
        (VirtualKey::Space, 5.0),
        (VirtualKey::Left, 1.0),
        (VirtualKey::Right, 1.0),
        (VirtualKey::Enter, 1.5),
    ],
];

/// The numeric layer: a phone pad, for the fields that only take digits.
const NUMERIC_ROWS: &[&[Key]] = &[
    &[letter("1", "1"), letter("2", "2"), letter("3", "3")],
    &[letter("4", "4"), letter("5", "5"), letter("6", "6")],
    &[letter("7", "7"), letter("8", "8"), letter("9", "9")],
    &[
        (VirtualKey::Layer(KeyboardLayer::Letters), 1.0),
        letter("0", "0"),
        (VirtualKey::Backspace, 1.0),
    ],
];

/// The rows of one layer.
fn rows(layer: KeyboardLayer) -> &'static [&'static [Key]] {
    match layer {
        KeyboardLayer::Letters => LETTER_ROWS,
        KeyboardLayer::Symbols => SYMBOL_ROWS,
        KeyboardLayer::Numeric => NUMERIC_ROWS,
    }
}

/// Scratch slot holding the visible layer.
const SCRATCH_LAYER: usize = 0;
/// Scratch slot holding the shift state: 0 off, 1 for one key, 2 latched.
const SCRATCH_SHIFT: usize = 1;
/// Scratch slot holding the pressed key, one-based so zero means none.
const SCRATCH_PRESSED: usize = 2;
/// Scratch slot holding the hovered key, one-based so zero means none.
const SCRATCH_HOVER: usize = 3;

/// Shift is off.
const SHIFT_OFF: f32 = 0.0;
/// Shift applies to the next character key only.
const SHIFT_ONCE: f32 = 1.0;
/// Shift is latched until pressed again.
const SHIFT_CAPS: f32 = 2.0;

/// The gap between two keys, in logical pixels.
const KEY_GAP: f32 = 5.0;
/// The height of the whole keyboard when nothing else is said.
const DEFAULT_HEIGHT: f32 = 250.0;

/// An on-screen keyboard.
///
/// See the [module documentation](self) for why it neither takes focus nor owns
/// any text.
pub struct VirtualKeyboard {
    id: Option<ElementId>,
    /// The layer the keyboard starts on, before the user switches.
    initial_layer: KeyboardLayer,
    /// When set, the layer cannot be switched away from.
    locked_layer: bool,
    style: Style,
    paint: PaintStyle,
    on_key: Option<Box<dyn FnMut(KeyPress)>>,
}

/// Creates a [`VirtualKeyboard`].
pub fn virtual_keyboard() -> VirtualKeyboard {
    VirtualKeyboard {
        id: None,
        initial_layer: KeyboardLayer::Letters,
        locked_layer: false,
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
        on_key: None,
    }
}

/// A [`VirtualKeyboard`] restricted to digits.
///
/// For a field that takes a number: showing the letter layer first and making
/// the user find the layer key is the difference between one press and three.
pub fn numeric_keyboard() -> VirtualKeyboard {
    VirtualKeyboard {
        initial_layer: KeyboardLayer::Numeric,
        locked_layer: true,
        ..virtual_keyboard()
    }
}

impl VirtualKeyboard {
    /// Gives the keyboard a stable identity.
    ///
    /// Worth setting: the shift and layer state live on the node, and without
    /// an id the node is identified by position — so inserting a sibling above
    /// the keyboard resets it to lowercase letters mid-word.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// The layer to show first.
    pub fn layer(mut self, layer: KeyboardLayer) -> Self {
        self.initial_layer = layer;
        self
    }

    /// Prevents the user from leaving the current layer.
    pub fn locked(mut self, locked: bool) -> Self {
        self.locked_layer = locked;
        self
    }

    /// Sets the keyboard's height, overriding the default.
    pub fn height(mut self, height: impl Into<spherekit_core::Length>) -> Self {
        self.style.size.height = height.into();
        self
    }

    /// Runs on every key that produces text or moves the caret.
    ///
    /// Shift and the layer keys never reach it: they change what the keyboard
    /// shows, which is the widget's own business.
    pub fn on_key(mut self, f: impl FnMut(KeyPress) + 'static) -> Self {
        self.on_key = Some(Box::new(f));
        self
    }

    /// The layer this node is currently showing.
    ///
    /// Scratch starts zeroed, and zero has to keep meaning "the user has not
    /// switched layers yet" — otherwise a keyboard asked to open on digits
    /// would open on letters instead.
    fn current_layer(&self, scratch: &[f32; 4]) -> KeyboardLayer {
        if self.locked_layer {
            return self.initial_layer;
        }
        match scratch[SCRATCH_LAYER] {
            LAYER_LETTERS => KeyboardLayer::Letters,
            LAYER_SYMBOLS => KeyboardLayer::Symbols,
            LAYER_NUMERIC => KeyboardLayer::Numeric,
            _ => self.initial_layer,
        }
    }
}

/// Scratch value for the letter layer. Not zero: zero means "never set", and
/// that has to keep meaning "whatever the caller asked for".
const LAYER_LETTERS: f32 = 1.0;
/// Scratch value for the symbol layer.
const LAYER_SYMBOLS: f32 = 2.0;
/// Scratch value for the numeric layer.
const LAYER_NUMERIC: f32 = 3.0;

/// Where every key of a layer sits inside `bounds`.
///
/// The single source of both the painted rect and the pressed one. Rows are
/// laid out by unit width, so a 1.5-unit shift key is exactly half again as
/// wide as a letter however wide the keyboard is.
pub fn key_rects(bounds: Rect<Px>, layer: KeyboardLayer) -> Vec<(Rect<Px>, VirtualKey)> {
    let rows = rows(layer);
    let mut out = Vec::with_capacity(rows.iter().map(|r| r.len()).sum());
    if bounds.is_empty() || rows.is_empty() {
        return out;
    }
    let gap = Px(KEY_GAP);
    let row_height =
        Px((bounds.height().get() - gap.get() * (rows.len() as f32 - 1.0)) / rows.len() as f32);
    if row_height <= Px::ZERO {
        return out;
    }

    for (index, row) in rows.iter().enumerate() {
        let units: f32 = row.iter().map(|(_, w)| *w).sum();
        if units <= 0.0 {
            continue;
        }
        let gaps = gap.get() * (row.len() as f32 - 1.0);
        // One unit is what is left of the row once the gaps have taken theirs,
        // divided by the units the row asks for. Solving it per row rather than
        // once is what lets a nine-key row centre itself under a ten-key one.
        let unit = (bounds.width().get() - gaps) / units;
        let width = bounds.width().get();
        let used = unit * units + gaps;
        let mut x = bounds.min_x() + Px((width - used) * 0.5);
        let y = bounds.min_y() + Px((row_height.get() + gap.get()) * index as f32);

        for (key, key_units) in row.iter() {
            let key_width = Px(unit * key_units);
            out.push((Rect::new(Point::new(x, y), Size::new(key_width, row_height)), *key));
            x = x + key_width + gap;
        }
    }
    out
}

/// The face a key shows, given the shift state.
fn key_label(key: VirtualKey, shifted: bool) -> &'static str {
    match key {
        VirtualKey::Char { lower, upper } => {
            if shifted {
                upper
            } else {
                lower
            }
        }
        VirtualKey::Space => "space",
        VirtualKey::Backspace => "back",
        VirtualKey::Enter => "enter",
        VirtualKey::Shift => "shift",
        VirtualKey::Layer(KeyboardLayer::Letters) => "abc",
        VirtualKey::Layer(KeyboardLayer::Symbols) => "?#+",
        VirtualKey::Layer(KeyboardLayer::Numeric) => "123",
        VirtualKey::Left => "<",
        VirtualKey::Right => ">",
        VirtualKey::Hide => "hide",
    }
}

/// True for the keys that are chrome rather than characters.
///
/// They are tinted differently for the same reason every phone tints them: a
/// key that deletes and a key that types a letter must not be one glance apart.
fn is_modifier(key: VirtualKey) -> bool {
    !matches!(key, VirtualKey::Char { .. } | VirtualKey::Space)
}

impl crate::element::Styled for VirtualKeyboard {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for VirtualKeyboard {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if matches!(style.size.width, spherekit_core::Length::Auto) {
            style.size.width = relative(1.0);
        }
        if matches!(style.size.height, spherekit_core::Length::Auto) {
            style.size.height = spherekit_core::Length::Px(px(DEFAULT_HEIGHT));
        }
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let c = cx.theme.colors;
        let layer = self.current_layer(&cx.scratch);
        let shifted = cx.scratch[SCRATCH_SHIFT] != SHIFT_OFF;
        let pressed = cx.scratch[SCRATCH_PRESSED] as usize;
        let hovered = cx.scratch[SCRATCH_HOVER] as usize;

        // The backing panel. Without it the keys float over whatever is behind
        // them and the gaps read as holes. A caller's own background wins, so
        // a keyboard can sit on a translucent sheet.
        let mut panel = self.paint.clone();
        if panel.background.is_none() {
            panel.background = Some(c.surface.into());
        }
        if panel.corner_radii.is_zero() {
            panel.corner_radii = Corners::all(cx.theme.radii.lg);
        }
        panel.paint_box(cx.canvas, cx.bounds, cx.state);
        // Under a custom window frame a keyboard drawn over the caption would
        // otherwise drag the window instead of typing.
        cx.keep_interactive();

        let inset = Px(KEY_GAP);
        let inner = Rect::new(
            Point::new(cx.bounds.min_x() + inset, cx.bounds.min_y() + inset),
            Size::new(cx.bounds.width() - inset * 2.0, cx.bounds.height() - inset * 2.0),
        );

        let radius = cx.theme.radii.md;
        let style = spherekit_text::TextStyle {
            font_size: cx.theme.typography.md,
            wrap: spherekit_text::WrapMode::None,
            ..Default::default()
        };

        for (index, (rect, key)) in key_rects(inner, layer).into_iter().enumerate() {
            let is_pressed = pressed == index + 1;
            let is_hovered = hovered == index + 1;
            let latched =
                matches!(key, VirtualKey::Shift) && cx.scratch[SCRATCH_SHIFT] == SHIFT_CAPS;

            let fill = if is_pressed {
                c.accent
            } else if latched {
                c.accent_hover
            } else if is_hovered {
                c.hover
            } else if is_modifier(key) {
                c.elevated
            } else {
                c.background
            };
            cx.canvas.fill_rounded_rect(RoundedRect::new(rect, Corners::all(radius)), fill);

            let text = key_label(key, shifted);
            let ink = if is_pressed || latched { c.text_on_accent } else { c.text };
            let layout = cx.text.layout(text, &style, None);
            let origin = Point::new(
                rect.min_x() + Px((rect.width().get() - layout.size.width.get()) * 0.5),
                rect.min_y() + Px((rect.height().get() - layout.size.height.get()) * 0.5),
            );
            crate::text::draw_layout(
                cx.canvas,
                &layout,
                origin,
                ink,
                TextRasterMode::Auto,
                (Px::ZERO, Color::TRANSPARENT),
                spherekit_render::coverage_contrast_for(ink, fill),
            );
        }
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        let layer = self.current_layer(cx.scratch);
        let inset = Px(KEY_GAP);
        let inner = Rect::new(
            Point::new(cx.bounds.min_x() + inset, cx.bounds.min_y() + inset),
            Size::new(cx.bounds.width() - inset * 2.0, cx.bounds.height() - inset * 2.0),
        );
        let keys = key_rects(inner, layer);
        let at = |position: Point<Px>| keys.iter().position(|(rect, _)| rect.contains(position));

        match cx.event {
            UiEvent::MouseMove(e) => {
                let hover = at(e.position).map(|i| i + 1).unwrap_or(0) as f32;
                if cx.scratch[SCRATCH_HOVER] != hover {
                    cx.scratch[SCRATCH_HOVER] = hover;
                    cx.notify();
                }
                EventFlow::Continue
            }

            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                let Some(index) = at(e.position) else { return EventFlow::Continue };
                cx.scratch[SCRATCH_PRESSED] = index as f32 + 1.0;
                // Captured so a finger that slides off the key still releases
                // here. Without it the release lands on whatever is underneath
                // and the key stays lit for good.
                cx.capture();
                cx.notify();
                EventFlow::Stop
            }

            UiEvent::MouseUp(e) if e.button == MouseButton::Primary => {
                let pressed = cx.scratch[SCRATCH_PRESSED];
                if pressed == 0.0 {
                    return EventFlow::Continue;
                }
                cx.scratch[SCRATCH_PRESSED] = 0.0;
                cx.release();
                cx.notify();

                let index = pressed as usize - 1;
                // Only a release *on* the key it started on counts. Sliding off
                // is how a mispress is taken back, and it is the only way to
                // take one back on a screen with no second button.
                let Some(under) = at(e.position) else { return EventFlow::Stop };
                if under != index {
                    return EventFlow::Stop;
                }
                let Some((_, key)) = keys.get(index).copied() else { return EventFlow::Stop };
                self.activate(cx, key);
                EventFlow::Stop
            }

            // A press that turned into a scroll, or one the system took away.
            // The key must un-light without producing a character.
            UiEvent::PointerCancel(_) | UiEvent::TouchCancel(_) => {
                if cx.scratch[SCRATCH_PRESSED] != 0.0 {
                    cx.scratch[SCRATCH_PRESSED] = 0.0;
                    cx.release();
                    cx.notify();
                }
                EventFlow::Continue
            }

            _ => EventFlow::Continue,
        }
    }

    /// Never focusable: see the [module documentation](self).
    fn focusable(&self) -> bool {
        false
    }

    fn semantics(&self) -> Option<Semantics> {
        Some(Semantics::new(Role::Group, "On-screen keyboard"))
    }
}

impl VirtualKeyboard {
    /// Runs one key, updating shift and layer or reporting a press.
    fn activate(&mut self, cx: &mut EventContext<'_>, key: VirtualKey) {
        let shifted = cx.scratch[SCRATCH_SHIFT] != SHIFT_OFF;
        let press = match key {
            VirtualKey::Shift => {
                cx.scratch[SCRATCH_SHIFT] = match cx.scratch[SCRATCH_SHIFT] {
                    // Shift, then shift again before typing anything, latches
                    // caps. Every touch keyboard does this and it is the only
                    // way to type an acronym without holding anything.
                    SHIFT_ONCE => SHIFT_CAPS,
                    SHIFT_OFF => SHIFT_ONCE,
                    _ => SHIFT_OFF,
                };
                return;
            }
            VirtualKey::Layer(target) => {
                if self.locked_layer {
                    return;
                }
                cx.scratch[SCRATCH_LAYER] = match target {
                    KeyboardLayer::Letters => LAYER_LETTERS,
                    KeyboardLayer::Symbols => LAYER_SYMBOLS,
                    KeyboardLayer::Numeric => LAYER_NUMERIC,
                };
                // A layer switch invalidates every key rect, and the hovered
                // index is an index into the old ones.
                cx.scratch[SCRATCH_HOVER] = 0.0;
                return;
            }
            VirtualKey::Char { lower, upper } => {
                KeyPress::Text(if shifted { upper } else { lower }.to_string())
            }
            VirtualKey::Space => KeyPress::Text(" ".to_string()),
            VirtualKey::Backspace => KeyPress::Backspace,
            VirtualKey::Enter => KeyPress::Enter,
            VirtualKey::Left => KeyPress::Left,
            VirtualKey::Right => KeyPress::Right,
            VirtualKey::Hide => KeyPress::Hide,
        };

        // A one-shot shift is spent by the character it shifted, and by nothing
        // else: backspace with shift pending must not clear it, or the capital
        // the user asked for is lost to a correction.
        if matches!(press, KeyPress::Text(_)) && cx.scratch[SCRATCH_SHIFT] == SHIFT_ONCE {
            cx.scratch[SCRATCH_SHIFT] = SHIFT_OFF;
        }
        if let Some(f) = self.on_key.as_mut() {
            f(press);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{point, rect};

    fn bounds() -> Rect<Px> {
        rect(px(0.0), px(0.0), px(600.0), px(240.0))
    }

    #[test]
    fn every_key_of_a_layer_gets_a_rect() {
        let expected: usize = LETTER_ROWS.iter().map(|r| r.len()).sum();
        assert_eq!(key_rects(bounds(), KeyboardLayer::Letters).len(), expected);
    }

    #[test]
    fn keys_stay_inside_the_keyboard() {
        for layer in [KeyboardLayer::Letters, KeyboardLayer::Symbols, KeyboardLayer::Numeric] {
            for (rect, key) in key_rects(bounds(), layer) {
                assert!(
                    rect.min_x() >= bounds().min_x() - px(0.01),
                    "{key:?} starts left of the box"
                );
                assert!(rect.max_x() <= bounds().max_x() + px(0.01), "{key:?} runs past the right");
                assert!(
                    rect.max_y() <= bounds().max_y() + px(0.01),
                    "{key:?} runs past the bottom"
                );
            }
        }
    }

    #[test]
    fn no_two_keys_overlap() {
        // The property that makes one hit test enough: a point can belong to at
        // most one key, so `position` finding the first is finding the only.
        let keys = key_rects(bounds(), KeyboardLayer::Letters);
        for (i, (a, _)) in keys.iter().enumerate() {
            for (b, _) in keys.iter().skip(i + 1) {
                let overlaps = a.min_x() < b.max_x()
                    && b.min_x() < a.max_x()
                    && a.min_y() < b.max_y()
                    && b.min_y() < a.max_y();
                assert!(!overlaps, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_shorter_row_is_centred_under_a_longer_one() {
        let keys = key_rects(bounds(), KeyboardLayer::Letters);
        let first_row_start = keys[0].0.min_x();
        // The second row has nine keys where the first has ten, so it must be
        // inset on both sides by the same amount rather than left-aligned.
        let second = keys.iter().find(|(_, k)| *k == LETTER_ROWS[1][0].0).expect("row two exists");
        let last_of_second =
            keys.iter().rev().find(|(_, k)| *k == LETTER_ROWS[1][8].0).expect("row two ends");
        let left = second.0.min_x() - first_row_start;
        let right = keys[9].0.max_x() - last_of_second.0.max_x();
        assert!((left.get() - right.get()).abs() < 0.5, "left {left:?} right {right:?}");
    }

    #[test]
    fn a_press_lands_on_the_key_it_is_drawn_over() {
        let keys = key_rects(bounds(), KeyboardLayer::Letters);
        let (rect, key) = keys[3];
        let centre = point(rect.min_x() + rect.width() * 0.5, rect.min_y() + rect.height() * 0.5);
        let hit = keys.iter().find(|(r, _)| r.contains(centre)).expect("the centre hits something");
        assert_eq!(hit.1, key);
    }

    #[test]
    fn a_shifted_key_shows_and_produces_its_upper_form() {
        assert_eq!(key_label(LETTER_ROWS[0][0].0, false), "q");
        assert_eq!(key_label(LETTER_ROWS[0][0].0, true), "Q");
        // Not every shifted key is an uppercase letter, which is the reason the
        // pair is stored rather than computed.
        let comma = LETTER_ROWS[3][1].0;
        assert_eq!(key_label(comma, false), ",");
        assert_eq!(key_label(comma, true), "!");
    }

    #[test]
    fn a_key_press_edits_the_text_it_is_applied_to() {
        let mut edit = crate::edit::TextEdit::from_text("ab");
        edit.set_caret(2);
        assert!(KeyPress::Text("c".into()).apply(&mut edit));
        assert_eq!(edit.text(), "abc");
        assert!(KeyPress::Backspace.apply(&mut edit));
        assert_eq!(edit.text(), "ab");
        KeyPress::Left.apply(&mut edit);
        assert_eq!(edit.caret(), 1);
        // Enter and Hide are the application's to interpret and must not touch
        // the string.
        assert!(!KeyPress::Enter.apply(&mut edit));
        assert_eq!(edit.text(), "ab");
    }

    #[test]
    fn an_empty_box_produces_no_keys_rather_than_panicking() {
        assert!(
            key_rects(rect(px(0.0), px(0.0), px(0.0), px(0.0)), KeyboardLayer::Letters).is_empty()
        );
    }
}
