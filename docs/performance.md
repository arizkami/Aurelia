# Performance

What is measured, where the counters are, and what they are protecting.

This document does not contain benchmark figures for hardware you do not have. It names the
counters, says what each one defends, and reports the numbers actually observed on the one machine
this was developed against.

## Targets

| Goal | Enforced by |
|---|---|
| Idle CPU near zero when nothing changes | `RedrawPolicy::Idle` blocks the loop |
| No layout work for paint-only updates | `DirtyFlags::PAINT` does not propagate |
| Very low allocation per frame | Reused scenes, growable buffers, pooled targets |
| Minimal draw calls | Run-length batching, one pipeline for the common primitive |
| Fast text cache hits | Shaping cache, size-independent glyph fields |

## The counters

Every layer reports what it did. These are the numbers to watch; a regression shows up here before
it shows up as a dropped frame.

### `LayoutStats` — `spherekit-layout`

| Counter | Watch for |
|---|---|
| `nodes_laid_out` | **Anything but zero on a paint-only frame.** The headline number. |
| `nodes_restyled` | Growth means styles are rebuilt when they did not change |
| `passes` / `skipped_passes` | A low skip ratio means something is over-invalidating |

`nodes_laid_out` counts layout-algorithm *invocations*, not distinct nodes: a flex container may
size a child twice under different constraints, and hiding that would make the number reassuring
rather than useful.

### `TreeStats` — `spherekit-ui`

| Counter | Watch for |
|---|---|
| `nodes_created` | Anything but zero on a steady frame means identity is unstable |
| `nodes_reused` | Should equal `elements` in steady state |
| `nodes_removed` | Churn means elements are appearing and disappearing |
| `elements_culled` | Should be large in a scrolled list; zero means culling is not working |

### `SceneStats` — `spherekit-render`

| Counter | Watch for |
|---|---|
| `commands` | Total recorded |
| `culled` | Rejected before an instance was written |
| `batches` | **Draw calls.** A jump at constant wall-clock is still a regression |
| `quads` / `glyphs` / `triangles` | Instance counts by kind |

### `FrameStats` — `spherekit-wgpu`

| Counter | Watch for |
|---|---|
| `draw_calls`, `pipeline_switches` | State changes, the thing batching exists to reduce |
| `bytes_uploaded` | Should fall to near-constant once caches are warm |
| `layers` | Each one is a render target and an extra pass |

### Text and atlas

| Counter | Watch for |
|---|---|
| `TextSystemStats::hit_rate` | Should sit near 1.0; a low rate means a key changes every frame |
| `glyph_texels_uploaded` | Should reach zero once every on-screen glyph is rasterised |
| `AtlasStats::utilization` | Low with many pages means packing is fragmenting |
| `CacheStats::collisions` | Expected to stay at zero forever |

### Buffers

`GrowableBuffer::grew()` counts reallocations over the buffer's lifetime. In steady state it stops
increasing. A number that keeps climbing means a workload is still growing, or is oscillating
around a capacity boundary.

## Per-frame allocation

Designed out rather than optimised later:

- `Scene::reset` clears nine vectors and keeps every allocation. A fresh `Scene` per frame would
  re-grow all nine every time.
- `GrowableBuffer` grows geometrically to a power of two and never shrinks in normal operation.
- `Tessellator` holds `lyon`'s scratch buffers across frames.
- Offscreen layer targets come from a pool keyed on size, released after 60 unused frames. Freeing
  them the moment one frame skips them would reallocate every time a menu opened and closed.
- `RealtimeFrame` carries a scratch `Vec` for envelope extraction; a test asserts its capacity stops
  changing after the second frame.
- Hit testing has a `hit_test_all_into` variant, because hit testing runs on every mouse move and
  allocating there would be a per-frame allocation in the most frequent code path in the engine.
- `SmallVec` inline capacity covers glyph runs, gradient stops, the canvas state stack, the hit
  chain and handler lists.

## Caches

Every cache has a key, a byte budget, an eviction policy and hit/miss counters. There is no cache in
the engine that can grow without bound, because unbounded growth inside a host process that runs for
eight hours is not a performance problem, it is a crash.

| Cache | Keyed on | Bounded by |
|---|---|---|
| Shaping | text + style + width | Bytes, LRU |
| Glyph atlas | `GlyphKey` | Page count, LRU page eviction, idle-frame eviction |
| Image | Content hash | Bytes, LRU, with a this-frame guard |
| Pipeline | kind + target format + sample count | Bounded by construction — a fixed set |
| SVG geometry | id + size + scale | Bytes, LRU |

## What multisampling costs

The surface and every offscreen layer are multisampled 4× so that tessellated paths have smooth
edges. That is not free:

- The surface's multisampled attachment is `width × height × 4 bytes × samples`. At 1180×720 that is
  roughly 13 MB; at 4K it is roughly 130 MB.
- Every layer allocates a second texture, because a multisampled texture cannot be sampled and has
  to resolve into a plain one before compositing.
- `WgpuRenderer::memory_usage` accounts for both, so the cost is visible rather than inferred.

Two things keep it from being worse than it needs to be. The multisampled attachment stores
`Discard` on any pass that will not be resumed, so the 4× buffer is never written back to memory —
only the resolve is. And the count is chosen once at startup from what the formats actually support,
so a device that cannot manage 4× silently runs at the highest it can rather than failing.

`SurfaceOptions::msaa_samples = 1` turns it off entirely. Everything except tessellated paths is
antialiased analytically and looks identical either way.

## Culling and batching

Culling happens **before** an instance is written, not after, and increments `SceneStats::culled`.
Two cases need care and both have tests: a shadow must be culled against its blurred footprint, and
a stroke against bounds grown by half its width.

Batching is run-length over *adjacent* compatible commands. A global sort would batch better and
draw wrong, because UI is painted back to front. Within a compatible run the merge is total: 500
rectangles sharing a clip become one draw call, and a test asserts exactly that.

## Measured on one machine

NVIDIA GTX 1060 3 GB, Vulkan, Windows 11, `--release`, 180 frames of the plug-in demo:

```text
draw calls:          26
pipeline switches:   25
quad instances:      10018
glyph instances:     178
mesh triangles:      1980
elements built:      39
elements painted:    39
nodes created:       0
nodes reused:        39
nodes laid out:      0
worst laid out:      0
cpu this frame:      1.150 ms
worst cpu frame:     2.243 ms
glyph texels up:     0
```

Read that as: ten thousand rectangles, multilingual MTSDF text and three continuously-animating
realtime visualisations, at roughly one millisecond of CPU per frame, with the layout engine doing
**nothing at all**.

The 26 draw calls are what painter's order costs. The frame alternates panel backgrounds, labels
and meshes, and merging across that boundary would draw the labels underneath the panels.
Twenty-six for a full plug-in editor is the expected shape, not a defect.

These are figures from one GPU on one machine, reported because they were measured. They are not a
guarantee about yours.

## Reproducing

```bash
# Bounded run that prints the report above.
SPHEREKIT_DEMO_FRAMES=180 cargo run -p spherekit --example plugin_ui_demo --release

# GPU-side validation: every shader through naga, every pipeline built.
cargo test -p spherekit-wgpu --test pipelines -- --nocapture
```

On Windows, set `CARGO_INCREMENTAL=0` if a workspace-wide test run fails at the link step; see the
build notes in the README.

## Not yet measured

Honesty about the gaps:

- There is **no Criterion benchmark suite yet.** The counters above are what exists, and the demo's
  bounded run is the only end-to-end measurement. A benchmark crate is on the roadmap.
- GPU frame time is not measured. `FrameStats::gpu_ms` is `None`; timestamp queries are detected as
  a capability but not yet used.
- No golden-image or screenshot tests exist, and there is no GPU readback path to build them on.
  `spherekit-text`'s `glyph_quad_probe` example renders text to a PNG, but it re-implements the glyph
  shader on the CPU rather than capturing a frame, so it can check geometry and not the GPU. The
  two examples print a measured report under `SPHEREKIT_DEMO_FRAMES`, which catches *structural*
  regressions — a jump in draw calls, a nonzero `nodes_laid_out` — but not visual ones.
- The figures above come from one discrete NVIDIA GPU. Integrated graphics, Metal and a software
  adapter are architecturally supported and untested for performance.
