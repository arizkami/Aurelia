# Architecture

Why the seams are where they are.

## The shape of the thing

```text
Application
    ↓
spherekit-ui              element tree, event dispatch, focus, widgets
    ↓
spherekit-layout          retained nodes, style, dirty flags, hit testing
    ↓
spherekit-render          canvas → scene → cull → batch → CompiledFrame
    ↓
spherekit-wgpu            the only crate that knows wgpu exists
    ↓
D3D12 / Vulkan / Metal / WebGPU
```

Dependencies point downward and never upward. `spherekit-render` does not know what a node is.
`spherekit-core` does not know what a GPU is.

## The three rules that shaped everything else

### 1. The graphics engine must stand alone

`spherekit-render` has no dependency on `spherekit-layout` or `spherekit-ui`. Drawing does not require nodes:

```rust
let mut scene = Scene::new(viewport, scale);
{
    let mut canvas = Canvas::new(&mut scene);
    canvas.fill_rect(rect, color);
    canvas.draw_glyph_run(run, brush);
}
```

This is not a nicety. A plug-in that wants a custom oscilloscope and nothing else should not have to
instantiate a widget tree, and a benchmark of the rasteriser should not be measuring layout.

### 2. Paint-only updates must not cost layout

A VU meter repainting sixty times a second marks itself `PAINT` dirty. That must not trigger
relayout, text reshaping, or work on any unrelated widget.

The dirty-flag system is what enforces it:

```rust
bitflags! {
    pub struct DirtyFlags: u32 {
        const STYLE     = 1 << 0;
        const LAYOUT    = 1 << 1;
        const PAINT     = 1 << 2;
        const TEXT      = 1 << 3;
        const CHILDREN  = 1 << 4;
        const TRANSFORM = 1 << 5;
    }
}
```

`LAYOUT` propagates to ancestors, because a child's size can change a parent's. `PAINT` does not
propagate at all. `spherekit-layout` exposes a "nodes relaid out this pass" counter specifically so
this property is *measured* rather than asserted, and there is a test that marks a deep node
`PAINT`-dirty and asserts the counter stays at zero.

### 3. Backend details must not leak

Three types are load-bearing here and none of them appear in a public signature outside their own
crate:

| Type | Hidden behind |
|---|---|
| `wgpu::*` | `RendererBackend`, `CompiledFrame` |
| `taffy::*` | `LayoutEngine`, SphereKit's own `Style` |
| `winit::*` | SphereKit's own `Window`, `WindowEvent`, `Key` |
| `lyon::*` | `Tessellator`, `Mesh` |

The point is not purity. It is that `CompiledFrame` is pure data, so the batch compiler is testable
without a device; and that a native D3D12 or software backend can be added later without
redesigning anything above it.

## Crates

| Crate | Owns | Depends on |
|---|---|---|
| `spherekit-core` | Units, geometry, transforms, colour, paths, paint, ids, errors | — |
| `spherekit-render` | Canvas, scene, culling, batching, tessellation, backend trait | core |
| `spherekit-wgpu` | wgpu backend, WGSL, pipelines, GPU buffers, textures | core, render |
| `spherekit-text` | Font db, shaping, line layout, MTSDF, atlas, caches | core |
| `spherekit-layout` | Retained tree, style, dirty flags, hit testing, scrolling | core |
| `spherekit-image` | Decoding, texture cache, fit resolution | core |
| `spherekit-svg` | SVG parsing, cached tessellation | core, render |
| `spherekit-platform` | Windows, input, IME, monitors, scheduling | core |
| `spherekit-ui` | Elements, events, focus, widgets | core, render, layout, text, platform |
| `spherekit-css` | CSS parsing, selector cascade, native style adapter | core, layout, ui |
| `spherekit-audio-ui` | Meters, waveforms, spectrums, lock-free transfer | core, render, ui |
| `spherekit` | Facade | all |
| `spherekit-react` | React host tree and native lowering | ui, core |
| `spherekit-bridge` | JSON Lines API and event transport | spherekit-react |
| `spherekit-cli` | React scaffolding and cross-platform builds | — |

The React integration adds four boundary layers: `spherekit-css` owns the
shared stylesheet runtime, `spherekit-react` owns the host-tree adapter,
`spherekit-bridge` owns the transport protocol, and `spherekit-cli` owns
project/build orchestration. They remain outside the GPU layer so the native
engine stays usable without React.

## The frame lifecycle

```text
build / update        views produce elements
      ↓
style resolution      element styles → layout styles
      ↓
layout                only if something is LAYOUT-dirty
      ↓
prepaint              resolve hit regions, focus, scroll offsets
      ↓
paint                 elements record into a Scene via Canvas
      ↓
scene compilation     cull, batch, tessellate → CompiledFrame
      ↓
GPU render            RendererBackend
```

Stages are skippable. An idle window with nothing dirty runs none of them. A paint-only frame runs
prepaint onward. Only a structural or style change runs the whole sequence.

## Identity and lifetime

Every resource that outlives a frame gets a typed generational handle: `TextureId`, `ImageId`,
`FontId`, `NodeId`, `ViewId`, `WindowId`, `SvgId`, `FocusId`.

The generation is what makes a stale handle *detectable*. Reusing a slot after a font or texture is
evicted would otherwise silently hand back someone else's data — the class of bug that is impossible
to reproduce. `GenerationalStore` bumps the generation on slot reuse, and a test asserts that a
handle to a removed entry does not resolve to the entry that replaced it.

`ElementId` is separate: it is the stable identity a retained tree needs across rebuilds, so that
state, focus and animation survive a re-render. `ElementId::from_key` derives it from a hashable key
and sets the high bit; `ElementId::unique` mints from a counter and does not. They cannot collide.

## Units

`Px` is logical. `DevicePx` is physical. `ScaleFactor` converts, and there is deliberately no
`From<Px> for DevicePx` that could do it silently.

Fractional scaling is first-class. Two rules follow from it:

- **Integral conversions round.** Surface extents, scissor rectangles and texture sizes are whole
  device pixels.
- **Vertex data does not.** `ScaleFactor::to_device_f32` is unrounded, because rounding vertex
  positions is exactly what produces unstable geometry at 125 % and 150 %.

`Rect<Px>::round_out` expands to the nearest enclosing whole-device-pixel rectangle, for invalidation
and scissors, where covering slightly too much is correct and covering slightly too little is a
visible artifact.

## Concurrency

The engine does not introduce threads for their own sake. The UI and render paths are coordinated on
one thread; expensive asset work moves off it:

| Thread | Work |
|---|---|
| UI / render | Build, layout, paint, compile, submit |
| Asset worker | Image decoding, SVG parsing |
| Glyph worker | MTSDF generation |
| Audio (external) | DSP. Never touches the engine. |

The audio boundary is the one that is not negotiable, and it has its own document:
[`audio-ui.md`](audio-ui.md).

## Error handling

SphereKit does not panic for conditions a running application can legitimately hit. A lost surface, a
minimised window, a missing font, a full atlas — all of them are values, and the caller decides the
policy.

`SurfaceError` is the clearest example. Every variant falls into exactly one of three recovery
classes, and a test asserts the classes are disjoint and total:

| Class | Response |
|---|---|
| Transient — `Timeout`, `Occluded`, `ZeroSized` | Skip the frame |
| Reconfigure — `Outdated`, `Lost` | Reconfigure and retry |
| Fatal — `DeviceLost`, `OutOfMemory` | Rebuild everything |

Without that partition a frame loop either spins forever or tears down state it did not need to.

## Memory

Every cache has a key, a byte budget, an eviction policy and a hit/miss counter. There is no cache in
the engine that can grow without bound, because unbounded growth in a plug-in that runs for eight
hours inside someone else's process is not a performance problem, it is a crash.

Per-frame allocation is designed out rather than optimised later:

- `Scene::reset` keeps every allocation.
- `GrowableBuffer` grows geometrically and never shrinks in normal operation.
- `Tessellator` holds its scratch buffers across frames.
- Offscreen layer targets come from a pool.
- `SmallVec` inline capacity covers the common case for glyph runs, gradient stops and the canvas
  state stack.

## Testing

Roughly two thirds of the test suite runs with no GPU, because roughly two thirds of the engine has
no business needing one. Geometry, colour, paths, scenes, culling, batching, layout and dirty
propagation are all pure functions of their input.

What does need a device is isolated into `crates/spherekit-wgpu/tests/pipelines.rs`, which creates a
headless adapter, validates every WGSL module through naga, and builds every pipeline for both the
swapchain and the layer format. On a machine with no adapter it skips rather than fails.
