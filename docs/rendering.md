# Rendering

How a canvas call becomes pixels, and why each step exists.

## The pipeline

```text
Canvas calls
   ↓  clip and transform stacks resolved to indices AT RECORD TIME
Scene                    flat command list + side tables
   ↓  cull against the device-space scissor
   ↓  batch: run-length merge of adjacent compatible commands
   ↓  tessellate paths (lyon), bake mesh transforms
CompiledFrame            instance buffers, transform/clip/gradient tables,
                         batches, passes, targets — pure data, no GPU
   ↓
RendererBackend          sphere-wgpu
   ↓
D3D12 / Vulkan / Metal / WebGPU
```

The split between `Scene`, `CompiledFrame` and the backend is the load-bearing decision in this
crate. `CompiledFrame` contains no GPU handles and is produced by code that never touches a device,
which is what makes the batch compiler unit-testable and benchmarkable on a machine with no GPU at
all. Roughly half of `sphere-render`'s tests exercise it.

## Why the canvas does not draw

`canvas.fill_rect(...)` appends a record. It never issues a draw call. Two properties follow:

**Order is preserved and stack work is not repeated.** The clip and transform stacks are resolved
*while recording*, so each `DrawCommand` carries a resolved index into `Scene::transforms` and
`Scene::clips`. Nothing downstream re-walks a stack. A clip is stored in scene-absolute
coordinates, so the backend gets a scissor rectangle and has no transform left to apply.

**Commands stay small.** A `DrawCommand` is a fixed-size tagged record — asserted at 128 bytes or
under by a test — that indexes into side tables. Paths, gradients, glyph runs and meshes live in
their own arenas. Ten thousand rectangles are ten thousand compact records and zero per-rectangle
heap allocations.

`Scene::reset` clears every vector while keeping its allocation, so steady-state frame building
allocates nothing. A fresh `Scene` per frame would re-grow nine vectors every time.

## Batching: run-length, never sorted

UI is painted back to front and overlapping translucent content depends on that order. So batching
merges only *adjacent* compatible commands. A global sort would batch better and draw wrong.

```rust
canvas.fill_rect(a, RED);       // ─┐
canvas.draw_image(img, b);      //  │ three batches, not two —
canvas.fill_rect(c, BLUE);      // ─┘ merging the rects hides the image
```

A batch breaks when the pipeline kind, the scissor rectangle, the bound texture or the render
target changes. Each of those is a real GPU state change, and `FrameStats::pipeline_switches`
reports them so a regression is visible rather than inferred.

Within a compatible run, batching is total: five hundred rectangles with the same clip become one
instanced draw call. A test asserts exactly that.

## Culling happens before an instance exists

`BatchCompiler::visible` computes the device-space scissor and rejects the command *before* writing
an instance, not after. Rejected commands increment `SceneStats::culled`.

Two cases need care and both are tested:

- A **shadow** must be culled against its blurred footprint, not its shape. A rectangle twenty
  pixels off the left edge with a forty-pixel blur still spills visibly into view.
- A **stroke** must be culled against bounds grown by half the stroke width.

## Primitives: one pipeline for the common case

Solid fills, rounded rectangles, borders, gradients, textured quads and box shadows all share the
`QuadInstance` layout and therefore one pipeline. They differ only in how the fragment is coloured.
Splitting them would multiply pipeline switches across a frame that is overwhelmingly made of
exactly this primitive.

Circles go through it too: a circle is a rounded rectangle whose radius is half its extent. That
keeps circles analytic and sharp instead of tessellating them into a mesh.

Paths are the exception. They tessellate through `lyon` into `MeshVertex` triangles — but none of
lyon's types appear in any public signature, so the tessellator is replaceable.

## Instance layouts

```rust
#[repr(C)]
pub struct QuadInstance {           // 96 bytes
    bounds: [f32; 4],               // local logical px
    radii: [f32; 4],                // clamped [tl, tr, br, bl]
    color: [f32; 4],                // linear premultiplied
    border_color: [f32; 4],         // (or the source rect for textured quads)
    border_width: f32,
    blur_sigma: f32,
    flags: u32,
    gradient: u32,                  // index, or u32::MAX
    transform_index: u32,
    clip_index: u32,
    _pad: [u32; 2],
}
```

Two decisions shape this:

**Transforms and clips are indices, not inline matrices.** Sibling elements overwhelmingly share
both. Inlining a six-float matrix and a clip rectangle in every instance would roughly double the
bytes uploaded for a typical panel-heavy frame. They live in storage-buffer tables bound once per
pass.

**Colours are linear premultiplied on the CPU.** The conversion happens once while building the
instance, never per fragment.

The source rectangle for a textured quad reuses the `border_color` slot, which is unused in that
case. That is a deliberate overload rather than growing the struct for every quad in the frame; the
shader comment and the emitter comment both say so.

`GlyphInstance` mirrors the layout at the same 96 bytes. Both are `Pod`, so `Vec<QuadInstance>`
becomes a vertex buffer with one `cast_slice` and no per-element work. A compile-time assertion
pins both sizes.

## Layers are allocated only when they change the result

`Canvas::push_layer` returns `bool`. A fully opaque, normally-blended, unfiltered group needs no
offscreen target, and it returns `false` without recording anything. Opacity applied to individual
primitives is exact as long as they do not overlap; only overlapping content needs the group
semantics of a real layer.

When a layer *is* created:

- It is sized to its content, not to the window, and clamped to twice the surface size so a runaway
  transform or an unclipped shadow cannot allocate a gigabyte of render target.
- Its target origin is subtracted in the vertex stage, which is why `PassUniforms` carries a
  `target_origin` that `FrameUniforms` does not.
- It always clears to transparent, because the texture came from a pool and may hold a previous
  frame's content.
- Only the first pass on a target clears it. A target resumed after a nested layer preserves what it
  already holds.

`Canvas` closes any layer left open on `Drop` and on `restore`. An unbalanced command stream would
desync the backend's target stack in a way that only shows up as a missing composite several frames
later.

## Clipping

Three tiers, cheapest first:

| Clip | Cost |
|---|---|
| `ClipKind::Rect` | Scissor rectangle. No mask, no offscreen pass, no per-fragment work. |
| `ClipKind::Rounded` | Scissor plus an analytic SDF evaluation per fragment. |
| `ClipKind::Path` | Scissor plus a mask texture. |

A rounded clip whose radii are all zero degrades to `Rect` at record time, so the cheap path stays
alive. Instances under a rounded clip get a `CLIP_ROUNDED` flag; instances under a plain clip do
not, and pay nothing.

Nesting two rounded clips evaluates only the innermost analytically. The outer one is still enforced
by the intersected scissor, so the result is conservative rather than wrong. Nested rounded clips
are vanishingly rare in UI and are not worth a per-fragment loop.

`Scene::scissor_for` walks the clip chain intersecting bounds, with a depth guard so a malformed
chain degrades to an over-large scissor instead of hanging the render thread.

## Shaders

WGSL has no `#include`, and six shaders need the same rounded-box SDF and the same clip evaluation.
Rather than copy them, there is exactly one directive:

```wgsl
//!include common/math.wgsl
```

resolved at startup against `include_str!`-embedded sources. Each file is included at most once per
module in first-seen order, so output is byte-stable. It is deliberately not a preprocessor: no
conditionals, no macros, no expression evaluation.

| Shader | Purpose |
|---|---|
| `common/math.wgsl` | Rounded-box SDF, derivative-based coverage, `erf`, analytic blurred box |
| `common/frame.wgsl` | Group 0 bindings, transform/clip/gradient lookup, gradient ramp |
| `quad.wgsl` | Rectangles, rounded rectangles, borders, gradients, textures, shadows |
| `text.wgsl` | MTSDF and bitmap glyphs, optional outline |
| `mesh.wgsl` | Vertex-coloured and textured triangles |
| `composite.wgsl` | Layer composite with opacity and saturation |
| `blur.wgsl` | Separable Gaussian, two passes |

The flag constants in `quad.wgsl` and `text.wgsl` are hard-coded because WGSL cannot import them.
A test asserts each one still matches the Rust value, so renumbering a flag fails the build rather
than silently miscolouring a frame.

### Antialiasing

There are two mechanisms, because there are two kinds of edge.

**Analytic, for the shapes with an equation.** `coverage_from_distance` divides the signed distance
by `fwidth(d)`. A fixed threshold would break the moment a shape is scaled or zoomed; `fwidth`
measures how much the distance changes across one actual pixel, so the antialiasing band is always
exactly one pixel wide regardless of transform. Rectangles, rounded rectangles, borders, circles,
shadows and glyphs all take this path and are smooth at any size for free.

**Multisampling, for the shapes without one.** A tessellated path is a bag of triangles and has no
analytic edge at all — nothing in the fragment shader knows where the outline was. Those edges are
therefore *hard*, and an icon, a curve or a waveform outline is visibly jagged without help.

So the surface and every offscreen layer are multisampled, 4× by default
([`SurfaceConfig::msaa_samples`]). The count is chosen once at startup from what the surface format
*and* the layer format both support, and rounds down rather than failing — a machine that cannot do
4× still opens a window.

Three details worth stating:

- A pipeline is bound to one sample count, so the count is part of the pipeline cache key. The GPU
  test builds every pipeline at both 1× and 4× so a mismatch cannot ship.
- A multisampled texture cannot be sampled, which is why a layer allocates *two* textures: render
  into the multisampled one, resolve into the plain one, sample that.
- The multisampled attachment is transient. A pass that clears and does not need to be resumed
  stores `Discard`, so the 4× buffer is never written back to memory. A pass resumed after a nested
  layer keeps its samples, because a discarded attachment has nothing to reload.

`alpha_to_coverage` is deliberately **off**: it would quantise every translucent primitive to the
sample count, which is far worse than the analytic alpha the shaders already produce.

### Box shadows

Solved analytically rather than blurred. The x direction uses an `erf` approximation exactly; the y
direction integrates over six samples of the rounded box's half-width at each height. Six is enough
that the residual error is invisible at the opacities UI actually uses, and it costs a fraction of a
separable two-pass blur plus its render target.

## Colour

Three representations, kept apart on purpose:

| Type | Space | Alpha | Where |
|---|---|---|---|
| `Color` | sRGB-encoded | straight | Authoring |
| `LinearColor` | linear light | premultiplied | GPU |
| `Hsla` | — | straight | Theme authoring |

The surface is configured with an sRGB format wherever one is offered, so the hardware performs the
encode and blending happens in linear light. Everything handed to the GPU is linear and
premultiplied, which is what avoids dark fringes on filtered edges and muddy midpoints in gradients.

`Color::lerp` round-trips through linear space. A test asserts the black-to-white midpoint is
`0.735` and not `0.5`; if that ever reads `0.5`, something started interpolating sRGB directly.

Glyph atlas pages are stored `Rgba8Unorm`, **not** sRGB. A distance field is geometry, not colour,
and gamma-decoding it on sample would corrupt every glyph edge.

## GPU buffers

`GrowableBuffer` grows geometrically to a power of two and never shrinks during normal operation. It
tracks a high-water mark and a reallocation count; in steady state the reallocation count stops
increasing, and one that keeps climbing means a workload is still growing or oscillating around a
capacity boundary.

Storage buffers may not be empty in WGSL, and an empty frame is entirely normal for a UI, so every
table gets a dummy element rather than a special case at each bind site.

Pass uniforms are packed into one buffer at the device's minimum dynamic-offset stride, so one
allocation and one bind group serve every pass in the frame.

Offscreen layer targets come from a pool keyed on size and are released after sixty unused frames.
Freeing them the moment one frame skips them would reallocate every time a menu opened and closed.

## Surface failure is not an error

`SurfaceError` classifies every failure into exactly one recovery class, and a test asserts the
classes are disjoint and total:

| Class | Variants | Response |
|---|---|---|
| Transient | `Timeout`, `Occluded`, `ZeroSized` | Skip the frame |
| Reconfigure | `Outdated`, `Lost` | Reconfigure, retry |
| Fatal | `DeviceLost`, `OutOfMemory` | Rebuild everything |

Minimising a window reports `Occluded` and is transient. It must never tear down the device, and
there is a regression test named after exactly that.

## Verification

`crates/sphere-wgpu/tests/pipelines.rs` creates a headless adapter, runs every WGSL module through
naga validation, and builds every pipeline for both the swapchain and the layer format. A shader
that fails validation is otherwise invisible until a window opens and shows nothing.

On a machine with no adapter the suite skips rather than fails — a missing GPU is an environment
fact, not a defect in the code under test.
