# Platform

Windows, input, HiDPI, and the parts of "not owning the process" that matter.

## The seam

`spherekit-platform` has two jobs: describe the hardware faithfully, and keep `winit` from leaking.

**No `winit` type appears in the public API.** SphereKit has its own `WindowEvent`, `Key`, `NamedKey`,
`MouseButton`, `Modifiers`, `ScrollDelta`, `Cursor` and `MonitorInfo`. The winit backend lives in
`backend/winit.rs` behind a feature, so a native Win32, AppKit or Wayland backend can replace it
without touching anything above.

`spherekit-ui` then has its *own* input types again, and translates at `spherekit_ui::InputTranslator`.
That is a deliberate second boundary rather than duplication:

| Layer | Answers |
|---|---|
| `spherekit-platform` | What did the window system say? |
| `spherekit-ui` | What interaction is this? |

A platform `MouseInput` carries no coordinates, because the window system does not send any. A UI
`MouseDown` must carry them, because every handler needs them. Something has to remember the last
cursor position, the held buttons, the modifier set and the click count; `InputTranslator` is that
something, one per window.

## Logical and physical pixels

Every position `spherekit-platform` reports is in **logical** pixels (`Point<Px>`); every extent is in
**physical** pixels (`Size<DevicePx>`). That asymmetry is not an accident:

- A pointer position is compared against layout, which is logical.
- A surface extent is a GPU allocation, which is physical and integral.

`ScaleFactor` converts, and there is deliberately no `From<Px> for DevicePx`.

### Where rounding happens, and where it must not

| Conversion | Rounds? | Why |
|---|---|---|
| `to_device` | Yes | Surface extents and scissor rects are whole device pixels |
| `to_device_f32` | **No** | Vertex positions; rounding them destabilises geometry at 125 % |
| `Rect::round_out` | Outward | Invalidation and scissors must never under-cover |

Fractional scaling is first-class. `ScaleFactor::new` clamps to a sane positive range because
platforms have been observed to report `0.0` transiently while a window moves between monitors, and
a zero scale makes every downstream conversion degenerate.

A `ScaleFactorChanged` normally arrives with a `Resized` in the same breath; the surface must be
reconfigured before the next frame, and `SphereKitSurface::resize` handles a zero extent by doing
nothing — a minimised window is not an error.

## The event loop

```rust
pub trait AppHandler {
    fn resumed(&mut self, cx: &mut AppContext<'_>);
    fn window_event(&mut self, cx: &mut AppContext<'_>, window: WindowId, event: WindowEvent);
    fn about_to_wait(&mut self, cx: &mut AppContext<'_>) {}
    fn suspended(&mut self, cx: &mut AppContext<'_>) {}
    fn exiting(&mut self, cx: &mut AppContext<'_>) {}
    fn memory_warning(&mut self, cx: &mut AppContext<'_>) {}
}
```

`resumed` rather than a constructor, because every platform requires window creation to happen on
the loop's thread while the loop is running, and on mobile a window created before `resumed` is
destroyed immediately. `suspended` must release every GPU surface: on Android the underlying window
is gone the moment it returns.

`AppContext` is borrowed for one callback because creating a window, enumerating displays and
exiting all need the running loop, which only exists there.

`close_window` returns the window rather than dropping it, because dropping an `Arc<Window>` while
a GPU surface still references it is undefined behaviour in the driver. The caller drops the
surface first.

## Start-up, and the blank window

A window is mapped by the OS the moment it is created. Everything that happens next — choosing an
adapter, creating a device, configuring a swapchain, scanning the system fonts, building the first
element tree — happens with an empty rectangle already on screen, and the user sees a white flash
for the whole of it.

Measured on one machine, an NVIDIA GTX 1060 on Windows 11:

```text
init: gpu 706 ms, fonts 22 ms
first frame: 46 ms
```

Three quarters of a second of blank window, and **the GPU is the cost, not the fonts** — which is
the opposite of what the `load_system_fonts` doc comment would lead you to guess. Restricting
`WGPU_BACKEND` to a single backend saves only 50-70 ms of it; the rest is the driver creating a
device and is not something this engine can shorten.

So the fix is not to make start-up fast. It is to not show anything until there is something to
show:

```rust
// 1. Create hidden.
let attrs = WindowAttributes::new("...").with_visible(false);
let window = cx.create_window(&attrs)?;

// 2. Initialise. Nothing is on screen while this runs.
let surface = SphereKitSurface::new(...).await?;

// 3. Draw one frame into the hidden window.
self.draw();

// 4. Reveal, now that the swapchain holds a painted frame.
window.set_visible(true);
window.request_redraw();
```

[`SphereKitSurface::has_presented`] is the condition to test at step 4, and it is deliberately not the
same as "`render` returned". `render` reports `Ok(None)` for a zero-area viewport and for a surface
that is transiently unavailable, and neither of those has drawn anything — revealing on either would
show exactly the blank window the sequence exists to avoid.

Reveal anyway if it is false. A surface that is not ready at start-up recovers on the next redraw;
an application whose window never appears does not, and that is the worse failure. Both examples do
this and print a warning when it happens.

The `request_redraw` at the end is insurance rather than necessity: mapping a window invalidates it,
and on some compositors the present at step 3 went to a surface that was not mapped yet.

## Frame scheduling

```rust
pub enum RedrawPolicy {
    Idle,       // loop blocks; a static window costs zero CPU
    Dirty,      // one frame is owed
    Animating,  // paced against the display refresh
    Realtime,   // continuous, unpaced
}
```

`FrameScheduler` maps those onto `ControlFlow::Wait`, `Poll` and `WaitUntil`, and knows the
monitor's refresh rate so `Animating` paces rather than spins. Refresh rate matters: the engine
targets 60, 120, 144 and 240 Hz, and pacing to a hardcoded 60 wastes two thirds of a 240 Hz
display.

`Realtime` is the expensive one and exists for the case that justifies it — a meter that must show
every audio buffer. It should be entered while the transport rolls and left when it stops. The rest
of the UI stays cached throughout.

## Custom window chrome

`WindowChrome` replaces a plain `decorations: bool`, which could not express the middle case — and
the middle case is the one worth having.

| | Title bar | Resize borders | Snap | Window menu | Drop shadow |
|---|---|---|---|---|---|
| `System` | platform | platform | yes | yes | yes |
| `Custom` | **application** | platform | yes | yes | yes |
| `None` | none | **none** | no | no | no |

`None` is what most "borderless window" code actually builds, and it is strictly worse than `Custom`
for anything with a frame: measured on Windows, a fully stripped frame reports `HTCLIENT` at every
edge and corner, so nothing resizes it but an explicit `Window::begin_resize`. It exists for splash
screens and HUDs.

### How `Custom` keeps everything but the caption

The obvious implementation answers `WM_NCCALCSIZE` with the whole window rectangle. It works, and it
silently throws away four things that are expensive to rebuild and free to keep.

So the subclass lets `DefWindowProcW` compute the normal frame first and then restores **only the top
edge**. `DefWindowProcW` goes on answering `HTLEFT`, `HTRIGHT`, `HTBOTTOM` and both bottom corners by
itself; `backend::nc::hit_test` only has to handle the top edge, the two top corners and the caption
band. When maximised the top is pushed in by the frame thickness instead, which lands the client at
exactly the work area with no `MonitorFromWindow` call.

Every decision is a pure function in `backend/nc.rs` with no `user32` linkage, tested without a
window. Every unsafe call is in `backend/ffi.rs`, which opens with the nine distinct safety
invariants it upholds — counted, rather than rediscovered per call site.

### The caption is geometry, not an event

```rust
window.set_caption_regions(&CaptionRegions::strip(width, px(32.0))
    .with_button(button_rect));
```

Pushed down rather than answered on demand, because `WM_NCHITTEST` is answered *synchronously from
inside the platform's own dispatch*: there is no point at which the interface thread could be called
back. The window procedure only ever `try_lock`s the published geometry — losing that race costs one
frame of drag, which beats blocking a compositor inside its own dispatch.

**Every interactive thing inside the caption must be listed in `exclude`.** A press the platform
routes as caption is swallowed by the modal move loop, so an unlisted button receives no click
**ever** — not merely a delayed one. That is the single most likely way to get this wrong.

### Transparent windows and system backdrops

`WindowAttributes::with_transparent(true)` makes the client surface respect alpha. On Windows 11,
`Window::set_backdrop(WindowBackdrop::Mica)` or `WindowBackdrop::Acrylic` then asks DWM to blur the
desktop behind that transparent surface. `WindowBackdrop::None` removes the material. The call
returns `Unsupported` on platforms without a system compositor API, where renderer-side
`Styled::backdrop_blur` remains the portable fallback.

### The system menu

Right-click on a published caption region opens the real window menu, and Alt+Space still works.
Both paths run `nc::menu_states` first. Measured: while a window was maximised, `GetMenuState` still
reported Move and Size as enabled — Windows fixes its own menu up only when *it* opens the menu, so
an uncorrected one offers actions that silently do nothing.

`TrackPopupMenu` runs a nested message loop, so no lock is held across it, and the chosen command is
*posted* rather than sent: the command may destroy the window, and returning through a nested loop
into a dead window is how that becomes a crash.

### A known cost

`request_inner_size` is reduced by the reclaimed caption before it reaches winit. The platform still
believes there is a title bar and sizes the outer window for one, while the subclass has already
given that strip to the client, so an uncompensated request comes out about thirty logical pixels
too tall at 96 dpi and more above it.

## Why there is no native Win32 backend yet

`backend/mod.rs` has always said a native backend "would live beside it and provide the same `Window`
surface". It is not built, and the reasons are worth stating rather than leaving as an absence:

1. **It delivers nothing the chrome work needed.** Borderless and the system menu land on the winit
   backend, through a subclass that depends on neither event loop. Borderless does not need a native
   backend; a native backend would need borderless.
2. **Measured scope.** winit's `platform_impl/windows` is 9,219 lines, of which keyboard and layout
   handling are 2,229 and the IME implementation is roughly 600. The hard parts are dead keys,
   layout switching and `ToUnicodeEx` kernel-state handling — none of which is chrome.
3. **A backend without IME is not a fallback.** SphereKit has a working input method path
   (see above). Shipping a second backend that reports `Unsupported` for it would make that backend
   unusable for exactly the languages an input method exists for.
4. **The stated justification is not yet demonstrated.** "A plug-in host owns the pump" is the
   argument for a native loop, and winit ships `EventLoopExtPumpEvents::pump_app_events`, which works
   on Windows. Build the native loop when a real host refuses it.

If it is built, the shape is known: `ControlFlow` becomes a wait strategy — `Wait` maps to
`GetMessageW`, `Poll` skips the wait, and `WaitUntil` maps to `MsgWaitForMultipleObjectsEx` with the
remainder **rounded up**, because truncating a sub-millisecond wait to zero silently converts
`WaitUntil` into `Poll` and burns a core.

## Plug-in embedding

The engine must tolerate not owning the process. A VST3 or CLAP editor owns neither the main thread
nor the message pump, and its window is a child of one the host created.

`WindowTarget` is either an owned window or a foreign `RawWindowHandle`:

```rust
pub enum WindowTarget {
    Owned(Arc<Window>),
    Foreign(ForeignWindow),
}
```

Both implement `HasWindowHandle` and `HasDisplayHandle`, so `WgpuRenderer::new` and
`SphereKitSurface::new` accept either. `WindowAttributes::with_parent` covers the case where the
engine creates a child window inside a host-supplied parent.

`App::run` is documented as main-thread-only and **not for plug-ins** — a plug-in must not call it
at all. The host drives the loop; the plug-in renders on demand.

Host lifecycle concerns worth stating plainly:

- The host may resize the editor at any time, including to zero while it is hidden.
- The host may destroy the parent window without warning. Every surface must be releasable
  synchronously.
- Several instances of the same plug-in share a process. Nothing in the engine may be a process
  global, which is why there is no global renderer, no global font database and no global cache.
- The host owns window focus. SphereKit's focus registry is deliberately independent of it; see
  `spherekit_ui::FocusRegistry`.

## Multiple windows

`WindowRegistry` holds every open window with its own size, scale factor, focus state and redraw
policy. A main window, a floating mixer, a plug-in editor and a modal each get their own
`SphereKitSurface`; the GPU device is shared where the backend allows it.

## Keyboard

`Key` separates the layout-dependent value from the layout-independent position:

```rust
WindowEvent::KeyboardInput {
    key: Key,                  // what it means: 'a', Enter, F5
    physical_key: PhysicalKey, // where it is: KeyA — for rebindable controls
    state, repeat, modifiers,
}
```

Text insertion does **not** come from here. A key that produces text also produces a separate
`TextInput`, because the mapping from key to text depends on layout, dead keys and the IME.
Deriving text from a key event works for ASCII and breaks for everything else.

`Modifiers::command()` maps to Super on macOS and Control elsewhere. It is a named method rather
than an inline `cfg!` because plug-in UIs get this wrong constantly.

Losing focus clears all held modifier and button state: the key-up for a modifier held during an
alt-tab is delivered to whoever has focus next, so keeping it would leave a phantom Shift held
forever. There is a test for that.

## IME

Input-method composition works end to end, and the path is worth spelling out because every layer
has to cooperate and a break anywhere is silent.

```text
winit Ime            spherekit-platform ImeEvent
   -> InputTranslator   spherekit-ui UiEvent::Ime
   -> UiTree::dispatch_with_text
   -> TextField         -> TextEdit::set_preedit / commit / cancel_composition
   -> paint             -> PaintContext::request_ime(caret)
   -> UiTree::ime()     -> the application
   -> Window::set_ime_allowed + set_ime_cursor_area
```

### The provisional text lives in the buffer

An input method composes over several keystrokes before committing. The user has to *see* what they
are composing, in place, with the text around it reflowing — so the provisional string is spliced
into the buffer like any other text and [`spherekit_ui::TextEdit::preedit`] records the byte range it
occupies.

Two consequences fall out of that, and both are easy to get wrong:

* [`spherekit_ui::TextEdit::text`] is **not** the value while composing. Anything outside the editor —
  validation, a search-as-you-type query, a bound model field — must read
  [`spherekit_ui::TextEdit::committed_text`], or it sees half-composed syllables.
* Ordinary edits have to be refused while a composition is open, or they splice into a range the
  input method believes it owns and the next pre-edit replaces the wrong bytes. `TextEdit` refuses
  them itself; `is_composing` is the check.

`ImeEvent::End` **discards** rather than commits. An input method that wanted the text kept sends a
commit first, and treating `End` as a commit inserts half-composed text every time the user presses
Escape. There is a test for exactly that.

### Permission and placement

The application owes the window two things, and omitting either is invisible until someone tries to
type Japanese:

1. `set_ime_allowed`. Nothing composes at all until the window is told something editable has focus.
2. `set_ime_cursor_area`. The candidate list opens *here*. Without it, it opens in a corner of the
   screen, which makes the feature useless for the languages that need it.

Both come from one signal. A focused field calls `PaintContext::request_ime` with its caret, the
tree collects it, and the application reads `SphereKitSurface::ime()` after rendering:

```rust
let area = surface.ime();
let allowed = area.is_some();
if allowed != self.ime_allowed {
    window.set_ime_allowed(allowed);
    self.ime_allowed = allowed;
}
if let Some(area) = area && self.ime_caret != Some(area.caret) {
    window.set_ime_cursor_area(area.caret);
    self.ime_caret = Some(area.caret);
}
```

**Read after rendering, not before.** Whether an element edits text and where its caret is are both
paint-time facts: a field only knows where its caret is once it has laid its string out, and it lays
it out while painting. A trait method answered before layout would have no caret to report.

**Diff before pushing.** A platform is entitled to treat re-enabling an input method as a reason to
cancel the composition in progress, so pushing the same state every frame would make composition
impossible. Both examples diff.

The request is rebuilt from scratch on every paint rather than remembered. A field that was
destroyed, scrolled out of view or blurred since the last frame has to stop asking, and the only
reliable signal for that is that it did not ask again.

### Hit testing needs the text system

Turning a click into a caret index means shaping the string, so `UiTree::dispatch_with_text` exists
alongside `UiTree::dispatch` — the same pairing as `compute_layout_with_text`, for the same reason.
`SphereKitSurface::dispatch` uses the text-aware form. Shaping there is a cache hit: paint shaped the
same string with the same style moments earlier.

A field lays out with [`spherekit_ui::TextField`]'s own `text_style`, used by paint *and* by hit
testing. Measuring at one size and painting at another puts the caret on the wrong character,
visibly so at the end of a long string.

## Cursor and clipboard

`Cursor` covers the standard set plus `ColResize` and `RowResize` for track dividers, and `None`
for pointer-locked interactions such as a knob drag.

Clipboard goes through a `ClipboardProvider` trait so a plug-in can route it through the host
rather than the system, which is what some hosts require.
