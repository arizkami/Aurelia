<img width="2542" height="1696" alt="image" src="https://github.com/user-attachments/assets/091e0d6c-9b7b-4d34-8498-9038d8e428c1" />

---

# SphereKit

> SphereKit is a GPU-first graphics and UI engine written in Rust for realtime creative
> applications. It combines a retained node/layout architecture, a WGPU rendering pipeline,
> GPU-native MTSDF text, and specialised realtime visualisation primitives for audio software.

**Status: v0.1, in development. Not production ready.**

---

## What this is

A graphics and UI foundation for software that has to stay responsive while something else is
already using the machine hard — digital audio workstations, audio plug-ins, creative tools,
realtime visualisation.

## What makes it different

**The graphics engine stands alone.** `spherekit-render` has no idea that nodes, layout or widgets
exist. You can drive the canvas directly and never touch the UI layer:

```rust
let mut scene = Scene::new(size(px(400.0), px(300.0)), ScaleFactor::IDENTITY);
{
    let mut canvas = Canvas::new(&mut scene);
    canvas.fill_rounded_rect(RoundedRect::uniform(rect(px(8.0), px(8.0), px(120.0), px(32.0)), px(6.0)),
                             Color::hex(0x1E88E5));
    canvas.draw_glyph_run(run, Color::WHITE);
}
let frame = compiler.compile(&scene, &mut glyphs, &mut textures);
backend.render(&mut handle, frame, Color::hex(0x101214))?;
```

**Paint-only updates cost no layout.** A VU meter repainting sixty times a second marks itself
`PAINT` dirty. Nothing above it relays out, no text reshapes, and no unrelated widget is touched.
This is enforced by the dirty-flag propagation rules, and it is tested rather than asserted.

**Text is a distance field, not a bitmap cache.** Glyphs are rasterised once into a multi-channel
signed distance field with a fourth true-distance channel (MTSDF), then sampled at any size.
Zooming a panel does not re-rasterise anything, and outlines, glows and shadows come from the same
field with no second pass. Below twenty-four device pixels — where a distance field runs out of
resolution before a glyph runs out of detail — an isolated bitmap fallback takes over, vertically
grid-fitted and positioned to a quarter pixel, which is what keeps interface labels crisp at 100 %
scaling. The threshold is on _device_ pixels, so a 13 px label is a bitmap at 100 % and a distance
field at 200 %.

**Realtime audio data never renders from the audio thread.** The boundary is a lock-free snapshot
or ring buffer. Everything on the far side of it — allocation, GPU upload, text shaping, file I/O —
is forbidden on the audio callback, and the architecture documents say so in the place you would
look before breaking it.

**Vector paths are antialiased too.** Rectangles, rounded rectangles and glyphs are smoothed
analytically by their own shaders. A tessellated path has no analytic edge at all, so the surface
and every offscreen layer are multisampled 4× — that is what keeps SVG icons, EQ curves and waveform
outlines from looking jagged.

**Colour is linear, and that is not optional.** `Color` is sRGB with straight alpha; `LinearColor`
is linear-light and premultiplied and is what reaches the GPU. Mixing happens in linear space, so a
black-to-white midpoint is the perceptually correct `0.735`, not a naive `0.5`.

**A widget owns no state.** Every control takes its value and reports changes; nothing is hidden in
the tree. That is what lets a parameter live in a DSP struct, an undo stack or a host automation
lane with no adapter in between.

```rust
knob(self.threshold.get())
    .range(-60.0, 0.0)
    .on_change({ let t = self.threshold.clone(); move |v| t.set(v) })
```

**One stylesheet, two producers.** `spherekit-css` parses CSS — selectors, combinators, specificity,
`!important`, `@media`, custom properties and `var()` — and resolves it to the same `Style` and
`PaintStyle` a hand-written element uses. A native `div()` and a React `<View>` with the same class
land on the same paint style. Syntax the engine does not model is *ignored, never reinterpreted*:
`width: 12` does not become `12px`, and an unknown media feature makes its query false rather than
true. A stylesheet that does nothing is debuggable; one that does something slightly different from
what it says is not.

**React runs here, without a browser.** React 19 reconciles against a native host tree. Each commit
crosses to Rust as one serialisable snapshot — never a mutation stream, so the native side never
observes a half-built tree and each half is testable with none of the other in the process — and is
lowered to native widgets through that same cascade. Events come back by node id, so `onPress` and
`onValueChange` fire without a function ever being serialised.

```tsx
<Panel title="Channel 1" className="strip">
  <Knob value={threshold} minimumValue={-60} maximumValue={0} onValueChange={setThreshold} />
  <Toggle checked={monitor} label="Monitor" onChange={setMonitor} />
</Panel>
```

**And it runs in SphereKit's own V8.** `spherekit-jsengine` embeds V8 directly: no WebView, no Node,
no IPC. The isolate lives on the UI thread in the renderer's address space, so a native call from
JavaScript is synchronous and an `invoke()` from a React effect can settle inside the same commit
that made it. The isolate's microtask policy is explicit, so queued JavaScript runs at the two points
in the frame the host picks and nowhere else — never in the middle of a layout pass. The V8 prebuilt
is Windows x86_64 today; everywhere else the same protocol is driven from a WebView or a child
process.

## Widgets

|            |                                                                                         |
| ---------- | --------------------------------------------------------------------------------------- |
| Buttons    | `button` — primary, secondary, ghost, danger; any width or height; icon-font glyphs     |
| Selection  | `toggle`, `checkbox`                                                                    |
| Values     | `slider`, `fader`, `knob` — stepped, bipolar, formatted, keyboard-adjustable            |
| Text       | `label`, `text_field` — selection, masking, input-method composition, clipboard         |
| Identity   | `avatar` — initials, a tint derived from the name, presence dot                         |
| Menus      | `dropdown` anchored to a control, `context_menu` at a point, `menu_item` with shortcuts |
| Containers | `scroll_view`, `scroll_area`, `panel`, `separator`, `progress` (determinate or not)     |

Cut, copy, paste and select-all live on `TextEdit`, so a keyboard shortcut and a menu item cannot
disagree about what Copy means — including the rule that a masked field never hands its contents to
a global clipboard. A field _reports_ a right-click through `on_context_menu` rather than opening a
menu itself: an element cannot place a popup outside its own box, so the application owns the menu
and therefore owns where it goes.

Scrolling is real scrolling: the wheel moves the innermost container that still has room and chains
outward when it does not, the offset glides to its destination on an eased curve, and overlay
scrollbars draw _over_ the content so showing them never changes what the content is laid out into.
One notch travels as far as the reader's own Windows setting says it should —
`SPI_GETWHEELSCROLLLINES`, including the "one screen at a time" option.

## Architecture

```text
Application                          React application (TypeScript)
    ↓                                    ↓
    │                                spherekit-bridge   JSON Lines: commits, methods, events
    │                                    ↓
    │                                spherekit-react    validate → cascade → lower
    ↓                                    ↓
spherekit-ui           element tree, events, focus, widgets
    ↓
spherekit-layout       retained nodes, styles, dirty propagation, hit testing
    ↓
spherekit-render       canvas → scene → cull → batch → CompiledFrame
    ↓
spherekit-wgpu         the only crate that knows wgpu exists
    ↓
D3D12 / Vulkan / Metal / WebGPU
```

Both entry paths converge at `spherekit-ui`, and nothing below it can tell which one it came from.

| Crate                | Responsibility                                                      |
| -------------------- | ------------------------------------------------------------------- |
| `spherekit-core`     | Units, geometry, transforms, colour, paths, paint, identity, errors |
| `spherekit-render`   | Canvas, display list, culling, batching, tessellation, backend seam |
| `spherekit-wgpu`     | wgpu backend, WGSL shaders, pipeline cache, GPU buffers             |
| `spherekit-text`     | Font discovery, shaping, line layout, MTSDF generation, paged atlas |
| `spherekit-layout`   | Retained layout tree, style, dirty flags, hit testing, scrolling    |
| `spherekit-image`    | Image decoding, texture cache, fit resolution                       |
| `spherekit-svg`      | SVG parsing and cached tessellation for interface assets            |
| `spherekit-platform` | Windows, input, IME, monitors, frame scheduling                     |
| `spherekit-ui`       | Element tree, event dispatch, focus, widgets                        |
| `spherekit-audio-ui` | Meters, waveforms, spectrums, EQ curves, lock-free transfer         |
| `spherekit-css`      | Stylesheet runtime shared by native and React apps                  |
| `spherekit-jsengine` | Embedded V8: isolate, host bindings, microtask and platform pumping |
| `spherekit-bridge`   | JSON Lines protocol between a React front end and the native host   |
| `spherekit-react`    | React renderer, and the Rust host that validates and lowers commits |
| `spherekit-cli`      | `spherekit` command: scaffolds and builds React + Rust apps         |
| `spherekit`          | Facade that re-exports the whole engine                             |

Backend mapping: Windows → Direct3D 12, Linux → Vulkan, macOS → Metal, Web → WebGPU.

> **Transparent windows are Direct3D 12 only.** A transparent surface is composited through a
> DirectComposition visual, which is the DX12 presentation path; NVIDIA's Windows Vulkan WSI
> exposes `Opaque` alpha and nothing else, so a Mica window on Vulkan renders as a black
> rectangle. `spherekit-wgpu` forces DX12 for transparent surfaces and says so in the log.
> `WGPU_BACKEND` still overrides it for diagnostics.

## Building

Requires Rust 1.87 or newer (edition 2024).

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features
```

> **Windows note.** Build from PowerShell or `cmd`, not from Git Bash. Git Bash puts a GNU
> `/usr/bin/link` ahead of the MSVC `link.exe` on `PATH`, and every link step fails with
> `extra operand`.
>
> If a workspace-wide `cargo test` fails at the link step with `LNK1120` while the same crates
> pass individually, the incremental-compilation cache is being interfered with — usually by
> on-access antivirus scanning, and visible as `did not finalize incremental compilation session
directory ... Access is denied (os error 5)`. Set `CARGO_INCREMENTAL=0` for the run.

### Quickstart: the React example app

`app/reactdemo` is React 19 in SphereKit's own V8 isolate, styled by the shared CSS runtime,
rendered on the GPU:

```bash
cargo run -p reactdemo --release
```

That works with no JavaScript toolchain installed. Without Bun, `build.rs` falls back to a renderer
written directly against the wire protocol — no React, no bundler — which still runs, still styles
itself from the same `styles/app.css`, and is worth reading because it shows what React's reconciler
eventually produces: one `commit` frame. For the React renderer:

```bash
cd app/reactdemo/renderer && bun install && cd ../../..
cargo run -p reactdemo --release
```

The status bar at the bottom of that window is a native `div()` no React component knows exists, and
it is resolved through the very same stylesheet rules. One cascade, two producers, no browser.

The `v8` feature is off by default everywhere, including on the facade, because the prebuilt is a
~100 MB Windows x86_64 download. `cargo build -p spherekit --features v8` opts in.

```bash
cargo run -p spherekit-bridge --features v8 --example js_roundtrip   # the whole path, no bundler
cargo run -p spherekit-jsengine --example repl -- path/to/script.js  # the smallest embedder
```

### SphereKit CLI

The workspace includes a portable `spherekit` CLI for creating and building a
React + Rust app. Install it from a checkout or run it through Cargo:

```bash
cargo install --path crates/spherekit-cli
spherekit react my-app --target current
cd my-app
spherekit build --release
```

`spherekit react` accepts `windows`, `macos`, `linux`, or an explicit Rust
target triple. `spherekit build` runs the React typecheck/compile and the native
Cargo build together; `--dry-run`, `--no-react`, `--no-rust`, and `--out-dir`
are available for CI and cross-compilation workflows.

The React package and native host communicate through the `spherekit-bridge`
JSON Lines protocol. It supports committed trees, request/response API calls,
and native events, while leaving the underlying transport open to WebView, FFI,
child process, or socket integrations.

`spherekit-css` provides the shared stylesheet runtime for native and React
apps. Use `Stylesheet`/`ResolvedStyle` in native code, or call
`bridge.setStylesheet(css)` and use `className`, `id`, and inline `style` props
from React. See [the CSS design note](docs/spherekit-css.md) for the research
tradeoffs and supported v1 property boundary, [`docs/react.md`](docs/react.md)
for the frontend end to end, and [`docs/javascript.md`](docs/javascript.md) for
the V8 embedding.

## Status

Measured on an NVIDIA GTX 1060 (Vulkan), running
`cargo run -p spherekit --example plugin_ui_demo --release` for 180 frames:

|                               |                                                             |
| ----------------------------- | ----------------------------------------------------------- |
| Tests                         | 1,571 Rust and 53 TypeScript, zero warnings, clippy clean, `cargo fmt` clean |
| Quad instances per frame      | 10,018                                                      |
| Glyph instances per frame     | 178 (Latin, Thai, Japanese, Chinese, Korean, Arabic)        |
| Mesh triangles per frame      | 1,980                                                       |
| **Layout nodes relaid out**   | **0** — across 180 frames of continuous meter animation     |
| Layout nodes created / reused | 0 / 39                                                      |
| Glyph texels uploaded         | 0 once the atlas is warm                                    |
| CPU per frame                 | 1.1 ms typical, 2.2 ms worst                                |

The zero is the point. See [`docs/architecture.md`](docs/architecture.md).

## Examples

```bash
cargo run -p uigallery                          --release  # every widget, live
cargo run -p reactdemo                          --release  # React 19 in an in-process V8 isolate
cargo run -p spherekit --example desktop_app    --release  # borderless, custom title bar
cargo run -p spherekit --example system_window  --release  # the platform draws the title bar
cargo run -p spherekit --example plugin_ui_demo --release  # a compressor plug-in editor

# Diagnostic: writes a side-by-side PNG of one line of text, distance field
# against whatever the automatic strategy picks, and reports why.
SPHEREKIT_PROBE_SIZE=13 SPHEREKIT_PROBE_ZOOM=4 cargo run -p spherekit-text --example glyph_quad_probe --release -- out.png

# The same probe with RGB coverage off, which is what a transparent window gets.
SPHEREKIT_PROBE_SUBPIXEL=0 SPHEREKIT_PROBE_SIZE=10 cargo run -p spherekit-text --example glyph_quad_probe --release
```

**`app/uigallery`** is the reference application: seven pages under a custom Windows frame over DWM
Mica, one per widget family, with a note on each specimen saying what that variant is _for_ — the
part an API listing cannot tell you. Nothing in it is a mock-up; the toggles toggle and the sliders
drag, because a gallery that showed pictures would be a worse document than the source it
documents.

`desktop_app` is the shape most applications are: header, sidebar, scrolling settings pane, status
bar, runtime theme switching, SVG icons, text fields with input-method support, and keyboard
navigation. It draws its own title bar — the caption buttons use the shell's own Segoe Fluent Icons
glyphs and fade on hover through a spring from the animation core. `system_window` is the same stack
with the platform's title bar instead, which is one line of difference and is the configuration to
reach for first. `plugin_ui_demo` is the audio case:
ten thousand instanced rectangles, multilingual text, and meters driven from a simulated audio
thread through the lock-free boundary.

Both accept `SPHEREKIT_DEMO_FRAMES=<n>` to run for a bounded number of frames and print what they
measured, which makes them usable as smoke tests.

## Documentation

| Document                                         | Contents                                                                |
| ------------------------------------------------ | ----------------------------------------------------------------------- |
| [`docs/architecture.md`](docs/architecture.md)   | Crate boundaries, the frame lifecycle, why the seams are where they are |
| [`docs/rendering.md`](docs/rendering.md)         | Scene, culling, batching, the instance layouts, shaders, colour         |
| [`docs/text.md`](docs/text.md)                   | Shaping, MTSDF, the atlas, and the small-text policy                    |
| [`docs/layout.md`](docs/layout.md)               | The retained tree, dirty propagation, hit testing                       |
| [`docs/audio-ui.md`](docs/audio-ui.md)           | The audio-thread boundary and realtime primitives                       |
| [`docs/platform.md`](docs/platform.md)           | Windowing, HiDPI, plug-in embedding, frame scheduling                   |
| [`docs/performance.md`](docs/performance.md)     | Targets, what is measured, and how                                      |
| [`docs/api-bridge.md`](docs/api-bridge.md)       | React/native JSON Lines API and event bridge                            |
| [`docs/spherekit-css.md`](docs/spherekit-css.md) | The stylesheet runtime and its v1 property boundary                     |
| [`docs/react.md`](docs/react.md)                 | JSX to native widgets, and why a commit crosses as a whole tree         |
| [`docs/javascript.md`](docs/javascript.md)       | Embedding V8: the engine surface, the prelude, bundling, the frame tick |
| [`docs/roadmap.md`](docs/roadmap.md)             | Phase status and what is not built yet                                  |

## Licence

BSD 3-Clause. Copyright (c) 2026 Futureboard Digital Technologies. See [`LICENSE`](LICENSE).
