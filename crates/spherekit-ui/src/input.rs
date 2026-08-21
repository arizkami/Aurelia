//! Translating platform events into UI events.
//!
//! `spherekit-platform` reports what the window system said. `spherekit-ui` needs
//! something richer: a pointer position on every button event, a click count, a
//! resolved modifier set. This module is the one place that gap is closed.
//!
//! Two crates having their own input enums is a deliberate boundary rather than
//! duplication. The platform layer's job is to describe the hardware faithfully
//! and to keep `winit` from leaking; the UI layer's job is to describe an
//! interaction. A platform `MouseInput` carries no position because the window
//! system does not send one; a UI `MouseDown` must carry one, because every
//! handler needs it.

use crate::event::{
    ClickTracker, ElementState, ImeEvent, Key, KeyEvent, Modifiers, MouseButton, MouseButtonEvent,
    MouseMoveEvent, ScrollDelta, ScrollEvent, TextInputEvent, UiEvent,
};
use smallvec::SmallVec;
use spherekit_core::{Point, Px, Size};

/// Accumulates the state the window system does not resend on every event.
///
/// A platform button event has no coordinates and a platform move event has no
/// button set; a UI event needs both. Something has to remember, and it should
/// be one object per window rather than a scattering of fields on an
/// application struct.
#[derive(Debug, Default)]
pub struct InputTranslator {
    position: Point<Px>,
    previous_position: Point<Px>,
    buttons: SmallVec<[MouseButton; 3]>,
    modifiers: Modifiers,
    clicks: ClickTracker,
    /// Milliseconds since start, advanced by the caller.
    now_ms: u64,
    inside: bool,
}

impl InputTranslator {
    /// A translator with no pointer state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the clock used for click-interval detection.
    ///
    /// Taken from the caller rather than read here, because a UI thread already
    /// has a frame clock and two independent clocks would drift.
    #[inline]
    pub fn set_time(&mut self, milliseconds: u64) {
        self.now_ms = milliseconds;
    }

    /// The last known pointer position, in window-logical pixels.
    #[inline]
    pub fn position(&self) -> Point<Px> {
        self.position
    }

    /// The currently held buttons.
    #[inline]
    pub fn buttons(&self) -> &[MouseButton] {
        &self.buttons
    }

    /// The currently held modifiers.
    #[inline]
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// True when the pointer is inside the window.
    #[inline]
    pub fn pointer_inside(&self) -> bool {
        self.inside
    }

    /// Forgets all held state.
    ///
    /// Called when the window loses focus. The key-up and button-up for
    /// anything held during an alt-tab is delivered to whoever has focus next,
    /// so keeping the state would leave a permanently-held phantom modifier.
    pub fn reset(&mut self) {
        self.buttons.clear();
        self.modifiers = Modifiers::NONE;
        self.clicks.reset();
    }

    /// Translates one platform event into zero or more UI events.
    ///
    /// Zero for events the UI layer does not care about — a resize, a redraw
    /// request — which the caller handles itself.
    pub fn translate(&mut self, event: &spherekit_platform::WindowEvent) -> SmallVec<[UiEvent; 2]> {
        use spherekit_platform::WindowEvent as P;
        let mut out: SmallVec<[UiEvent; 2]> = SmallVec::new();

        match event {
            P::CursorMoved(position) => {
                self.previous_position = self.position;
                self.position = *position;
                self.inside = true;
                out.push(UiEvent::MouseMove(self.move_event()));
            }
            P::CursorEntered => {
                self.inside = true;
            }
            P::CursorLeft => {
                self.inside = false;
                // Hover state must be cleared, and there is no further move
                // event to do it. Synthesising a move to somewhere impossible
                // is how every hovered element learns it is no longer hovered.
                self.previous_position = self.position;
                self.position = Point::new(Px(f32::MIN), Px(f32::MIN));
                out.push(UiEvent::MouseMove(self.move_event()));
            }
            P::MouseInput { button, state, modifiers } => {
                self.modifiers = convert_modifiers(*modifiers);
                let button = convert_button(*button);
                let state = convert_state(*state);
                match state {
                    ElementState::Pressed => {
                        if !self.buttons.contains(&button) {
                            self.buttons.push(button);
                        }
                        let count = self.clicks.press(button, self.position, self.now_ms);
                        out.push(UiEvent::MouseDown(MouseButtonEvent {
                            position: self.position,
                            button,
                            state,
                            click_count: count,
                            modifiers: self.modifiers,
                        }));
                    }
                    ElementState::Released => {
                        self.buttons.retain(|b| *b != button);
                        out.push(UiEvent::MouseUp(MouseButtonEvent {
                            position: self.position,
                            button,
                            state,
                            // The release belongs to the click the press began.
                            click_count: 1,
                            modifiers: self.modifiers,
                        }));
                    }
                }
            }
            P::MouseWheel { delta, phase } => {
                out.push(UiEvent::Scroll(ScrollEvent {
                    position: self.position,
                    delta: convert_scroll(*delta),
                    modifiers: self.modifiers,
                    momentum: matches!(phase, spherekit_platform::TouchPhase::Moved),
                }));
            }
            P::KeyboardInput { key, state, repeat, modifiers, .. } => {
                self.modifiers = convert_modifiers(*modifiers);
                out.push(UiEvent::Key(KeyEvent {
                    key: convert_key(key),
                    state: convert_state(*state),
                    repeat: *repeat,
                    modifiers: self.modifiers,
                }));
            }
            P::TextInput(text) => {
                out.push(UiEvent::TextInput(TextInputEvent { text: text.clone() }));
            }
            P::Ime(ime) => {
                if let Some(converted) = convert_ime(ime) {
                    out.push(UiEvent::Ime(converted));
                }
            }
            P::Focused(gained) => {
                if !*gained {
                    self.reset();
                }
                out.push(if *gained { UiEvent::FocusIn } else { UiEvent::FocusOut });
            }
            // Everything else is the application's business, not the tree's.
            _ => {}
        }
        out
    }

    fn move_event(&self) -> MouseMoveEvent {
        MouseMoveEvent {
            position: self.position,
            delta: Size::new(
                self.position.x - self.previous_position.x,
                self.position.y - self.previous_position.y,
            ),
            buttons: self.buttons.clone(),
            modifiers: self.modifiers,
        }
    }
}

fn convert_button(button: spherekit_platform::MouseButton) -> MouseButton {
    use spherekit_platform::MouseButton as P;
    match button {
        P::Left => MouseButton::Primary,
        P::Right => MouseButton::Secondary,
        P::Middle => MouseButton::Middle,
        P::Back => MouseButton::Back,
        P::Forward => MouseButton::Forward,
        P::Other(n) => MouseButton::Other(n),
    }
}

fn convert_state(state: spherekit_platform::ElementState) -> ElementState {
    match state {
        spherekit_platform::ElementState::Pressed => ElementState::Pressed,
        spherekit_platform::ElementState::Released => ElementState::Released,
    }
}

fn convert_modifiers(modifiers: spherekit_platform::Modifiers) -> Modifiers {
    use spherekit_platform::Modifiers as P;
    Modifiers {
        shift: modifiers.contains(P::SHIFT),
        control: modifiers.contains(P::CTRL),
        alt: modifiers.contains(P::ALT),
        meta: modifiers.contains(P::SUPER),
    }
}

fn convert_scroll(delta: spherekit_platform::ScrollDelta) -> ScrollDelta {
    use spherekit_platform::ScrollDelta as P;
    match delta {
        P::Lines { x, y } => ScrollDelta::Lines(Size::new(x, y)),
        P::Pixels { x, y } => ScrollDelta::Pixels(Size::new(x, y)),
    }
}

fn convert_key(key: &spherekit_platform::Key) -> Key {
    use spherekit_platform::{Key as P, NamedKey as N};
    match key {
        P::Character(text) => Key::Character(text.as_str().to_string()),
        P::Named(named) => match named {
            N::Enter => Key::Enter,
            N::Tab => Key::Tab,
            N::Space => Key::Space,
            N::Backspace => Key::Backspace,
            N::Delete => Key::Delete,
            N::Escape => Key::Escape,
            N::ArrowLeft => Key::Left,
            N::ArrowRight => Key::Right,
            N::ArrowUp => Key::Up,
            N::ArrowDown => Key::Down,
            N::Home => Key::Home,
            N::End => Key::End,
            N::PageUp => Key::PageUp,
            N::PageDown => Key::PageDown,
            N::Insert => Key::Insert,
            N::F(n) => Key::Function(*n),
            // A named key with no UI-level meaning still has to round-trip so a
            // handler can match on it; the discriminant is stable enough for
            // that and nothing in the UI layer interprets it.
            other => Key::Unidentified(named_key_code(other)),
        },
        // A dead key produces no text of its own; the composed result arrives
        // as a separate text input event.
        P::Dead(_) => Key::Unidentified(0),
        // `spherekit_platform::Key` is `#[non_exhaustive]`, so a new variant added
        // there must not break this crate's build. Anything unrecognised is
        // still delivered, just without a UI-level meaning.
        _ => Key::Unidentified(0),
    }
}

/// A stable-enough numeric code for a named key the UI layer does not model.
fn named_key_code(named: &spherekit_platform::NamedKey) -> u32 {
    use core::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    named.to_string().hash(&mut hasher);
    hasher.finish() as u32
}

fn convert_ime(ime: &spherekit_platform::ImeEvent) -> Option<ImeEvent> {
    use spherekit_platform::ImeEvent as P;
    Some(match ime {
        P::Enabled => ImeEvent::Start,
        P::Preedit { text, cursor } => ImeEvent::Preedit { text: text.clone(), cursor: *cursor },
        P::Commit(text) => ImeEvent::Commit { text: text.clone() },
        P::Disabled => ImeEvent::End,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::px;
    use spherekit_platform::{
        ElementState as PState, Modifiers as PMods, MouseButton as PButton, ScrollDelta as PScroll,
        TouchPhase, WindowEvent as P,
    };

    fn press(button: PButton) -> P {
        P::MouseInput { button, state: PState::Pressed, modifiers: PMods::empty() }
    }

    fn release(button: PButton) -> P {
        P::MouseInput { button, state: PState::Released, modifiers: PMods::empty() }
    }

    #[test]
    fn a_button_event_gains_the_position_from_the_last_move() {
        // The window system does not send coordinates with a button event; a UI
        // handler cannot work without them.
        let mut t = InputTranslator::new();
        t.translate(&P::CursorMoved(Point::new(px(120.0), px(64.0))));
        let events = t.translate(&press(PButton::Left));
        match &events[0] {
            UiEvent::MouseDown(e) => {
                assert_eq!(e.position, Point::new(px(120.0), px(64.0)));
                assert_eq!(e.button, MouseButton::Primary);
            }
            other => panic!("expected MouseDown, got {other:?}"),
        }
    }

    #[test]
    fn held_buttons_appear_on_subsequent_moves() {
        // What distinguishes a drag from a hover.
        let mut t = InputTranslator::new();
        t.translate(&P::CursorMoved(Point::new(px(10.0), px(10.0))));
        t.translate(&press(PButton::Left));
        let events = t.translate(&P::CursorMoved(Point::new(px(20.0), px(10.0))));
        match &events[0] {
            UiEvent::MouseMove(e) => {
                assert!(e.is_dragging());
                assert_eq!(e.delta, Size::new(px(10.0), px(0.0)));
            }
            other => panic!("expected MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn releasing_clears_the_held_button() {
        let mut t = InputTranslator::new();
        t.translate(&press(PButton::Left));
        assert_eq!(t.buttons(), &[MouseButton::Primary]);
        t.translate(&release(PButton::Left));
        assert!(t.buttons().is_empty());
    }

    #[test]
    fn click_counting_uses_the_supplied_clock() {
        let mut t = InputTranslator::new();
        t.translate(&P::CursorMoved(Point::new(px(50.0), px(50.0))));

        t.set_time(0);
        let first = t.translate(&press(PButton::Left));
        t.translate(&release(PButton::Left));
        t.set_time(100);
        let second = t.translate(&press(PButton::Left));

        let count = |events: &SmallVec<[UiEvent; 2]>| match &events[0] {
            UiEvent::MouseDown(e) => e.click_count,
            other => panic!("expected MouseDown, got {other:?}"),
        };
        assert_eq!(count(&first), 1);
        assert_eq!(count(&second), 2);
    }

    #[test]
    fn a_slow_second_click_is_not_a_double_click() {
        let mut t = InputTranslator::new();
        t.set_time(0);
        t.translate(&press(PButton::Left));
        t.translate(&release(PButton::Left));
        t.set_time(10_000);
        let second = t.translate(&press(PButton::Left));
        match &second[0] {
            UiEvent::MouseDown(e) => assert_eq!(e.click_count, 1),
            other => panic!("expected MouseDown, got {other:?}"),
        }
    }

    #[test]
    fn losing_focus_forgets_every_held_key_and_button() {
        // The key-up for a modifier held during an alt-tab goes to someone
        // else, so keeping it would leave a phantom Shift held forever.
        let mut t = InputTranslator::new();
        t.translate(&P::MouseInput {
            button: PButton::Left,
            state: PState::Pressed,
            modifiers: PMods::SHIFT | PMods::CTRL,
        });
        assert!(t.modifiers().shift);
        assert!(!t.buttons().is_empty());

        t.translate(&P::Focused(false));
        assert!(t.modifiers().is_none());
        assert!(t.buttons().is_empty());
    }

    #[test]
    fn the_cursor_leaving_synthesises_a_move_that_clears_hover() {
        // There is no further move event, so hover would otherwise stay lit.
        let mut t = InputTranslator::new();
        t.translate(&P::CursorMoved(Point::new(px(10.0), px(10.0))));
        let events = t.translate(&P::CursorLeft);
        assert_eq!(events.len(), 1);
        match &events[0] {
            UiEvent::MouseMove(e) => {
                assert!(e.position.x < px(-1.0e6), "the synthetic position must be unhittable");
            }
            other => panic!("expected MouseMove, got {other:?}"),
        }
        assert!(!t.pointer_inside());
    }

    #[test]
    fn modifiers_map_across_the_boundary() {
        let mut t = InputTranslator::new();
        t.translate(&P::MouseInput {
            button: PButton::Left,
            state: PState::Pressed,
            modifiers: PMods::SHIFT | PMods::ALT | PMods::SUPER,
        });
        let m = t.modifiers();
        assert!(m.shift && m.alt && m.meta);
        assert!(!m.control);
    }

    #[test]
    fn every_mouse_button_maps() {
        for (platform, ui) in [
            (PButton::Left, MouseButton::Primary),
            (PButton::Right, MouseButton::Secondary),
            (PButton::Middle, MouseButton::Middle),
            (PButton::Back, MouseButton::Back),
            (PButton::Forward, MouseButton::Forward),
            (PButton::Other(9), MouseButton::Other(9)),
        ] {
            assert_eq!(convert_button(platform), ui);
        }
    }

    #[test]
    fn scroll_deltas_keep_their_units() {
        // Collapsing lines into pixels here would be wrong: the conversion needs
        // a line height, which only the scroll container knows.
        assert_eq!(
            convert_scroll(PScroll::Lines { x: 0.0, y: -3.0 }),
            ScrollDelta::Lines(Size::new(0.0, -3.0))
        );
        assert_eq!(
            convert_scroll(PScroll::Pixels { x: px(1.0), y: px(2.0) }),
            ScrollDelta::Pixels(Size::new(px(1.0), px(2.0)))
        );
    }

    #[test]
    fn a_scroll_event_carries_the_current_pointer_position() {
        let mut t = InputTranslator::new();
        t.translate(&P::CursorMoved(Point::new(px(77.0), px(33.0))));
        let events = t.translate(&P::MouseWheel {
            delta: PScroll::Lines { x: 0.0, y: -1.0 },
            phase: TouchPhase::Moved,
        });
        match &events[0] {
            UiEvent::Scroll(e) => assert_eq!(e.position, Point::new(px(77.0), px(33.0))),
            other => panic!("expected Scroll, got {other:?}"),
        }
    }

    #[test]
    fn named_keys_map_to_their_ui_equivalents() {
        use spherekit_platform::{Key as P, NamedKey as N};
        for (named, expected) in [
            (N::Enter, Key::Enter),
            (N::Tab, Key::Tab),
            (N::Escape, Key::Escape),
            (N::ArrowLeft, Key::Left),
            (N::ArrowDown, Key::Down),
            (N::Home, Key::Home),
            (N::PageDown, Key::PageDown),
            (N::Backspace, Key::Backspace),
        ] {
            assert_eq!(convert_key(&P::Named(named)), expected);
        }
        assert_eq!(convert_key(&P::Named(N::F(5))), Key::Function(5));
    }

    #[test]
    fn character_keys_carry_their_text() {
        use spherekit_platform::{Key as P, KeyText};
        let key = convert_key(&P::Character(KeyText::new("é")));
        assert_eq!(key.text(), Some("é"));
    }

    #[test]
    fn events_the_ui_does_not_care_about_translate_to_nothing() {
        let mut t = InputTranslator::new();
        assert!(t.translate(&P::RedrawRequested).is_empty());
        assert!(t.translate(&P::CloseRequested).is_empty());
        assert!(
            t.translate(&P::Resized(Size::new(
                spherekit_core::DevicePx(800),
                spherekit_core::DevicePx(600)
            )))
            .is_empty()
        );
    }
}
