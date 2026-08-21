# Roadmap

An honest account of what exists, what is partial, and what has not been started.

An inaccurate roadmap is worse than none, so the status below was checked against the source rather
than against intent.

## Where v0.1 stands

**50,636 lines** of Rust and WGSL. **1,005 unit tests + 11 doctests**, zero warnings, clippy clean,
`cargo fmt` clean.

| Crate | Tests | Status |
|---|---|---|
| `sphere-core` | 78 | Done |
| `sphere-render` | 77 | Done |
| `sphere-wgpu` | 31 (4 on a real GPU) | Done |
| `sphere-text` | 232 | Done |
| `sphere-layout` | 108 | Done |
| `sphere-image` | 95 | Done |
| `sphere-platform` | 116 | Done |
| `sphere-ui` | 147 | Done |
| `sphere-audio-ui` | 86 | Done |
| `sphere-svg` | 32 | Done |
| `sphere` (facade) | 3 | Done |

## Phases

### Phase 0 — Foundation · **DONE**

Cargo workspace, core geometry, colour, window, wgpu initialisation, surface, swapchain
configuration, frame lifecycle, clear colour, diagnostics.

*Success criterion — a native window opens and renders reliably.* Met, and verified by running it:
the demo opens on an NVIDIA GTX 1060 through Vulkan and presents 181 frames.

### Phase 1 — Primitive renderer · **DONE**

Rect, rounded rect, line, circle, gradient, clip rect, transform, instancing, batching.

*Success criterion — 10k+ primitives render smoothly.* Met. The demo draws 10,018 quad instances
per frame at roughly 1 ms of CPU.

Circles and rounded rectangles stay on the analytic quad pipeline rather than tessellating, and 500
compatible rectangles merge into a single draw call.

### Phase 2 — Text · **DONE**

Font loading, font database, fallback, shaping, glyph runs, MTSDF generation, paged atlas, GPU text
shader, HiDPI, multilingual text.

*Success criterion — text renders correctly at common DAW UI sizes.* Met. The MTSDF generator is a
pure-Rust implementation of Chlumsky's method including edge colouring, pseudo-distance and error
correction; the small-size bitmap fallback is an analytic-coverage rasteriser selected on physical
size. The demo renders Latin, Thai, Japanese, Chinese, Korean and Arabic through one path.

### Phase 3 — Layout · **DONE**

Node tree, stable ids, styles, taffy backend, layout cache, dirty flags, hit testing.

*Success criterion — complex nested panels resize correctly.* Met. No `taffy` type appears in the
public API. `LayoutStats::nodes_laid_out` makes the invalidation behaviour measurable.

### Phase 4 — UI events · **DONE**

Hover, click, drag, scroll, focus, keyboard, text input, IME architecture.

*Success criterion — basic interactive widgets function correctly.* Met. Capture/target/bubble
dispatch, pointer capture, a click tracker with both a time and a distance window, a focus registry
with scopes and traps, and an `InputTranslator` bridging platform events.

IME is *architecturally* complete — the event types, the pre-edit model and `set_ime_cursor_area`
all exist — but no text field consumes them yet, because there is no text field.

### Phase 5 — Widgets · **PARTIAL**

| Widget | Status |
|---|---|
| Button (4 variants) | Done |
| Toggle / switch | Done |
| Checkbox | Done |
| Slider | Done |
| Knob | Done |
| Fader | Done |
| Separator, progress, panel | Done |
| ScrollView | Done |
| Label | Done |
| **Text input** | **Not started** |
| **Menu, dropdown** | **Not started** |
| **Tabs, tooltip, modal, popover** | **Not started** |
| **List, tree, dock container** | **Not started** |
| **Radio, icon button, number input** | **Not started** |

The value controls are complete: vertical drag with pointer capture, Shift for fine mode,
double-click to reset, arrow/Page/Home/End keyboard support, scroll-wheel adjustment, step
quantisation relative to the minimum, and full semantic reporting including a caller-supplied value
string so a fader announces "−6.0 dB" rather than "79 %".

Text input is the significant gap. Everything it needs exists — IME events, the cluster mapping in
shaped runs, `TextLayout::hit_test`, focus, per-node scratch — but the widget itself is not written.

### Phase 6 — Realtime audio UI · **DONE**

VU meter, waveform, spectrum, EQ curve, realtime paint node, lock-free visual data transfer.

*Success criterion — changing realtime graphics do not invalidate unrelated layout.* Met, and
asserted: 60 frames of meter repaint with `nodes_laid_out == 0` on every one.

Delivered: `AtomicSnapshot` (triple buffered), `SpscRing`, `Seqlock`, meters with proper dB scaling
and ballistics, min/max waveform envelopes, log-frequency spectra, EQ and compressor curves,
oscilloscope, vectorscope, gain reduction, and a piano keyboard with correct black-key offsets.

Not built: spectrogram, automation-curve editing, ADSR envelope editor, XY pad, timeline ruler,
playhead. The `RealtimeFrame` primitives they would compose from all exist.

### SVG · **DONE** (not a numbered phase)

Content-addressed parsing, cached tessellation, `viewBox` fitting with aspect preservation, linear
and radial gradients, fills, strokes with caps/joins/dashes, nested transforms, flattened group
opacity, and one-argument theme tinting.

Unsupported features are **refused**, not silently dropped. That distinction is load-bearing: usvg
removes `<text>` from its tree entirely when no fonts are loaded, so a document is scanned before
parsing and `SvgError::Unsupported` is returned rather than handing back a silently empty icon.

### Phase 7 — Effects · **PARTIAL**

| Feature | Status |
|---|---|
| Offscreen layers | Done — allocated only when they change the result |
| Layer compositing with opacity | Done |
| Box shadow (analytic) | Done |
| Inner shadow | Done |
| Saturation filter | Done |
| Gaussian blur shader | Written and validated, **not yet wired to the layer pipeline** |
| Backdrop blur | Not started |
| Colour matrix | Type exists, shader path not written |
| Blend modes beyond fixed-function | Layers open correctly; the shader does not implement them |
| Glow, bloom, mask, reflection, glass | Not started |

`blur.wgsl` compiles and its pipeline builds — the GPU test covers it — but `SphereSurface` does not
yet run the two-pass blur between a layer's render and its composite.

## Definition of done for v0.1

| Criterion | Status |
|---|---|
| Windows, Linux, macOS architecturally supported | Yes — one `cfg`-gated backend module, no platform code above it |
| A WGPU window renders reliably | Yes — verified by running it |
| Core primitives GPU accelerated | Yes |
| Text shaping and MTSDF rendering work | Yes |
| Thai and Japanese text render correctly | Yes — in the demo and in tests |
| DPI scaling works | Yes — fractional scaling tested at 1.0/1.25/1.5/1.75/2.0 |
| A retained node tree exists | Yes |
| Flex layouts work | Yes |
| Dirty propagation works | Yes — and is measured, not asserted |
| Basic input and hit testing work | Yes |
| Basic widgets exist | Partial — see Phase 5 |
| A realtime meter updates without full relayout | Yes — tested |
| The public API is not tied to WGPU | Yes — `RendererBackend` is the only seam |
| No Skia/C++ rendering dependency remains | Yes — pure Rust throughout |
| Examples and documentation demonstrate the architecture | Partial — one example, eight documents |
| Workspace builds and tests cleanly | Yes |

## Known gaps, in the order they should be closed

1. **Text input.** The largest functional hole. Everything it depends on exists.
2. **Benchmarks.** No Criterion suite. The engine reports counters and the demo measures itself, but
   there is no regression harness.
3. **More examples.** One demo covers everything at once; small focused examples would teach the
   pieces better.
4. **Blur wiring.** The shader and pipeline exist and are GPU-validated; the pass is not run.
5. **Remaining widgets.** Menu, tabs, tooltip, modal, list, tree.
6. **Accessibility bridge.** Every element already reports role, value, state and actions. No
   platform bridge (UI Automation, AT-SPI, NSAccessibility) consumes them yet.
7. **Golden-image tests.** Visual regressions are currently caught by eye.

## Beyond v0.1

- Web and WASM through WebGPU. `wgpu` already supports the target; the platform layer does not.
- A native D3D12 backend, if measurement ever shows wgpu's abstraction costing something that
  matters.
- Compute-shader blur, for the cases where the separable fragment path is not enough.
- HDR and wide-gamut output. `SurfaceColorSpace` is plumbed through; nothing uses it yet.
- Virtualised lists for very large sessions.
- A debug inspector showing the node tree, computed bounds, dirty flags, batches and atlas pages.
