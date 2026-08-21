# SphereGraphicEngine

> SphereGraphicEngine is a GPU-first graphics and UI engine written in Rust for realtime creative
> applications. It combines a retained node/layout architecture, a WGPU rendering pipeline,
> GPU-native MTSDF text, and specialised realtime visualisation primitives for audio software.

**Status: v0.1, in development. Not production ready.**

---

## What this is

A graphics and UI foundation for software that has to stay responsive while something else is
already using the machine hard — digital audio workstations, audio plug-ins, creative tools,
realtime visualisation.

It is pure Rust. There is no Skia, no C++ rendering core, no browser DOM, no Electron, no CEF, no
Qt, and no JUCE GUI anywhere in the dependency tree.

## What makes it different

**The graphics engine stands alone.** `sphere-render` has no idea that nodes, layout or widgets
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
field with no second pass. Below roughly twelve device pixels — where a distance field runs out of
resolution before a glyph runs out of detail — an isolated grayscale fallback keeps small labels
crisp.

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

## Architecture

```text
Application
    ↓
sphere-ui           element tree, events, focus, widgets
    ↓
sphere-layout       retained nodes, styles, dirty propagation, hit testing
    ↓
sphere-render       canvas → scene → cull → batch → CompiledFrame
    ↓
sphere-wgpu         the only crate that knows wgpu exists
    ↓
D3D12 / Vulkan / Metal / WebGPU
```

| Crate | Responsibility |
|---|---|
| `sphere-core` | Units, geometry, transforms, colour, paths, paint, identity, errors |
| `sphere-render` | Canvas, display list, culling, batching, tessellation, backend seam |
| `sphere-wgpu` | wgpu backend, WGSL shaders, pipeline cache, GPU buffers |
| `sphere-text` | Font discovery, shaping, line layout, MTSDF generation, paged atlas |
| `sphere-layout` | Retained layout tree, style, dirty flags, hit testing, scrolling |
| `sphere-image` | Image decoding, texture cache, fit resolution |
| `sphere-svg` | SVG parsing and cached tessellation for interface assets |
| `sphere-platform` | Windows, input, IME, monitors, frame scheduling |
| `sphere-ui` | Element tree, event dispatch, focus, widgets |
| `sphere-audio-ui` | Meters, waveforms, spectrums, EQ curves, lock-free transfer |
| `sphere` | Facade that re-exports the whole engine |

Backend mapping: Windows → Direct3D 12, Linux → Vulkan, macOS → Metal, Web → WebGPU.

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
> directory ... Access is denied (os error 5)`. Set `CARGO_INCREMENTAL=0` for the run.

## Status

Measured on an NVIDIA GTX 1060 (Vulkan), running
`cargo run -p sphere --example plugin_ui_demo --release` for 180 frames:

| | |
|---|---|
| Tests | 1,020 total, zero warnings, clippy clean, `cargo fmt` clean |
| Quad instances per frame | 10,018 |
| Glyph instances per frame | 178 (Latin, Thai, Japanese, Chinese, Korean, Arabic) |
| Mesh triangles per frame | 1,980 |
| **Layout nodes relaid out** | **0** — across 180 frames of continuous meter animation |
| Layout nodes created / reused | 0 / 39 |
| Glyph texels uploaded | 0 once the atlas is warm |
| CPU per frame | 1.1 ms typical, 2.2 ms worst |

The zero is the point. See [`docs/architecture.md`](docs/architecture.md).

## Examples

```bash
cargo run -p sphere --example desktop_app    --release  # borderless, custom title bar
cargo run -p sphere --example system_window  --release  # the platform draws the title bar
cargo run -p sphere --example plugin_ui_demo --release  # a compressor plug-in editor

# Diagnostic: writes a side-by-side PNG of one line of text, distance field
# against whatever the automatic strategy picks, and reports why.
SPHERE_PROBE_SIZE=13 SPHERE_PROBE_ZOOM=4 cargo run -p sphere-text --example glyph_quad_probe --release -- out.png
```

`desktop_app` is the shape most applications are: header, sidebar, scrolling settings pane, status
bar, runtime theme switching, SVG icons, text fields with input-method support, and keyboard
navigation. It draws its own title bar — the caption buttons use the shell's own Segoe Fluent Icons
glyphs and fade on hover through a spring from the animation core. `system_window` is the same stack
with the platform's title bar instead, which is one line of difference and is the configuration to
reach for first. `plugin_ui_demo` is the audio case:
ten thousand instanced rectangles, multilingual text, and meters driven from a simulated audio
thread through the lock-free boundary.

Both accept `SPHERE_DEMO_FRAMES=<n>` to run for a bounded number of frames and print what they
measured, which makes them usable as smoke tests.

## Documentation

| Document | Contents |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | Crate boundaries, the frame lifecycle, why the seams are where they are |
| [`docs/rendering.md`](docs/rendering.md) | Scene, culling, batching, the instance layouts, shaders, colour |
| [`docs/text.md`](docs/text.md) | Shaping, MTSDF, the atlas, and the small-text policy |
| [`docs/layout.md`](docs/layout.md) | The retained tree, dirty propagation, hit testing |
| [`docs/audio-ui.md`](docs/audio-ui.md) | The audio-thread boundary and realtime primitives |
| [`docs/platform.md`](docs/platform.md) | Windowing, HiDPI, plug-in embedding, frame scheduling |
| [`docs/performance.md`](docs/performance.md) | Targets, what is measured, and how |
| [`docs/roadmap.md`](docs/roadmap.md) | Phase status and what is not built yet |

## Licence

BSD 3-Clause. Copyright (c) 2026 Futureboard Digital Technologies. See [`LICENSE`](LICENSE).
