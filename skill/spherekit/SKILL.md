---
name: spherekit
description: Work on SphereKit, the GPU-first Rust graphics and UI engine in this repository — its rendering, layout, text, CSS, widget, audio-visualisation, React/V8 and platform layers, and the applications built on them. Use when touching anything under crates/spherekit-*, app/uigallery, app/reactdemo or app/musicplayer; when writing a native UI with div()/label()/widgets; when working on the CSS runtime, the React host, the API bridge or the V8 embedding; when adding realtime audio visualisation; or when building, testing, versioning or publishing any of it.
---

# SphereKit

A GPU-first graphics and UI engine in pure Rust for realtime creative
applications: digital audio workstations, audio plug-ins, creative tools and
visualisation. Sixteen crates plus an npm package, all at `2026.8.0`.

## Read this first

**Run every cargo command through the PowerShell tool, never Bash.** Git Bash's
`link.exe` shadows the MSVC linker and every link fails with a message blaming
Visual Studio. See `skill-data/pitfalls.md`.

## What makes this engine what it is

Every design decision is downstream of one workload: software that must stay
responsive while something else is already using the machine hard. If a change
you are making conflicts with one of these, you are probably solving the wrong
problem.

- **Paint-only updates cost no layout.** A meter repainting sixty times a second
  marks `PAINT`; nothing relayouts and no text reshapes. This is why layout
  style and paint style are separate structs.
- **An unchanged tree is free.** `UiTree::build` reconciles by identity; an
  unchanged frame creates no nodes and lays out nothing. Styling now matches
  this via a resolved-style cache.
- **Text is a distance field.** Glyphs are rasterised once and sampled at any
  size, so zooming re-rasterises nothing.
- **Realtime audio never renders from the audio thread.** The boundary is a
  lock-free ring or snapshot, and the ring is allowed to drop rather than block.
- **Colour is linear.** Blending and gradient interpolation happen in linear
  light.
- **The backend is a seam.** No `wgpu` type appears in any public API outside
  `spherekit-wgpu`; no `taffy` type appears outside `spherekit-layout`.
- **Unsupported input is ignored, never reinterpreted.** The CSS engine drops
  `width: 12` rather than guessing `12px`; the SVG loader refuses a document it
  cannot render rather than returning a silently empty icon.

## Where to look

Load the reference file for the area you are working in. They live in
`skill-data/` at the repository root.

| File | Covers |
|---|---|
| `skill-data/crates.md` | The sixteen crates, what is in each, the layering, feature flags, the three example applications |
| `skill-data/ui.md` | Elements, `Styled`/`Interactive`/`ParentElement`, the invalidation split, the frame loop, widgets, theming |
| `skill-data/css.md` | `Stylesheet`, resolving, what is supported and what is deliberately absent, and why the cascade is shaped for speed |
| `skill-data/react-v8.md` | The React host, the API bridge protocol, custom host types, the V8 embedding, the prelude, bundling, the two non-optional performance rules |
| `skill-data/audio-and-text.md` | The audio-thread boundary, the transports, the realtime draw calls, meter ballistics, shaping, and why a mark's zero `y_offset` is correct |
| `skill-data/pitfalls.md` | Thirteen traps that have each already cost real time here |
| `skill-data/workflow.md` | Commands, house style, the testing standard, and measuring before optimising |

The `docs/` directory holds the long-form design documents these summarise:
`architecture.md`, `rendering.md`, `layout.md`, `text.md`, `platform.md`,
`audio-ui.md`, `spherekit-css.md`, `react.md`, `api-bridge.md`,
`javascript.md`, `performance.md`, `roadmap.md`.

## The shortest possible orientation

A native window:

```rust
let mut surface = SphereKitSurface::new(window, size, scale, SurfaceOptions::default()).await?;
surface.render(
    div().flex_col().p(px(16.0)).child(label("Threshold")).into_element(),
    Color::hex(0x14161A),
)?;
```

A React window: the same `surface.render`, but the element tree comes from
`bridge.host().ui_element_with_events(&queue)` after a commit arrives from V8.
`app/reactdemo` is the smallest complete version; `app/musicplayer` is the full
one, including realtime visualisers registered as custom host types.

Nothing above is required. The canvas stands alone with no nodes, no layout and
no widgets:

```rust
let mut scene = Scene::new(size(px(400.0), px(300.0)), ScaleFactor::IDENTITY);
let mut canvas = Canvas::new(&mut scene);
canvas.fill_rect(rect(px(8.0), px(8.0), px(120.0), px(32.0)), Color::hex(0x1E88E5));
```

## Working here

House style is strict and the code is heavily commented in a particular
register — comments justify decisions rather than restate signatures, and tests
are named as sentences describing the property they protect. Read
`skill-data/workflow.md` before writing code, and match the surrounding file.

Three standing rules worth repeating:

1. `#![deny(missing_docs)]` is on everywhere. Every public field needs a doc
   comment; a missing one is a compile error.
2. `cargo clippy --all-targets -- -D warnings` must be clean, and an `#[allow]`
   added to silence something is not a fix.
3. Measure before optimising. The last time performance work happened here, the
   two most plausible hypotheses were both disproved by measurement and the real
   cost was somewhere neither suggested.
