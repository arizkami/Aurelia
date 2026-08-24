# Touch

Touchscreens, trackpad gestures, and the on-screen keyboard.

The design goal is stated once and everything below follows from it: **an interface written for a
mouse must work under a finger without being rewritten**, and an interface that wants to know the
difference must be able to ask.

---

## The three layers

```text
winit                spherekit-platform        spherekit-ui                   widgets
Touch          →     WindowEvent::Touch   →    InputTranslator          →     MouseDown/Up/Move
                     (raw contacts)            (gesture recognition)          TouchStart/Move/End
                                                                              Pinch / Swipe / LongPress
                                               UiTree
                                               (pan, fling, cancellation)
```

Each layer decides exactly one thing:

| Layer | Decides | Does not decide |
|---|---|---|
| `spherekit-platform` | what the digitiser reported | what it meant |
| `InputTranslator` | tap vs drag, one finger vs two, how fast | what is under the finger |
| `UiTree` | which container scrolls, whether a widget owns the gesture | thresholds |

The split is not decoration. The thresholds are policy an application replaces
(`InputTranslator::set_touch_config`); the pan decision needs the hit chain and the pointer capture
state, which only the tree has. Putting either in the other layer means one of them has to guess.

---

## Pointer emulation

A single finger drives the mouse path as well as the touch path:

```text
finger down  →  TouchStart, MouseMove, MouseDown
finger moves →  TouchMove,  MouseMove
finger up    →  TouchEnd,   MouseUp
```

The move comes **before** the press, always. A `MouseDown` dispatched before the pointer position
had been updated would be hit-tested against wherever the mouse last was, which on a touchscreen is
wherever the last tap happened to be.

Every emulated event carries `source: PointerSource::Touch`. A widget that must behave differently
under a finger — no hover affordance, no tooltip, a larger hit slop — reads it. Everything else
ignores it and works unchanged, which is why `Button`, `TextField`, `Slider` and the rest needed no
touch handling of their own.

Two fingers is a gesture, not a press, so the moment a second one lands the emulated press is
**cancelled** rather than left running:

```text
second finger down  →  TouchStart, PointerCancel, Pinch(Began)
```

`PointerCancel` is W3C `pointercancel` by another name: the interaction was taken away and nothing
was committed. A widget that treated it as a `MouseUp` would fire on every pinch that happened to
begin on it.

Set `TouchConfig::emulate_pointer` to `false` for a surface that handles raw contacts itself and
would otherwise see each one twice.

---

## Drag to scroll

The tree owns this, for one reason: a slider dragged with a finger must beat the list it sits in,
and *pointer capture* is how the slider says so. The translator cannot see capture.

```text
TouchStart   →  remember the hit chain; stop any fling on it
TouchMove    →  under the slop?  nothing moves
                past the slop?   PointerCancel the press, then scroll 1:1
TouchEnd     →  throw a fling with the finger's final velocity
```

Three consequences worth stating:

**Nothing scrolls until the slop is crossed.** Ten logical pixels — the figure Android and iOS both
converged on. A container that moved on contact would drag the list a pixel under every tap.

**The scroll is direct, never eased.** A wheel notch has a destination and is glided to it over
about an eighth of a second; a finger has no destination at all. Easing it puts the content behind
the fingertip, which reads as a dropped frame rather than as smoothing. `ScrollPhase` is what
carries this distinction: `Wheel` glides, `Began`/`Changed` track.

**The press that became a scroll never fires.** This is the difference between a touch list that
works and one that activates a random row on every flick.

### Flings

Released with speed, the content coasts under exponential friction:

```text
v *= 0.998 ^ milliseconds        // per millisecond, not per frame
offset -= v * dt                 // content follows the finger
```

Per *millisecond* on purpose. Decaying once a frame makes a list travel further on a 60 Hz display
than on a 120 Hz one — the classic way the same code feels like two different products.

An axis that reaches its end drops its velocity immediately instead of coasting against the wall,
and touching a moving list catches it, exactly as a hand on a spinning record does.

`UiTree::advance` drives this and returns `true` while motion is owed, so a window that stops
drawing mid-fling parks the content halfway. `SphereKitSurface::render` already calls it and
reports the answer as `SurfaceStats::animating`.

Ask for the next frame from that stat, not from `needs_paint`: a fling is advanced *by* rendering,
so by the time a frame is on screen the dirty flag it set has already been consumed.

```rust
if surface.stats().animating || surface.needs_paint() {
    window.request_redraw();
}
```

---

## Gestures

| Event | Fired when | Carries |
|---|---|---|
| `Pinch` | two fingers change separation or angle | total and per-event scale and rotation, midpoint |
| `Swipe` | a fast flick ends | dominant direction, velocity, start and end |
| `LongPress` | a still finger passes the threshold | position, how long |

Scale and rotation come together in one event because the fingers produce them together; splitting
them would make a handler that wants both apply them a frame apart. Rotation is wrapped to `-π..=π`,
so fingers crossing the half-turn boundary do not report a full rotation in one tick.

A swipe resolves to one of four directions by the **dominant axis**, not the resultant angle: a
swipe is a decision between four choices and a diagonal has to land on one of them.

Long press is the only event that time alone produces — a finger held perfectly still generates no
further platform events — so it comes from a poll rather than a translation:

```rust
input.set_time(started.elapsed().as_millis() as u64);
for event in input.tick() {
    surface.dispatch(&event);
}
```

Call it once a frame. Without it, nothing else in the pipeline notices that half a second has gone
by. A finger that has moved past the slop never becomes a long press, so a scroll cannot also open a
context menu.

---

## Velocity

Estimated per finger with an exponential average weighted toward the newest sample, and clamped two
ways:

* Samples more than 100 ms apart contribute nothing. The finger was resting, and a small movement
  divided by a small time is a large velocity for a finger that was not moving.
* A fling is capped at 6000 px/s. A digitiser reporting two samples a millisecond apart can compute
  an enormous speed from a two-pixel movement, and without a ceiling one unlucky pair sends a list
  to its end.

Both are the difference between a fling that feels thrown and one that feels random.

---

## The on-screen keyboard

```rust
virtual_keyboard()
    .id("keys")
    .on_key(move |press| match press {
        KeyPress::Hide  => showing.set(false),
        KeyPress::Enter => commit(),
        other           => { other.apply(&mut editing.borrow_mut()); }
    })
```

**It never takes focus.** A keyboard that took keyboard focus would take it from the field it is
typing into, and the field would stop drawing its caret on the first key press. So the widget is
unfocusable and every press is reported to the application, which applies it to whatever it is
already editing. This is why a system on-screen keyboard is a separate, non-activating window.

**It owns no text.** `KeyPress::apply` edits a `TextEdit`; anything more interesting — a numeric
field with its own parsing, a shortcut that consumes Enter — is the application's to write.

**It paints its own keys.** Thirty-odd keys as thirty-odd child elements would be thirty-odd layout
nodes rebuilt every frame for a grid whose geometry is a division. The rows are data,
`keyboard::key_rects` produces the rects, and both painting and hit testing call it — so a key can
never be drawn somewhere other than where it can be pressed.

Three layers — letters, symbols, digits — with `numeric_keyboard()` for a field that only takes
numbers, because making the user find the layer key first is the difference between one press and
three. Shift is one-shot; pressed twice before anything is typed, it latches caps. A one-shot shift
is spent by the character it shifted and by nothing else, so backspacing does not lose the capital
that was asked for.

Shifted forms are stored as pairs rather than computed, because the shifted form of a key is not
always its uppercase: `,` shifts to `!`, and `char::to_uppercase` has no opinion about that.

---

## What is not done yet

* **Overscroll.** A list clamps at its end rather than rubber-banding.
* **Two-finger pan.** Pinch reports its midpoint, so an application can pan from it, but the tree
  does not scroll on two fingers.
* **Stylus.** `PointerSource::Pen` exists and force is carried through, but no backend reports it
  yet; a stylus arrives as a finger.
* **React bindings.** Touch events do not cross the JavaScript bridge.
