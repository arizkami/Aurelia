//! UI-level events and dispatch.
//!
//! Platform events arrive as `spherekit_platform::WindowEvent`. This module turns
//! them into events that carry *element-relative* information — which element
//! was hit, where the pointer is in that element's local space, whether a drag
//! is in progress — and dispatches them along the hit chain.
//!
//! ## Phases
//!
//! Dispatch runs root-to-target (**capture**), then at the **target**, then
//! target-to-root (**bubble**). Two things genuinely need capture: a modal
//! overlay that must swallow input before it reaches what is behind it, and a
//! drag that has captured the pointer. Everything else is a bubble handler, and
//! the phase is explicit at the registration site so it is never a guess.

use smallvec::SmallVec;
use spherekit_core::{ElementId, NodeId, Point, Px, Size};

/// Which mouse button.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Primary, usually left.
    Primary,
    /// Secondary, usually right.
    Secondary,
    /// Middle, usually the wheel click.
    Middle,
    /// Back, the fourth button.
    Back,
    /// Forward, the fifth button.
    Forward,
    /// Any other button, by index.
    Other(u16),
}

/// Pressed or released.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ElementState {
    /// The control went down.
    Pressed,
    /// The control came up.
    Released,
}

impl ElementState {
    /// True when pressed.
    #[inline]
    pub fn is_pressed(self) -> bool {
        matches!(self, ElementState::Pressed)
    }
}

/// Keyboard modifiers held during an event.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub control: bool,
    /// Alt, or Option on macOS.
    pub alt: bool,
    /// The platform meta key: Command on macOS, Windows key elsewhere.
    pub meta: bool,
}

impl Modifiers {
    /// No modifiers held.
    pub const NONE: Self = Self { shift: false, control: false, alt: false, meta: false };

    /// The platform's primary shortcut modifier.
    ///
    /// Command on macOS, Control everywhere else. Plug-in UIs get this wrong
    /// constantly, so it is a named method rather than an inline `cfg!`.
    #[inline]
    pub fn command(self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.meta
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.control
        }
    }

    /// True when no modifier is held.
    #[inline]
    pub fn is_none(self) -> bool {
        self == Self::NONE
    }
}

/// How far and in what units a scroll moved.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ScrollDelta {
    /// Discrete wheel notches.
    Lines(Size<f32>),
    /// Continuous movement in logical pixels, as a trackpad reports.
    Pixels(Size<Px>),
}

impl ScrollDelta {
    /// Converts to logical pixels using a per-line height.
    ///
    /// Line-based and pixel-based deltas cannot be unified upstream because a
    /// scroll container's line height depends on its content, so the conversion
    /// happens where that is known.
    #[inline]
    pub fn to_pixels(self, line_height: Px) -> Size<Px> {
        match self {
            ScrollDelta::Pixels(p) => p,
            ScrollDelta::Lines(l) => {
                Size::new(Px(l.width * line_height.get()), Px(l.height * line_height.get()))
            }
        }
    }
}

/// A key on the keyboard, after layout mapping.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A character-producing key, carrying the text it produces.
    Character(String),
    /// Enter or Return.
    Enter,
    /// Tab.
    Tab,
    /// Space.
    Space,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Escape.
    Escape,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
    /// A function key, `F1` being `Function(1)`.
    Function(u8),
    /// A key with no SphereKit-level meaning, identified by its platform code.
    Unidentified(u32),
}

impl Key {
    /// The text this key produces, if any.
    #[inline]
    pub fn text(&self) -> Option<&str> {
        match self {
            Key::Character(s) => Some(s),
            Key::Space => Some(" "),
            _ => None,
        }
    }

    /// True when the key moves the caret or selection rather than editing.
    #[inline]
    pub fn is_navigation(&self) -> bool {
        matches!(
            self,
            Key::Left
                | Key::Right
                | Key::Up
                | Key::Down
                | Key::Home
                | Key::End
                | Key::PageUp
                | Key::PageDown
        )
    }
}

/// What produced a pointer event.
///
/// A touchscreen drives the same `MouseDown`/`MouseMove`/`MouseUp` path a mouse
/// does — that is what makes every existing widget work under a finger without
/// being rewritten — so the *only* way to tell the two apart is this field.
/// Widgets that must differ (no hover state under a finger, a larger hit slop,
/// no tooltip on a long press that is already a gesture) read it; everything
/// else ignores it and behaves identically.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum PointerSource {
    /// A real mouse or trackpad.
    #[default]
    Mouse,
    /// A finger on a touchscreen, emulating a pointer.
    Touch,
    /// A stylus.
    Pen,
}

impl PointerSource {
    /// True when the event came from a finger or a stylus rather than a mouse.
    ///
    /// The question almost every caller actually has: whether a hover state is
    /// meaningful, and whether the input has a resting position at all.
    #[inline]
    pub fn is_direct(self) -> bool {
        matches!(self, PointerSource::Touch | PointerSource::Pen)
    }
}

/// A pointer moved.
#[derive(Clone, Debug, PartialEq)]
pub struct MouseMoveEvent {
    /// Position in window-logical coordinates.
    pub position: Point<Px>,
    /// Movement since the previous event.
    pub delta: Size<Px>,
    /// Buttons currently held.
    pub buttons: SmallVec<[MouseButton; 3]>,
    /// Modifiers held.
    pub modifiers: Modifiers,
    /// Whether a mouse, a finger or a stylus produced this.
    pub source: PointerSource,
}

impl MouseMoveEvent {
    /// True when any button is held, which is what distinguishes a drag from a
    /// hover.
    #[inline]
    pub fn is_dragging(&self) -> bool {
        !self.buttons.is_empty()
    }
}

/// A mouse button changed state.
#[derive(Clone, Debug, PartialEq)]
pub struct MouseButtonEvent {
    /// Position in window-logical coordinates.
    pub position: Point<Px>,
    /// Which button.
    pub button: MouseButton,
    /// Pressed or released.
    pub state: ElementState,
    /// How many clicks this is part of: 1 single, 2 double, 3 triple.
    pub click_count: u8,
    /// Modifiers held.
    pub modifiers: Modifiers,
    /// Whether a mouse, a finger or a stylus produced this.
    pub source: PointerSource,
}

/// Where a scroll gesture is in its life.
///
/// The distinction is not cosmetic: a wheel notch has a destination and is
/// eased toward it, while a finger has no destination at all and must track
/// one-to-one. Easing a finger makes the content lag behind it, which reads as
/// a dropped frame rather than as smoothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum ScrollPhase {
    /// A discrete wheel notch or a trackpad delta with no gesture around it.
    #[default]
    Wheel,
    /// A continuous gesture began. No movement is implied by this alone.
    Began,
    /// A continuous gesture moved.
    Changed,
    /// A continuous gesture ended. [`ScrollEvent::velocity`] carries whatever
    /// speed it ended at, which is what a fling is thrown with.
    Ended,
}

/// A scroll gesture.
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollEvent {
    /// Position in window-logical coordinates.
    pub position: Point<Px>,
    /// How far.
    pub delta: ScrollDelta,
    /// Modifiers held. `command()` plus scroll is conventionally zoom.
    pub modifiers: Modifiers,
    /// Where in a continuous gesture this is.
    pub phase: ScrollPhase,
    /// Speed in logical pixels per second, meaningful at [`ScrollPhase::Ended`].
    pub velocity: Size<Px>,
    /// Whether a mouse, a finger or a stylus produced this.
    pub source: PointerSource,
}

impl ScrollEvent {
    /// A plain wheel scroll at a position, with no gesture around it.
    ///
    /// The shape almost every caller and test wants; the gesture fields are
    /// only interesting to a touch pipeline.
    pub fn wheel(position: Point<Px>, delta: ScrollDelta, modifiers: Modifiers) -> Self {
        Self {
            position,
            delta,
            modifiers,
            phase: ScrollPhase::Wheel,
            velocity: Size::new(Px::ZERO, Px::ZERO),
            source: PointerSource::Mouse,
        }
    }

    /// True while a continuous gesture is still in flight.
    #[inline]
    pub fn momentum(&self) -> bool {
        matches!(self.phase, ScrollPhase::Began | ScrollPhase::Changed)
    }
}

/// One finger, as the UI layer sees it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TouchPoint {
    /// Which finger, stable for the life of the contact.
    pub id: spherekit_platform::TouchId,
    /// Where it is now, in window-logical coordinates.
    pub position: Point<Px>,
    /// Where it first went down.
    ///
    /// Kept per touch rather than recomputed, because every gesture threshold
    /// in the engine is measured from it and a widget that tracked it itself
    /// would have to survive its own rebuild to do so.
    pub start: Point<Px>,
    /// Movement since the previous event for this finger.
    pub delta: Size<Px>,
    /// Speed in logical pixels per second, smoothed.
    pub velocity: Size<Px>,
    /// Pressure in `0.0..=1.0`, when the digitiser reports it.
    pub force: Option<f32>,
}

impl TouchPoint {
    /// How far this finger has travelled from where it went down.
    #[inline]
    pub fn travel(&self) -> Px {
        self.position.distance_to(self.start)
    }
}

/// A touch contact changed.
///
/// Carries every finger currently down, not only the one that moved: a
/// two-finger gesture is decided by where *both* fingers are, and a handler
/// that only saw the moving one would have to accumulate the other itself.
#[derive(Clone, Debug, PartialEq)]
pub struct TouchEvent {
    /// The finger this event is about.
    pub touch: TouchPoint,
    /// Every finger currently down, including this one.
    pub touches: SmallVec<[TouchPoint; 4]>,
    /// Modifiers held. A touchscreen on a laptop still has a keyboard.
    pub modifiers: Modifiers,
}

/// Where a multi-touch gesture is in its life.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GestureState {
    /// Enough fingers arrived and the gesture is now recognised.
    Began,
    /// The gesture moved.
    Changed,
    /// A finger left and the gesture is over.
    Ended,
    /// The system took the gesture away. Roll back rather than commit.
    Cancelled,
}

/// A two-finger pinch, and the rotation that came with it.
///
/// Scale and rotation are reported together because the fingers produce them
/// together: separating them into two events would make a handler that wants
/// both apply them a frame apart.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PinchEvent {
    /// The midpoint between the fingers, in window-logical coordinates.
    pub position: Point<Px>,
    /// Total scale since the gesture began: `1.0` is unchanged.
    pub scale: f32,
    /// Scale change since the previous event.
    pub scale_delta: f32,
    /// Total rotation since the gesture began, in radians, clockwise-positive.
    pub rotation: f32,
    /// Rotation change since the previous event, in radians.
    pub rotation_delta: f32,
    /// Where in the gesture this is.
    pub state: GestureState,
}

/// Which way a swipe went.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SwipeDirection {
    /// Toward negative x.
    Left,
    /// Toward positive x.
    Right,
    /// Toward negative y.
    Up,
    /// Toward positive y.
    Down,
}

/// A fast directional flick, reported when the finger leaves.
///
/// Distinct from a scroll: a scroll is the content following a finger, a swipe
/// is a decision — dismiss this card, go to the next page. A handler that
/// wanted "the scroll ended fast" can read [`ScrollEvent::velocity`] instead.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SwipeEvent {
    /// Where the finger left, in window-logical coordinates.
    pub position: Point<Px>,
    /// Where the finger went down.
    pub start: Point<Px>,
    /// The dominant axis and sign of the movement.
    pub direction: SwipeDirection,
    /// Speed in logical pixels per second at the moment of release.
    pub velocity: Size<Px>,
}

/// A finger stayed still long enough to mean something.
///
/// The touch equivalent of a right-click, and the reason a context menu is
/// reachable at all without a second button.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LongPressEvent {
    /// Where the finger is, in window-logical coordinates.
    pub position: Point<Px>,
    /// How long it has been down, in milliseconds.
    pub duration_ms: u64,
    /// Whether a finger or a stylus produced this.
    pub source: PointerSource,
}

/// An in-progress pointer interaction was taken away.
///
/// Not a release: nothing was committed. A press that turns into a scroll
/// sends this so the button under the finger un-highlights instead of firing,
/// and a widget that treated it as a `MouseUp` would activate on every scroll
/// that happened to start on it. This is W3C `pointercancel` by another name.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PointerCancelEvent {
    /// Where the pointer was when the interaction was taken away.
    pub position: Point<Px>,
    /// Whether a mouse, a finger or a stylus produced this.
    pub source: PointerSource,
}

/// A key changed state.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyEvent {
    /// The key.
    pub key: Key,
    /// Pressed or released.
    pub state: ElementState,
    /// True when this is an auto-repeat rather than a fresh press.
    pub repeat: bool,
    /// Modifiers held.
    pub modifiers: Modifiers,
}

/// Text was committed, whether typed or from an input method.
#[derive(Clone, Debug, PartialEq)]
pub struct TextInputEvent {
    /// The committed text.
    pub text: String,
}

/// An input method's composition state changed.
#[derive(Clone, Debug, PartialEq)]
pub enum ImeEvent {
    /// Composition began.
    Start,
    /// The pre-edit string changed. The cursor range is a byte range into it.
    Preedit {
        /// The text being composed.
        text: String,
        /// Selection within the pre-edit, as a byte range.
        cursor: Option<(usize, usize)>,
    },
    /// The composition was committed.
    Commit {
        /// The committed text.
        text: String,
    },
    /// Composition was abandoned.
    End,
}

/// Anything the UI can respond to.
#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    /// The pointer moved.
    MouseMove(MouseMoveEvent),
    /// A button went down.
    MouseDown(MouseButtonEvent),
    /// A button came up.
    MouseUp(MouseButtonEvent),
    /// The pointer entered an element's bounds.
    MouseEnter(MouseMoveEvent),
    /// The pointer left an element's bounds.
    MouseLeave(MouseMoveEvent),
    /// A scroll gesture.
    Scroll(ScrollEvent),
    /// A finger went down.
    TouchStart(TouchEvent),
    /// A finger moved.
    TouchMove(TouchEvent),
    /// A finger lifted.
    TouchEnd(TouchEvent),
    /// The system took a contact away; roll back rather than commit.
    TouchCancel(TouchEvent),
    /// Two fingers changed their separation or their angle.
    Pinch(PinchEvent),
    /// A fast directional flick ended.
    Swipe(SwipeEvent),
    /// A contact stayed still long enough to be a long press.
    LongPress(LongPressEvent),
    /// An in-progress pointer interaction was taken away without committing.
    PointerCancel(PointerCancelEvent),
    /// A key changed state.
    Key(KeyEvent),
    /// Text was committed.
    TextInput(TextInputEvent),
    /// Input method state changed.
    Ime(ImeEvent),
    /// This element gained keyboard focus.
    FocusIn,
    /// This element lost keyboard focus.
    FocusOut,
}

impl UiEvent {
    /// The pointer position, for events that have one.
    #[inline]
    pub fn position(&self) -> Option<Point<Px>> {
        match self {
            UiEvent::MouseMove(e) | UiEvent::MouseEnter(e) | UiEvent::MouseLeave(e) => {
                Some(e.position)
            }
            UiEvent::MouseDown(e) | UiEvent::MouseUp(e) => Some(e.position),
            UiEvent::Scroll(e) => Some(e.position),
            UiEvent::TouchStart(e)
            | UiEvent::TouchMove(e)
            | UiEvent::TouchEnd(e)
            | UiEvent::TouchCancel(e) => Some(e.touch.position),
            UiEvent::Pinch(e) => Some(e.position),
            UiEvent::Swipe(e) => Some(e.position),
            UiEvent::LongPress(e) => Some(e.position),
            UiEvent::PointerCancel(e) => Some(e.position),
            _ => None,
        }
    }

    /// What produced this event, for the ones that come from a pointer.
    ///
    /// Touch events are always [`PointerSource::Touch`] and need no field of
    /// their own to say so.
    #[inline]
    pub fn source(&self) -> Option<PointerSource> {
        match self {
            UiEvent::MouseMove(e) | UiEvent::MouseEnter(e) | UiEvent::MouseLeave(e) => {
                Some(e.source)
            }
            UiEvent::MouseDown(e) | UiEvent::MouseUp(e) => Some(e.source),
            UiEvent::Scroll(e) => Some(e.source),
            UiEvent::LongPress(e) => Some(e.source),
            UiEvent::PointerCancel(e) => Some(e.source),
            UiEvent::TouchStart(_)
            | UiEvent::TouchMove(_)
            | UiEvent::TouchEnd(_)
            | UiEvent::TouchCancel(_)
            | UiEvent::Pinch(_)
            | UiEvent::Swipe(_) => Some(PointerSource::Touch),
            _ => None,
        }
    }

    /// True when this is a raw contact event rather than an emulated pointer.
    #[inline]
    pub fn is_touch(&self) -> bool {
        matches!(
            self,
            UiEvent::TouchStart(_)
                | UiEvent::TouchMove(_)
                | UiEvent::TouchEnd(_)
                | UiEvent::TouchCancel(_)
        )
    }

    /// True when the event is delivered by position rather than by focus.
    ///
    /// Pointer events go to whatever is under the cursor; keyboard and IME
    /// events go to the focused element regardless of where the mouse is.
    #[inline]
    pub fn is_positional(&self) -> bool {
        self.position().is_some()
    }

    /// True when the event should be routed to the focused element.
    #[inline]
    pub fn is_focus_routed(&self) -> bool {
        matches!(self, UiEvent::Key(_) | UiEvent::TextInput(_) | UiEvent::Ime(_))
    }
}

/// Which pass of dispatch a handler runs in.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Phase {
    /// Root to target, before the target sees the event.
    ///
    /// For modals and pointer capture: things that must intercept input before
    /// what is underneath them can react.
    Capture,
    /// Target to root, after the target has seen the event. The default, and
    /// what almost every handler wants.
    #[default]
    Bubble,
}

/// Whether dispatch continues after a handler runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum EventFlow {
    /// Keep dispatching to the next element in the chain.
    #[default]
    Continue,
    /// Stop. No further element sees this event.
    Stop,
}

impl EventFlow {
    /// True when dispatch should stop.
    #[inline]
    pub fn is_stopped(self) -> bool {
        matches!(self, EventFlow::Stop)
    }

    /// Combines two outcomes; any `Stop` wins.
    #[inline]
    pub fn merge(self, other: Self) -> Self {
        if self.is_stopped() || other.is_stopped() { EventFlow::Stop } else { EventFlow::Continue }
    }
}

/// One entry in a hit chain.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct HitTarget {
    /// The layout node.
    pub node: NodeId,
    /// The element's stable identity, if it declared one.
    pub element: Option<ElementId>,
    /// The element's absolute bounds.
    pub bounds: spherekit_core::Rect<Px>,
}

/// The chain of elements under a point, outermost first.
///
/// `SmallVec` because a UI hierarchy is rarely deeper than a dozen levels, and
/// hit testing runs on every mouse move — allocating there would be a per-frame
/// allocation in the most frequent code path in the engine.
pub type HitChain = SmallVec<[HitTarget; 12]>;

/// Tracks click counts so double and triple clicks can be recognised.
///
/// Kept as its own type because the rules — a time window *and* a distance
/// window, both required — are easy to get subtly wrong, and a double-click that
/// fires when the user moved the mouse 40 px between presses is worse than no
/// double-click at all.
#[derive(Debug, Clone)]
pub struct ClickTracker {
    last_position: Point<Px>,
    last_button: Option<MouseButton>,
    /// Milliseconds since engine start of the previous press.
    last_time_ms: u64,
    count: u8,
    /// Maximum gap between presses, in milliseconds.
    pub interval_ms: u64,
    /// Maximum movement between presses, in logical pixels.
    pub slop: Px,
}

impl Default for ClickTracker {
    fn default() -> Self {
        Self {
            last_position: Point::new(Px(f32::NEG_INFINITY), Px(f32::NEG_INFINITY)),
            last_button: None,
            last_time_ms: 0,
            count: 0,
            // Matches the common platform default; both Windows and macOS sit
            // near half a second.
            interval_ms: 500,
            slop: Px(4.0),
        }
    }
}

impl ClickTracker {
    /// Records a press and returns its click count, starting at 1.
    pub fn press(&mut self, button: MouseButton, position: Point<Px>, time_ms: u64) -> u8 {
        let in_time = time_ms.saturating_sub(self.last_time_ms) <= self.interval_ms;
        let in_place = position.distance_to(self.last_position) <= self.slop;
        let same_button = self.last_button == Some(button);

        self.count = if in_time && in_place && same_button && self.count > 0 {
            // Cap at triple. A quadruple click is not a distinct gesture in any
            // interface SphereKit targets, and wrapping a u8 would be worse.
            self.count.saturating_add(1).min(3)
        } else {
            1
        };
        self.last_button = Some(button);
        self.last_position = position;
        self.last_time_ms = time_ms;
        self.count
    }

    /// Forgets any in-progress multi-click, for example when focus is lost.
    pub fn reset(&mut self) {
        self.count = 0;
        self.last_button = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::px;

    #[test]
    fn command_maps_to_the_platform_shortcut_modifier() {
        let ctrl = Modifiers { control: true, ..Modifiers::NONE };
        let meta = Modifiers { meta: true, ..Modifiers::NONE };
        #[cfg(target_os = "macos")]
        {
            assert!(meta.command());
            assert!(!ctrl.command());
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(ctrl.command());
            assert!(!meta.command());
        }
    }

    #[test]
    fn scroll_delta_converts_lines_to_pixels() {
        let d = ScrollDelta::Lines(Size::new(0.0, -3.0));
        assert_eq!(d.to_pixels(px(20.0)), Size::new(px(0.0), px(-60.0)));
        // Pixel deltas pass through untouched.
        let p = ScrollDelta::Pixels(Size::new(px(1.0), px(2.0)));
        assert_eq!(p.to_pixels(px(20.0)), Size::new(px(1.0), px(2.0)));
    }

    #[test]
    fn events_are_routed_by_position_or_by_focus_but_never_both() {
        let move_ev = UiEvent::MouseMove(MouseMoveEvent {
            position: Point::new(px(1.0), px(1.0)),
            delta: Size::default(),
            buttons: SmallVec::new(),
            modifiers: Modifiers::NONE,
            source: PointerSource::Mouse,
        });
        let key_ev = UiEvent::Key(KeyEvent {
            key: Key::Enter,
            state: ElementState::Pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
        assert!(move_ev.is_positional() && !move_ev.is_focus_routed());
        assert!(key_ev.is_focus_routed() && !key_ev.is_positional());
    }

    #[test]
    fn event_flow_merge_is_sticky_on_stop() {
        assert_eq!(EventFlow::Continue.merge(EventFlow::Continue), EventFlow::Continue);
        assert_eq!(EventFlow::Continue.merge(EventFlow::Stop), EventFlow::Stop);
        assert_eq!(EventFlow::Stop.merge(EventFlow::Continue), EventFlow::Stop);
    }

    #[test]
    fn a_dragging_move_is_distinguished_from_a_hover() {
        let hover = MouseMoveEvent {
            position: Point::new(px(0.0), px(0.0)),
            delta: Size::default(),
            buttons: SmallVec::new(),
            modifiers: Modifiers::NONE,
            source: PointerSource::Mouse,
        };
        let mut drag = hover.clone();
        drag.buttons.push(MouseButton::Primary);
        assert!(!hover.is_dragging());
        assert!(drag.is_dragging());
    }

    #[test]
    fn double_click_requires_both_time_and_proximity() {
        let mut t = ClickTracker::default();
        let p = Point::new(px(10.0), px(10.0));
        assert_eq!(t.press(MouseButton::Primary, p, 0), 1);
        assert_eq!(t.press(MouseButton::Primary, p, 100), 2);
        assert_eq!(t.press(MouseButton::Primary, p, 200), 3);
    }

    #[test]
    fn a_slow_second_click_is_a_fresh_single_click() {
        let mut t = ClickTracker::default();
        let p = Point::new(px(10.0), px(10.0));
        assert_eq!(t.press(MouseButton::Primary, p, 0), 1);
        assert_eq!(t.press(MouseButton::Primary, p, 5_000), 1);
    }

    #[test]
    fn a_moved_second_click_is_a_fresh_single_click() {
        // This is the case that makes drag-select feel broken when it is wrong.
        let mut t = ClickTracker::default();
        assert_eq!(t.press(MouseButton::Primary, Point::new(px(10.0), px(10.0)), 0), 1);
        assert_eq!(t.press(MouseButton::Primary, Point::new(px(60.0), px(10.0)), 100), 1);
    }

    #[test]
    fn switching_buttons_restarts_the_count() {
        let mut t = ClickTracker::default();
        let p = Point::new(px(10.0), px(10.0));
        assert_eq!(t.press(MouseButton::Primary, p, 0), 1);
        assert_eq!(t.press(MouseButton::Secondary, p, 50), 1);
    }

    #[test]
    fn click_count_saturates_at_triple() {
        let mut t = ClickTracker::default();
        let p = Point::new(px(1.0), px(1.0));
        let mut last = 0;
        for i in 0..10 {
            last = t.press(MouseButton::Primary, p, i * 50);
        }
        assert_eq!(last, 3, "a u8 counter must not wrap on a held-down mouse");
    }

    #[test]
    fn reset_clears_an_in_progress_multi_click() {
        let mut t = ClickTracker::default();
        let p = Point::new(px(1.0), px(1.0));
        assert_eq!(t.press(MouseButton::Primary, p, 0), 1);
        t.reset();
        assert_eq!(t.press(MouseButton::Primary, p, 50), 1);
    }

    #[test]
    fn navigation_keys_are_classified() {
        assert!(Key::Left.is_navigation());
        assert!(Key::PageDown.is_navigation());
        assert!(!Key::Backspace.is_navigation());
        assert!(!Key::Character("a".into()).is_navigation());
    }

    #[test]
    fn key_text_extraction() {
        assert_eq!(Key::Character("é".into()).text(), Some("é"));
        assert_eq!(Key::Space.text(), Some(" "));
        assert_eq!(Key::Escape.text(), None);
    }
}
