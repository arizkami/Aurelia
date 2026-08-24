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
    ClickTracker, ElementState, GestureState, ImeEvent, Key, KeyEvent, LongPressEvent, Modifiers,
    MouseButton, MouseButtonEvent, MouseMoveEvent, PinchEvent, PointerCancelEvent, PointerSource,
    ScrollDelta, ScrollEvent, ScrollPhase, SwipeDirection, SwipeEvent, TextInputEvent, TouchEvent,
    TouchPoint, UiEvent,
};
use smallvec::SmallVec;
use spherekit_core::{Point, Px, Size};
use spherekit_platform::{TouchId, TouchPhase};

/// The thresholds that decide what a contact meant.
///
/// Policy, not physics, which is why it is a value the application can replace
/// rather than a set of constants: a drawing surface wants a tighter slop than
/// a list of rows, and a kiosk with gloved users wants a much looser one.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TouchConfig {
    /// How far a finger may travel and still be a tap rather than a drag.
    ///
    /// Ten logical pixels is the figure Android and iOS both converged on. Too
    /// tight and no button on a train is ever pressable; too loose and a slow
    /// scroll fires whatever it started on.
    pub tap_slop: Px,
    /// How long a still finger must stay down to be a long press.
    pub long_press_ms: u64,
    /// The speed, in logical pixels per second, above which a release is a
    /// swipe rather than the end of a drag.
    pub swipe_velocity: Px,
    /// Whether a single finger also drives the mouse path.
    ///
    /// On by default, and it is what makes every existing widget work under a
    /// finger without being touched. Turn it off for a surface that handles raw
    /// contacts itself and would otherwise see each one twice.
    pub emulate_pointer: bool,
}

impl Default for TouchConfig {
    fn default() -> Self {
        Self {
            tap_slop: Px(10.0),
            long_press_ms: 500,
            swipe_velocity: Px(320.0),
            emulate_pointer: true,
        }
    }
}

/// How much of the previous velocity estimate survives one sample.
///
/// A finger's per-event delta is noisy — the digitiser reports at its own rate,
/// not the frame's — and a fling thrown with an unsmoothed final sample lands
/// anywhere. Weighted toward the new sample so the estimate still turns with
/// the finger rather than trailing it.
const VELOCITY_SMOOTHING: f32 = 0.3;

/// The largest gap between two samples that still says anything about speed.
///
/// Past this the finger has effectively stopped, and dividing a small movement
/// by a small time would otherwise report a large velocity for a finger that
/// has been resting.
const VELOCITY_MAX_GAP_MS: u64 = 100;

/// One finger the translator is tracking.
#[derive(Copy, Clone, Debug)]
struct ActiveTouch {
    id: TouchId,
    start: Point<Px>,
    position: Point<Px>,
    delta: Size<Px>,
    velocity: Size<Px>,
    force: Option<f32>,
    start_ms: u64,
    last_ms: u64,
    /// Whether this finger has travelled past the tap slop.
    moved: bool,
    /// Whether a long press has already been reported for it.
    long_pressed: bool,
}

impl ActiveTouch {
    fn point(&self) -> TouchPoint {
        TouchPoint {
            id: self.id,
            position: self.position,
            start: self.start,
            delta: self.delta,
            velocity: self.velocity,
            force: self.force,
        }
    }
}

/// A two-finger gesture in progress.
#[derive(Copy, Clone, Debug)]
struct PinchState {
    /// The finger separation the gesture began at, never zero.
    start_distance: f32,
    /// The angle between the fingers when it began, in radians.
    start_angle: f32,
    last_scale: f32,
    last_rotation: f32,
}

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
    /// Fingers currently down, in the order they arrived.
    touches: SmallVec<[ActiveTouch; 4]>,
    pinch: Option<PinchState>,
    touch_config: TouchConfig,
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

    /// The thresholds gestures are recognised with.
    #[inline]
    pub fn touch_config(&self) -> TouchConfig {
        self.touch_config
    }

    /// Replaces the gesture thresholds.
    #[inline]
    pub fn set_touch_config(&mut self, config: TouchConfig) {
        self.touch_config = config;
    }

    /// How many fingers are currently down.
    #[inline]
    pub fn touch_count(&self) -> usize {
        self.touches.len()
    }

    /// Every finger currently down, in the order they arrived.
    pub fn touches(&self) -> SmallVec<[TouchPoint; 4]> {
        self.touches.iter().map(ActiveTouch::point).collect()
    }

    /// Emits anything that becomes true purely by time passing.
    ///
    /// Only a long press so far. Call it once a frame, after
    /// [`InputTranslator::set_time`]: a finger that is held perfectly still
    /// produces no further platform events, so nothing else would ever notice
    /// that half a second has gone by.
    pub fn tick(&mut self) -> SmallVec<[UiEvent; 2]> {
        let mut out: SmallVec<[UiEvent; 2]> = SmallVec::new();
        // A long press is a single-finger gesture: with a second finger down
        // the interaction is a pinch, and reporting a context menu underneath
        // it would open one every time a zoom is held.
        if self.touches.len() != 1 {
            return out;
        }
        let long_press_ms = self.touch_config.long_press_ms;
        let slop = self.touch_config.tap_slop;
        let now = self.now_ms;
        let touch = &mut self.touches[0];
        let held = now.saturating_sub(touch.start_ms);
        if !touch.long_pressed
            && !touch.moved
            && held >= long_press_ms
            && touch.position.distance_to(touch.start) <= slop
        {
            touch.long_pressed = true;
            out.push(UiEvent::LongPress(LongPressEvent {
                position: touch.position,
                duration_ms: held,
                source: PointerSource::Touch,
            }));
        }
        out
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
        // A finger held when the window lost focus produces no release either:
        // the gesture is now somebody else's, and a phantom contact would keep
        // every later one looking like a pinch.
        self.touches.clear();
        self.pinch = None;
    }

    /// Translates one platform event into zero or more UI events.
    ///
    /// Zero for events the UI layer does not care about — a resize, a redraw
    /// request — which the caller handles itself.
    pub fn translate(&mut self, event: &spherekit_platform::WindowEvent) -> SmallVec<[UiEvent; 2]> {
        use spherekit_platform::WindowEvent as P;
        let mut out: SmallVec<[UiEvent; 2]> = SmallVec::new();

        match event {
            P::Touch(contact) => self.translate_touch(contact, &mut out),
            P::PinchGesture { delta, phase } => {
                // The platform already recognised this one; there are no
                // underlying contacts to derive it from, so it is passed
                // through at the pointer's position with no rotation.
                let scale = 1.0 + delta;
                let state = match phase {
                    TouchPhase::Started => GestureState::Began,
                    TouchPhase::Moved => GestureState::Changed,
                    TouchPhase::Ended => GestureState::Ended,
                    TouchPhase::Cancelled => GestureState::Cancelled,
                };
                out.push(UiEvent::Pinch(PinchEvent {
                    position: self.position,
                    scale,
                    scale_delta: *delta,
                    rotation: 0.0,
                    rotation_delta: 0.0,
                    state,
                }));
            }
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
                            source: PointerSource::Mouse,
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
                            source: PointerSource::Mouse,
                        }));
                    }
                }
            }
            P::MouseWheel { delta, phase } => {
                // A discrete wheel reports `Moved` for every notch and never
                // starts or ends anything, so its phase says nothing about a
                // gesture. Only a trackpad's own `Started`/`Ended` does, and
                // that is the distinction a scroll container needs to decide
                // between easing to a destination and tracking a finger.
                let phase = match phase {
                    TouchPhase::Started => ScrollPhase::Began,
                    TouchPhase::Ended => ScrollPhase::Ended,
                    TouchPhase::Cancelled => ScrollPhase::Ended,
                    TouchPhase::Moved => ScrollPhase::Wheel,
                };
                out.push(UiEvent::Scroll(ScrollEvent {
                    position: self.position,
                    delta: convert_scroll(*delta),
                    modifiers: self.modifiers,
                    phase,
                    velocity: Size::new(Px::ZERO, Px::ZERO),
                    source: PointerSource::Mouse,
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
        self.move_event_from(PointerSource::Mouse)
    }

    fn move_event_from(&self, source: PointerSource) -> MouseMoveEvent {
        MouseMoveEvent {
            position: self.position,
            delta: Size::new(
                self.position.x - self.previous_position.x,
                self.position.y - self.previous_position.y,
            ),
            buttons: self.buttons.clone(),
            modifiers: self.modifiers,
            source,
        }
    }

    // ---------------------------------------------------------------- touch

    /// Translates one contact, and everything it implies.
    ///
    /// A single finger drives the mouse path as well as the touch path, so a
    /// button, a slider and a text field work under a finger with no per-widget
    /// touch handling. The moment a second finger arrives that emulation is
    /// cancelled rather than left running: two fingers are a gesture, and a
    /// pinch that also held a button down would activate whatever was beneath
    /// it on release.
    fn translate_touch(
        &mut self,
        contact: &spherekit_platform::TouchContact,
        out: &mut SmallVec<[UiEvent; 2]>,
    ) {
        let emulate = self.touch_config.emulate_pointer;
        match contact.phase {
            TouchPhase::Started => {
                // A duplicate id means the release was lost — a real thing on
                // Windows when a window loses capture mid-gesture. Replace
                // rather than track the same finger twice.
                self.touches.retain(|t| t.id != contact.id);
                self.touches.push(ActiveTouch {
                    id: contact.id,
                    start: contact.position,
                    position: contact.position,
                    delta: Size::new(Px::ZERO, Px::ZERO),
                    velocity: Size::new(Px::ZERO, Px::ZERO),
                    force: contact.force,
                    start_ms: self.now_ms,
                    last_ms: self.now_ms,
                    moved: false,
                    long_pressed: false,
                });
                self.push_touch(out, UiEvent::TouchStart, contact.id);

                match self.touches.len() {
                    1 if emulate => {
                        self.previous_position = contact.position;
                        self.position = contact.position;
                        self.inside = true;
                        // The move first: hover state and every handler's
                        // notion of where the pointer is come from it, and a
                        // press delivered before it would be dispatched against
                        // the *previous* position.
                        out.push(UiEvent::MouseMove(self.move_event_from(PointerSource::Touch)));
                        if !self.buttons.contains(&MouseButton::Primary) {
                            self.buttons.push(MouseButton::Primary);
                        }
                        let count =
                            self.clicks.press(MouseButton::Primary, self.position, self.now_ms);
                        out.push(UiEvent::MouseDown(MouseButtonEvent {
                            position: self.position,
                            button: MouseButton::Primary,
                            state: ElementState::Pressed,
                            click_count: count,
                            modifiers: self.modifiers,
                            source: PointerSource::Touch,
                        }));
                    }
                    2 => {
                        if emulate {
                            self.cancel_emulated_pointer(out);
                        }
                        self.begin_pinch(out);
                    }
                    _ => {}
                }
            }

            TouchPhase::Moved => {
                let Some(index) = self.touches.iter().position(|t| t.id == contact.id) else {
                    return;
                };
                let slop = self.touch_config.tap_slop;
                {
                    let now = self.now_ms;
                    let touch = &mut self.touches[index];
                    let delta = Size::new(
                        contact.position.x - touch.position.x,
                        contact.position.y - touch.position.y,
                    );
                    let dt_ms = now.saturating_sub(touch.last_ms);
                    if dt_ms > 0 && dt_ms <= VELOCITY_MAX_GAP_MS {
                        let dt = dt_ms as f32 / 1000.0;
                        let instant =
                            Size::new(Px(delta.width.get() / dt), Px(delta.height.get() / dt));
                        touch.velocity = Size::new(
                            Px(touch.velocity.width.get() * VELOCITY_SMOOTHING
                                + instant.width.get() * (1.0 - VELOCITY_SMOOTHING)),
                            Px(touch.velocity.height.get() * VELOCITY_SMOOTHING
                                + instant.height.get() * (1.0 - VELOCITY_SMOOTHING)),
                        );
                    } else if dt_ms > VELOCITY_MAX_GAP_MS {
                        // The finger has been resting; whatever it was doing
                        // before says nothing about where it is going now.
                        touch.velocity = Size::new(Px::ZERO, Px::ZERO);
                    }
                    touch.position = contact.position;
                    touch.delta = delta;
                    touch.force = contact.force;
                    touch.last_ms = now;
                    if !touch.moved && touch.position.distance_to(touch.start) > slop {
                        touch.moved = true;
                    }
                }
                self.push_touch(out, UiEvent::TouchMove, contact.id);

                if self.touches.len() >= 2 {
                    self.update_pinch(out);
                } else if emulate {
                    self.previous_position = self.position;
                    self.position = contact.position;
                    out.push(UiEvent::MouseMove(self.move_event_from(PointerSource::Touch)));
                }
            }

            TouchPhase::Ended | TouchPhase::Cancelled => {
                let Some(index) = self.touches.iter().position(|t| t.id == contact.id) else {
                    return;
                };
                let cancelled = contact.phase == TouchPhase::Cancelled;
                self.touches[index].position = contact.position;
                self.touches[index].delta = Size::new(Px::ZERO, Px::ZERO);
                let ending = self.touches[index];
                let was_last = self.touches.len() == 1;
                let had_pinch = self.pinch.is_some();

                self.push_touch(
                    out,
                    if cancelled { UiEvent::TouchCancel } else { UiEvent::TouchEnd },
                    contact.id,
                );

                if had_pinch {
                    // Two fingers minus one is not a pan: the remaining finger
                    // sits wherever the pinch left it, and treating its next
                    // move as a drag from there jumps the content.
                    self.end_pinch(out, cancelled);
                }

                self.touches.remove(index);

                if was_last && !had_pinch {
                    if !cancelled {
                        self.emit_swipe(out, &ending);
                    }
                    if emulate {
                        self.previous_position = self.position;
                        self.position = contact.position;
                        self.buttons.retain(|b| *b != MouseButton::Primary);
                        if cancelled {
                            out.push(UiEvent::PointerCancel(PointerCancelEvent {
                                position: self.position,
                                source: PointerSource::Touch,
                            }));
                        } else {
                            out.push(UiEvent::MouseUp(MouseButtonEvent {
                                position: self.position,
                                button: MouseButton::Primary,
                                state: ElementState::Released,
                                click_count: 1,
                                modifiers: self.modifiers,
                                source: PointerSource::Touch,
                            }));
                        }
                    }
                }
            }
        }
    }

    /// Pushes one touch event carrying the named contact plus every other one.
    fn push_touch(
        &self,
        out: &mut SmallVec<[UiEvent; 2]>,
        make: fn(TouchEvent) -> UiEvent,
        id: TouchId,
    ) {
        let Some(touch) = self.touches.iter().find(|t| t.id == id) else { return };
        out.push(make(TouchEvent {
            touch: touch.point(),
            touches: self.touches.iter().map(ActiveTouch::point).collect(),
            modifiers: self.modifiers,
        }));
    }

    /// Takes back an emulated press without committing it.
    fn cancel_emulated_pointer(&mut self, out: &mut SmallVec<[UiEvent; 2]>) {
        if !self.buttons.contains(&MouseButton::Primary) {
            return;
        }
        self.buttons.retain(|b| *b != MouseButton::Primary);
        // Not a release: nothing was clicked. A widget that treated this as one
        // would fire on every gesture that happened to begin on it.
        self.clicks.reset();
        out.push(UiEvent::PointerCancel(PointerCancelEvent {
            position: self.position,
            source: PointerSource::Touch,
        }));
    }

    fn begin_pinch(&mut self, out: &mut SmallVec<[UiEvent; 2]>) {
        let (a, b) = (self.touches[0].position, self.touches[1].position);
        let distance = a.distance_to(b).get();
        // Two fingers reported at the same point is not a pinch anyone can
        // scale from: every later distance would divide by zero.
        if distance < f32::EPSILON {
            return;
        }
        let angle = (b.y.get() - a.y.get()).atan2(b.x.get() - a.x.get());
        self.pinch = Some(PinchState {
            start_distance: distance,
            start_angle: angle,
            last_scale: 1.0,
            last_rotation: 0.0,
        });
        out.push(UiEvent::Pinch(PinchEvent {
            position: midpoint(a, b),
            scale: 1.0,
            scale_delta: 0.0,
            rotation: 0.0,
            rotation_delta: 0.0,
            state: GestureState::Began,
        }));
    }

    fn update_pinch(&mut self, out: &mut SmallVec<[UiEvent; 2]>) {
        let Some(state) = self.pinch.as_mut() else { return };
        let (a, b) = (self.touches[0].position, self.touches[1].position);
        let distance = a.distance_to(b).get();
        let angle = (b.y.get() - a.y.get()).atan2(b.x.get() - a.x.get());
        let scale = distance / state.start_distance;
        let rotation = normalize_angle(angle - state.start_angle);
        let event = PinchEvent {
            position: midpoint(a, b),
            scale,
            scale_delta: scale - state.last_scale,
            rotation,
            rotation_delta: normalize_angle(rotation - state.last_rotation),
            state: GestureState::Changed,
        };
        state.last_scale = scale;
        state.last_rotation = rotation;
        out.push(UiEvent::Pinch(event));
    }

    fn end_pinch(&mut self, out: &mut SmallVec<[UiEvent; 2]>, cancelled: bool) {
        let Some(state) = self.pinch.take() else { return };
        let position = if self.touches.len() >= 2 {
            midpoint(self.touches[0].position, self.touches[1].position)
        } else {
            self.position
        };
        out.push(UiEvent::Pinch(PinchEvent {
            position,
            scale: state.last_scale,
            scale_delta: 0.0,
            rotation: state.last_rotation,
            rotation_delta: 0.0,
            state: if cancelled { GestureState::Cancelled } else { GestureState::Ended },
        }));
    }

    /// Reports a flick, when the finger left fast enough and far enough.
    fn emit_swipe(&self, out: &mut SmallVec<[UiEvent; 2]>, touch: &ActiveTouch) {
        let (vx, vy) = (touch.velocity.width.get(), touch.velocity.height.get());
        let speed = (vx * vx + vy * vy).sqrt();
        if speed < self.touch_config.swipe_velocity.get() {
            return;
        }
        if touch.position.distance_to(touch.start) <= self.touch_config.tap_slop {
            return;
        }
        // The dominant axis, not the resultant angle: a swipe is a decision
        // between four choices, and a diagonal has to land on one of them.
        let direction = if vx.abs() >= vy.abs() {
            if vx < 0.0 { SwipeDirection::Left } else { SwipeDirection::Right }
        } else if vy < 0.0 {
            SwipeDirection::Up
        } else {
            SwipeDirection::Down
        };
        out.push(UiEvent::Swipe(SwipeEvent {
            position: touch.position,
            start: touch.start,
            direction,
            velocity: touch.velocity,
        }));
    }
}

/// The point halfway between two others.
fn midpoint(a: Point<Px>, b: Point<Px>) -> Point<Px> {
    Point::new(Px((a.x.get() + b.x.get()) * 0.5), Px((a.y.get() + b.y.get()) * 0.5))
}

/// Wraps an angle into `-pi..=pi`.
///
/// Two fingers crossing the half-turn boundary otherwise report a full rotation
/// in one event, which is how a pinch-to-rotate suddenly spins the content.
fn normalize_angle(mut radians: f32) -> f32 {
    use core::f32::consts::PI;
    while radians > PI {
        radians -= 2.0 * PI;
    }
    while radians < -PI {
        radians += 2.0 * PI;
    }
    radians
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

    // -------------------------------------------------------------- touch

    fn contact(id: u64, phase: TouchPhase, x: f32, y: f32) -> P {
        P::Touch(spherekit_platform::TouchContact {
            id: TouchId(id),
            phase,
            position: Point::new(px(x), px(y)),
            force: None,
        })
    }

    /// The kinds of event in a batch, for asserting order without matching on
    /// every field.
    fn kinds(events: &SmallVec<[UiEvent; 2]>) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                UiEvent::TouchStart(_) => "touch-start",
                UiEvent::TouchMove(_) => "touch-move",
                UiEvent::TouchEnd(_) => "touch-end",
                UiEvent::TouchCancel(_) => "touch-cancel",
                UiEvent::MouseMove(_) => "move",
                UiEvent::MouseDown(_) => "down",
                UiEvent::MouseUp(_) => "up",
                UiEvent::PointerCancel(_) => "cancel",
                UiEvent::Pinch(_) => "pinch",
                UiEvent::Swipe(_) => "swipe",
                UiEvent::LongPress(_) => "long-press",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn one_finger_down_drives_the_mouse_path_as_well() {
        // The property every existing widget depends on: a button under a
        // finger sees exactly what it would see under a mouse.
        let mut t = InputTranslator::new();
        let events = t.translate(&contact(1, TouchPhase::Started, 40.0, 60.0));
        assert_eq!(kinds(&events), ["touch-start", "move", "down"]);
        // The move must come first, or the press is dispatched against
        // wherever the pointer used to be.
        match (&events[1], &events[2]) {
            (UiEvent::MouseMove(m), UiEvent::MouseDown(d)) => {
                assert_eq!(m.position, Point::new(px(40.0), px(60.0)));
                assert_eq!(d.position, Point::new(px(40.0), px(60.0)));
                assert_eq!(d.source, PointerSource::Touch);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(t.touch_count(), 1);
    }

    #[test]
    fn lifting_a_finger_releases_the_emulated_button() {
        let mut t = InputTranslator::new();
        t.translate(&contact(1, TouchPhase::Started, 10.0, 10.0));
        let events = t.translate(&contact(1, TouchPhase::Ended, 10.0, 10.0));
        assert_eq!(kinds(&events), ["touch-end", "up"]);
        assert!(t.buttons().is_empty(), "a lifted finger must not leave a button held");
        assert_eq!(t.touch_count(), 0);
    }

    #[test]
    fn a_cancelled_contact_cancels_rather_than_clicks() {
        // The distinction that decides whether a button fires: a cancel is not
        // a release, and treating it as one activates whatever was underneath.
        let mut t = InputTranslator::new();
        t.translate(&contact(1, TouchPhase::Started, 10.0, 10.0));
        let events = t.translate(&contact(1, TouchPhase::Cancelled, 10.0, 10.0));
        assert_eq!(kinds(&events), ["touch-cancel", "cancel"]);
    }

    #[test]
    fn a_second_finger_takes_the_press_back_and_starts_a_pinch() {
        let mut t = InputTranslator::new();
        t.translate(&contact(1, TouchPhase::Started, 100.0, 100.0));
        let events = t.translate(&contact(2, TouchPhase::Started, 200.0, 100.0));
        assert_eq!(kinds(&events), ["touch-start", "cancel", "pinch"]);
        assert!(t.buttons().is_empty(), "a two-finger gesture must hold no button");
    }

    #[test]
    fn separating_two_fingers_reports_the_scale_they_separated_by() {
        let mut t = InputTranslator::new();
        t.translate(&contact(1, TouchPhase::Started, 100.0, 100.0));
        t.translate(&contact(2, TouchPhase::Started, 200.0, 100.0));
        // Twice as far apart is exactly a scale of two, whatever the fingers
        // did on the way there.
        let events = t.translate(&contact(2, TouchPhase::Moved, 300.0, 100.0));
        let pinch = events
            .iter()
            .find_map(|e| match e {
                UiEvent::Pinch(p) => Some(*p),
                _ => None,
            })
            .expect("a two-finger move is a pinch");
        assert_eq!(pinch.state, GestureState::Changed);
        assert!((pinch.scale - 2.0).abs() < 1e-3, "scale was {}", pinch.scale);
        assert_eq!(pinch.position, Point::new(px(200.0), px(100.0)));
    }

    #[test]
    fn a_pinch_does_not_become_a_swipe_when_a_finger_leaves() {
        // Otherwise letting go of a zoom throws the page sideways.
        let mut t = InputTranslator::new();
        t.set_time(0);
        t.translate(&contact(1, TouchPhase::Started, 100.0, 100.0));
        t.translate(&contact(2, TouchPhase::Started, 200.0, 100.0));
        t.set_time(16);
        t.translate(&contact(2, TouchPhase::Moved, 300.0, 100.0));
        let events = t.translate(&contact(2, TouchPhase::Ended, 300.0, 100.0));
        assert!(!kinds(&events).contains(&"swipe"));
        assert!(kinds(&events).contains(&"pinch"));
    }

    #[test]
    fn a_fast_flick_is_a_swipe_in_the_dominant_direction() {
        let mut t = InputTranslator::new();
        t.set_time(0);
        t.translate(&contact(1, TouchPhase::Started, 300.0, 100.0));
        // Sixty logical pixels left in 16 ms is well past the threshold, with a
        // small vertical wobble that must not change the reported direction.
        t.set_time(16);
        t.translate(&contact(1, TouchPhase::Moved, 240.0, 104.0));
        t.set_time(32);
        t.translate(&contact(1, TouchPhase::Moved, 180.0, 106.0));
        let events = t.translate(&contact(1, TouchPhase::Ended, 180.0, 106.0));
        let swipe = events
            .iter()
            .find_map(|e| match e {
                UiEvent::Swipe(s) => Some(*s),
                _ => None,
            })
            .expect("a fast flick is a swipe");
        assert_eq!(swipe.direction, SwipeDirection::Left);
        assert!(swipe.velocity.width < Px::ZERO);
    }

    #[test]
    fn a_slow_drag_is_not_a_swipe() {
        let mut t = InputTranslator::new();
        t.set_time(0);
        t.translate(&contact(1, TouchPhase::Started, 300.0, 100.0));
        // The same distance over ten times as long.
        for step in 1..=10 {
            t.set_time(step * 50);
            t.translate(&contact(1, TouchPhase::Moved, 300.0 - step as f32 * 12.0, 100.0));
        }
        let events = t.translate(&contact(1, TouchPhase::Ended, 180.0, 100.0));
        assert!(!kinds(&events).contains(&"swipe"), "{:?}", kinds(&events));
    }

    #[test]
    fn a_still_finger_becomes_a_long_press_once() {
        let mut t = InputTranslator::new();
        t.set_time(0);
        t.translate(&contact(1, TouchPhase::Started, 20.0, 20.0));
        t.set_time(400);
        assert!(t.tick().is_empty(), "too early to be a long press");
        t.set_time(600);
        let events = t.tick();
        assert_eq!(kinds(&events), ["long-press"]);
        // And exactly once: a handler that opened a menu would otherwise open
        // one every frame the finger stayed down.
        t.set_time(900);
        assert!(t.tick().is_empty());
    }

    #[test]
    fn a_finger_that_moved_never_becomes_a_long_press() {
        let mut t = InputTranslator::new();
        t.set_time(0);
        t.translate(&contact(1, TouchPhase::Started, 20.0, 20.0));
        t.set_time(100);
        t.translate(&contact(1, TouchPhase::Moved, 20.0, 90.0));
        t.set_time(900);
        assert!(t.tick().is_empty(), "a scroll must not also open a context menu");
    }

    #[test]
    fn losing_focus_forgets_every_finger() {
        // The release for a finger held during an alt-tab goes elsewhere, and a
        // phantom contact would make the next single tap look like a pinch.
        let mut t = InputTranslator::new();
        t.translate(&contact(1, TouchPhase::Started, 10.0, 10.0));
        t.translate(&P::Focused(false));
        assert_eq!(t.touch_count(), 0);
    }

    #[test]
    fn a_repeated_start_for_the_same_finger_replaces_it() {
        // Windows drops a release when a window loses capture mid-gesture;
        // tracking the same finger twice would leave every later tap looking
        // like a two-finger gesture.
        let mut t = InputTranslator::new();
        t.translate(&contact(1, TouchPhase::Started, 10.0, 10.0));
        t.translate(&contact(1, TouchPhase::Started, 30.0, 30.0));
        assert_eq!(t.touch_count(), 1);
        assert_eq!(t.touches()[0].start, Point::new(px(30.0), px(30.0)));
    }

    #[test]
    fn emulation_can_be_turned_off() {
        let mut t = InputTranslator::new();
        t.set_touch_config(TouchConfig { emulate_pointer: false, ..TouchConfig::default() });
        let events = t.translate(&contact(1, TouchPhase::Started, 10.0, 10.0));
        assert_eq!(kinds(&events), ["touch-start"]);
    }

    #[test]
    fn a_touch_event_carries_every_finger_that_is_down() {
        let mut t = InputTranslator::new();
        t.translate(&contact(1, TouchPhase::Started, 10.0, 10.0));
        let events = t.translate(&contact(2, TouchPhase::Started, 90.0, 10.0));
        match &events[0] {
            UiEvent::TouchStart(e) => {
                assert_eq!(e.touches.len(), 2);
                assert_eq!(e.touch.id, TouchId(2));
            }
            other => panic!("expected TouchStart, got {other:?}"),
        }
    }

    #[test]
    fn a_trackpad_gesture_keeps_its_phase_and_a_wheel_notch_does_not() {
        let mut t = InputTranslator::new();
        let notch = t.translate(&P::MouseWheel {
            delta: PScroll::Lines { x: 0.0, y: -1.0 },
            phase: TouchPhase::Moved,
        });
        match &notch[0] {
            UiEvent::Scroll(e) => assert_eq!(e.phase, ScrollPhase::Wheel),
            other => panic!("expected Scroll, got {other:?}"),
        }
        let began = t.translate(&P::MouseWheel {
            delta: PScroll::Pixels { x: px(0.0), y: px(-4.0) },
            phase: TouchPhase::Started,
        });
        match &began[0] {
            UiEvent::Scroll(e) => {
                assert_eq!(e.phase, ScrollPhase::Began);
                assert!(e.momentum());
            }
            other => panic!("expected Scroll, got {other:?}"),
        }
    }
}
