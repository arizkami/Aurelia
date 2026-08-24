# Roadmap

An honest account of what exists, what is partial, and what has not been started.

An inaccurate roadmap is worse than none, so the status below was checked against the source rather
than against intent.

## Where v0.1 stands

**81,043 lines** of Rust and WGSL, plus 2,538 of TypeScript and 619 of C++ (the V8 shim).
**1,571 Rust tests** — 1,556 unit and integration, 15 doc — and **53 TypeScript tests**. Zero
warnings, clippy clean, `cargo fmt` clean.

Counted from `cargo test --workspace` and `bun test` on Windows. On Linux and macOS the total is 26
lower: `spherekit-jsengine`'s 19 tests and `spherekit-bridge`'s 7 V8 tests need the Windows prebuilt.

| Crate | Tests | Status |
|---|---|---|
| `spherekit-core` | 117 | Done |
| `spherekit-render` | 108 | Done |
| `spherekit-wgpu` | 42 (4 on a real GPU) | Done |
| `spherekit-text` | 284 | Done |
| `spherekit-layout` | 108 | Done |
| `spherekit-image` | 95 | Done |
| `spherekit-platform` | 148 | Done |
| `spherekit-ui` | 231 | Done |
| `spherekit-audio-ui` | 86 | Done |
| `spherekit-svg` | 32 | Done |
| `spherekit-css` | 161 | Partial — see Phase 8 |
| `spherekit-react` | 71 Rust + 53 TypeScript | Partial — see Phase 9 |
| `spherekit-bridge` | 24, and 31 with `v8` | Done — see Phase 10 |
| `spherekit-jsengine` | 19 (Windows only) | Partial — see Phase 11 |
| `spherekit-cli` | 6 | Partial |
| `spherekit` (facade) | 6 | Done |

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

*Success criterion — text renders correctly at common DAW UI sizes.* Met. Glyph quads are snapped
to the device pixel grid — the baseline always, and a bitmap glyph's origin and extent as well, so
the atlas's edge-aligned UV convention actually holds — and coverage carries a perceptual gamma so
light-on-dark text does not bloom in linear light. The MTSDF generator is a
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

IME is complete end to end as of the text field: platform event, translated event, buffer,
composition, caret rectangle and the two `Window` calls that make a candidate window appear in the
right place. See `docs/platform.md`.

Touch is in as of the contact pipeline: raw contacts from the platform, gesture recognition in the
translator (tap, long press, swipe, pinch, with replaceable thresholds), pointer emulation so every
existing widget works under a finger, and drag-to-scroll with flings in the tree. Overscroll
rubber-banding, two-finger pan and stylus reporting are not done. See `docs/touch.md`.

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
| Text field, with IME | Done |
| Custom window chrome + system menu | Done (Windows) |
| Menu, dropdown, context menu | Done |
| Colour picker (square, ramps, swatches) | Done |
| Calendar and `Date` | Done |
| Segmented control, radio, badge, spinner, stepper | Done |
| Tooltip | Done — positioning and animation; the hover delay is the application's |
| Overlay, popover, toast | Done — the timing and the queue are the application's |
| Outline button variant | Done |
| **List, tree, dock container** | **Not started** |
| Virtual keyboard (letters, symbols, numeric) | Done |
| **Icon button, editable number input** | **Not started** |

The value controls are complete: vertical drag with pointer capture, Shift for fine mode,
double-click to reset, arrow/Page/Home/End keyboard support, scroll-wheel adjustment, step
quantisation relative to the minimum, and full semantic reporting including a caller-supplied value
string so a fader announces "−6.0 dB" rather than "79 %".

`TextField` is single-line and covers selection by click, drag, double-click and triple-click,
grapheme-correct arrow and word motion, Home/End, backspace and delete, select-all, submit-on-Enter,
masking for passwords, and input-method composition with an underlined pre-edit. `TextEdit` under it
holds no pixels and is tested without a font.

Still missing: a multi-line editor, which needs vertical caret motion and therefore the laid-out
lines; clipboard integration, which needs a platform clipboard call the field does not yet make; and
undo.

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
| Multisample antialiasing for paths | Done — 4× by default, on the surface and every layer |
| Gaussian blur | Done — separable two-pass layer filter |
| Backdrop blur | Done — destination snapshot plus separable two-pass filter |
| Colour matrix | Type exists, shader path not written |
| Blend modes beyond fixed-function | Layers open correctly; the shader does not implement them |
| Glow, bloom, mask, reflection, glass | Not started |

`SphereKitSurface` runs the two-pass blur between a filtered layer's render and its composite.
`BackdropBlur` snapshots the destination behind the layer first, then composites the original
layer over that blurred backdrop. If the swapchain cannot be copied, the tint still renders and
the backdrop sample is skipped gracefully.

### Phase 8 — Stylesheet runtime · **PARTIAL**

`spherekit-css` parses a CSS subset, applies selector specificity and `!important`, and produces a
`ResolvedStyle` that a native `Styled` element and a React node both take. One cascade, two
producers.

*Success criterion — a native `div()` and a React `<View>` with the same class resolve to the same
paint style.* Met, and asserted in both crates.

| Feature | Status |
|---|---|
| Type, class, id and universal selectors | Done |
| Descendant, child, `+` and `~` combinators | Done |
| `:hover`, `:active`, `:focus`, `:disabled`, `:checked`, `:root`, `:first-child`, `:last-child`, `:nth-child()`, `:not()` | Done |
| Specificity, source order, `!important` | Done |
| `@media` — `min/max-width`, `min/max-height`, `prefers-color-scheme`, `and`, comma lists | Done |
| Custom properties and `var()` with fallbacks | Done |
| `px`, `%`, `em`, `rem`, `vw`, `vh`, `vmin`, `vmax`, unitless zero | Done |
| Flex box model: display, position, inset, size, margin, padding, border, gap, alignment | Done |
| Paint: `background`, `border-color`, `border-radius`, `box-shadow`, `opacity`, `cursor`, `visibility` | Done |
| Typography: `color`, `font-family`, `font-size`, `font-weight`, `font-style`, `line-height`, `letter-spacing`, `text-align`, `text-overflow`, with inheritance | Done |
| **Gradients** — `background` takes a colour, not a `linear-gradient()` | **Not started** |
| **`transition`, `animation`, `@keyframes`** | **Not started** — blocked on the retained animation layer in *Known gaps* below |
| **`@media not`, and every media type but `screen`/`all`** | **Not started** — such a query is false, which is the conservative reading |
| **`transform`** | **Not started** |
| **Grid template properties** | **Not started** — Taffy supports grid, the property model does not expose it |
| **Attribute selectors, pseudo-elements** | **Not started** — dropped from a selector list rather than matched loosely |
| **`@font-face`, `@supports`, `@import`** | **Not started** — skipped as unknown at-rules |

The rule that governs all of it is that syntax the engine does not model is *ignored, never
reinterpreted*: `width: 12` does not become `12px`, an unknown media feature makes its query false
rather than true, and a selector with an attribute test is dropped from its comma list. A stylesheet
that does nothing is debuggable; one that does something slightly different from what it says is not.
Rules inside a currently-unmatched `@media` block are retained rather than dropped, so a stylesheet's
meaning does not depend on the window size at the moment it was installed.

See [`docs/spherekit-css.md`](spherekit-css.md).

### Phase 9 — React frontend · **PARTIAL**

React 19 reconciles against a native host tree. After each commit the whole tree crosses to Rust as
one serialisable snapshot, is validated and retained, and is lowered to `spherekit-ui` elements
through the same CSS cascade a native element uses. Events come back by node id.

*Success criterion — an ordinary React application with hooks and JSX drives native widgets with no
DOM in the process.* Met, and running: `app/reactdemo` is React 19 in SphereKit's own V8 isolate,
with no browser, no WebView and no Node.

| Feature | Status |
|---|---|
| `react-reconciler` host config, React 19 | Done |
| Whole-tree snapshot at commit, with revision ordering | Done |
| Commit validation: duplicate ids, missing types, depth and node-count limits | Done |
| Lowering with cascade, ancestor chain and typography inheritance in one walk | Done |
| 15 host component types | Done |
| `register_host_type` plus the `<Native type=… />` escape hatch | Done |
| Events: `press`, `valueChange`, `change`, `select`, `submit` | Done |
| `useInvoke`, `useNativeEvent`, `useStylesheet` | Done |
| `stylesheet()`, `toCssText()`, `cx()` authoring helpers | Done |
| **`ScrollView`'s `onScroll`** | **Not offered** — nothing lowers a scroll callback, and `spherekit-ui`'s `ScrollView` does not implement `Interactive`, so there is no offset to report; the prop is left out rather than declared and inert |
| **Focus, keyboard and IME props** | **Not started** — the native widgets have all three; no React prop reaches them |
| **Portals** | **Not started** — `preparePortalMount` is a no-op, so a context menu cannot escape its parent's box from React |
| **Suspense and transitions** | Partial — `hidden` is carried through commits; nothing suspends on a native resource |
| **Dropdown, context menu, panel chrome** | **Not started** as host types, though the native widgets exist |

The snapshot is not a performance decision and is not an interim one. It is what keeps the native
side from ever observing a half-built tree, what lets each half be tested with none of the other in
the process, and what makes the boundary transport-agnostic. The cost — re-serialising every node on
every commit — was accepted. See [`docs/react.md`](react.md).

### Phase 10 — API bridge · **DONE**

A newline-delimited JSON protocol between a JavaScript runtime and the native host, with no opinion
about the transport underneath it.

*Success criterion — the same protocol works over an in-process call and over a byte stream.* Met:
`JsBridge` drives it through a synchronous V8 host function, and `JsonLines` reassembles it from
arbitrary chunks. Every split point of a sample stream, down to one byte at a time, decodes to the
same messages — that is a test, not a claim.

Delivered: the `hello`/`ready` handshake with version refusal, `commit`, `invoke`/`response`,
`event`, `shutdown`; eight built-in `spherekit.*` methods; application methods through
`register_method`; the outbound event queue and `pump_events`; the style context for viewport,
root font size and colour scheme; a frame-size cap so a renderer that dies mid-frame cannot grow the
buffer until the process does.

Not built: no batching of consecutive commits, so a burst of React commits crosses as a burst of
frames; no back-pressure signal, because no transport in use has needed one; no binary encoding, and
none is planned until a profile asks for it.

See [`docs/api-bridge.md`](api-bridge.md).

### Phase 11 — JavaScript runtime · **PARTIAL**

`spherekit-jsengine` embeds V8 through a hand-written C++ shim. The public Rust surface owns an
`Engine` and never lets a `v8::Local<T>` out, because isolates are thread-affine and local handles
are stack-scoped — the shim opens its scopes, does the work, and returns owned UTF-8.

*Success criterion — React's production bundle evaluates and renders in a bare isolate.* Met on
Windows.

| Feature | Status |
|---|---|
| `eval`, `eval_named`, `bind`, `call_global`, `has_global` | Done |
| Explicit microtask policy; `run_microtasks` and a bounded `pump` | Done |
| Exceptions with message, stack, line, column and script name as separate fields | Done |
| Panic containment at the FFI boundary | Done |
| `runtime/prelude.js`: timers, frame callbacks, `console`, `performance`, `navigator` | Done |
| `JsBridge`: isolate wired to an `ApiBridge` through two globals | Done |
| **Targets other than Windows x86_64** | **Not started** — the shim is portable C++20; the *prebuilt* is not. The non-Windows `Engine` is a complete API mirror that returns `UnsupportedPlatform` |
| **ES modules** | **Not started** — the application must arrive as one IIFE bundle |
| **`fetch`, `URL`, `TextEncoder`, `structuredClone`** | **Not started** — the prelude supplies only what React's module evaluation reaches for |
| **A debugger or inspector protocol** | **Not started** — `js_protocol.pdl` ships with the prebuilt; nothing serves it |
| **Snapshots or code cache** | **Not started** — the bundle is parsed from source on every start |

The `v8` feature is off by default everywhere, and `spherekit-bridge` compiles with no JavaScript
engine at all, because the protocol is the product. An application wanting React on another platform
today drives the same bridge from a WebView or a child process.

See [`docs/javascript.md`](javascript.md).

### CLI and templates · **PARTIAL** (not a numbered phase)

`spherekit react <name>` scaffolds a React + Rust application from `template/spherekit-app-react`
and `spherekit build` runs the TypeScript and Cargo halves together, with `--dry-run`, `--no-react`,
`--no-rust`, `--out-dir` and cross-compilation targets for CI. It detects Bun, npm, pnpm or Yarn.

Not built: no `spherekit dev` with a watch loop, no packaging or installer step, and one template.

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
| No Skia/C++ rendering dependency remains | Yes — pure Rust throughout. The only C++ in the workspace is the V8 shim, which draws nothing |
| A stylesheet styles native and React elements alike | Yes — one `Stylesheet`, two producers, asserted in both crates |
| React renders native widgets with no DOM | Yes — `app/reactdemo`, React 19 in an in-process isolate |
| Examples and documentation demonstrate the architecture | Yes — five runnable applications, three diagnostic examples, twelve documents |
| Workspace builds and tests cleanly | Yes — and without a JavaScript toolchain, which is why `reactdemo` falls back to a bundler-free renderer |

## Known gaps, in the order they should be closed

1. **Clipboard, undo, and multi-line editing.** The text field handles selection, motion and
   composition; cut/copy/paste, an undo stack and vertical caret motion are not written. Vertical
   motion is the one that needs new machinery, because it has to walk the laid-out lines.
2. **Animation core integration.** `spherekit-core::animate` is built and tested — analytic springs,
   tweens as the degenerate case, `Animatable` for the scalar, geometry and colour types — and
   `spherekit-core::time` gives it one clock. What is *not* built is the retained layer that would let a
   stock widget animate without the application holding the `Motion`. The `desktop_app` caption shows
   the app-owned pattern working end to end; `Styled::transition` does not exist yet.
3. **A native Win32 backend.** Deliberately deferred, with the reasoning written down in
   `docs/platform.md`. The custom chrome it was supposed to enable did not need it.
4. **Benchmarks.** No Criterion suite. The engine reports counters and the demo measures itself, but
   there is no regression harness.
5. **More examples.** Two exist — a desktop settings window and a plug-in editor. Small focused
   examples for individual subsystems would still teach the pieces better.
6. **Remaining widgets.** Menu, tabs, tooltip, modal, list, tree.
7. **Accessibility bridge.** Every element already reports role, value, state and actions. No
   platform bridge (UI Automation, AT-SPI, NSAccessibility) consumes them yet.
8. **Golden-image tests.** Visual regressions are currently caught by eye.
9. **V8 on Linux and macOS.** The shim is portable C++20 against the public V8 API; only the
   prebuilt is Windows-only. Until a monolith exists for the other two, React on them means a
   WebView or a child process driving the same bridge. What the port involves is written down in
   `docs/javascript.md`.
10. **Scroll, focus and keyboard events in React.** No React prop reaches the focus, keyboard or IME
    machinery the native widgets already have, and `ScrollView` reports no offset to lower into an
    `onScroll`. The gap is a lowering and a `spherekit-ui` builder, not a design question.
11. **CSS transitions.** The property model has no `transition`, which is the CSS-facing half of
    gap 2 and blocked on the same retained animation layer.

## Beyond v0.1

- Web and WASM through WebGPU. `wgpu` already supports the target; the platform layer does not.
- A native D3D12 backend, if measurement ever shows wgpu's abstraction costing something that
  matters.
- Compute-shader blur, for the cases where the separable fragment path is not enough.
- HDR and wide-gamut output. `SurfaceColorSpace` is plumbed through; nothing uses it yet.
- Virtualised lists for very large sessions.
- A debug inspector showing the node tree, computed bounds, dirty flags, batches and atlas pages.
- ES modules and a code cache in the isolate, so a large bundle is not reparsed from source on every
  start.
- A V8 inspector endpoint, so a React application can be debugged with the tools its authors expect
  rather than through `console.log` and `drain_console`.
