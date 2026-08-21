# Platform

Windows, input, HiDPI, and the parts of "not owning the process" that matter.

## The seam

`sphere-platform` has two jobs: describe the hardware faithfully, and keep `winit` from leaking.

**No `winit` type appears in the public API.** Sphere has its own `WindowEvent`, `Key`, `NamedKey`,
`MouseButton`, `Modifiers`, `ScrollDelta`, `Cursor` and `MonitorInfo`. The winit backend lives in
`backend/winit.rs` behind a feature, so a native Win32, AppKit or Wayland backend can replace it
without touching anything above.

`sphere-ui` then has its *own* input types again, and translates at `sphere_ui::InputTranslator`.
That is a deliberate second boundary rather than duplication:

| Layer | Answers |
|---|---|
| `sphere-platform` | What did the window system say? |
| `sphere-ui` | What interaction is this? |

A platform `MouseInput` carries no coordinates, because the window system does not send any. A UI
`MouseDown` must carry them, because every handler needs them. Something has to remember the last
cursor position, the held buttons, the modifier set and the click count; `InputTranslator` is that
something, one per window.

## Logical and physical pixels

Every position `sphere-platform` reports is in **logical** pixels (`Point<Px>`); every extent is in
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
reconfigured before the next frame, and `SphereSurface::resize` handles a zero extent by doing
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
`SphereSurface::new` accept either. `WindowAttributes::with_parent` covers the case where the
engine creates a child window inside a host-supplied parent.

`App::run` is documented as main-thread-only and **not for plug-ins** — a plug-in must not call it
at all. The host drives the loop; the plug-in renders on demand.

Host lifecycle concerns worth stating plainly:

- The host may resize the editor at any time, including to zero while it is hidden.
- The host may destroy the parent window without warning. Every surface must be releasable
  synchronously.
- Several instances of the same plug-in share a process. Nothing in the engine may be a process
  global, which is why there is no global renderer, no global font database and no global cache.
- The host owns window focus. Sphere's focus registry is deliberately independent of it; see
  `sphere_ui::FocusRegistry`.

## Multiple windows

`WindowRegistry` holds every open window with its own size, scale factor, focus state and redraw
policy. A main window, a floating mixer, a plug-in editor and a modal each get their own
`SphereSurface`; the GPU device is shared where the backend allows it.

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

`ImeEvent` covers `Enabled`, `Preedit { text, cursor }`, `Commit(text)` and `Disabled`. A text field
displays the pre-edit string inline and commits on `Commit`. `set_ime_cursor_area` tells the
platform where to put the candidate window; without it the candidate list appears in the wrong place
for CJK input, which is a complete usability failure for the languages that need it most.

## Cursor and clipboard

`Cursor` covers the standard set plus `ColResize` and `RowResize` for track dividers, and `None`
for pointer-locked interactions such as a knob drag.

Clipboard goes through a `ClipboardProvider` trait so a plug-in can route it through the host
rather than the system, which is what some hosts require.
