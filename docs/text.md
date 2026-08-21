# Text

Why glyphs are distance fields, and what that costs.

## The pipeline

```text
text + style
   ↓  bidi resolution                    unicode-bidi
   ↓  script itemisation                 unicode-script
   ↓  font resolution and fallback       fontdb
   ↓  shaping, per run                   rustybuzz
   ↓  line breaking                      unicode-linebreak
   ↓  alignment and justification
   ↓  glyph rasterisation                MTSDF, or bitmap below ~12 px
   ↓  paged atlas placement
   ↓  instanced draw                     sphere-wgpu
```

Nothing above the rasteriser is Sphere-specific: bidi, itemisation and shaping are solved problems
with correct Rust implementations, and reimplementing them would be a large amount of work whose
best possible outcome is parity. What Sphere owns is everything from rasterisation down, because
that is where the design decision lives.

## Why MTSDF and not a bitmap cache

The conventional approach stores one rasterised bitmap per `(glyph, size)` pair. For a fixed-size
interface that is fine. For the interfaces this engine targets it is not:

- A DAW mixes 10 px labels with 24 px headings on the same screen.
- Panels zoom, and a zoom step is a new size.
- Displays run at 100 %, 125 %, 150 %, 175 % and 200 %, and each is a different *physical* size.

The cache multiplies out across all three axes, and every zoom step re-rasterises from outlines.

A **signed distance field** stores, per texel, the distance to the nearest outline. It is sampled
at any size, so one field serves every size, every zoom level and every scale factor. The atlas
footprint stops depending on the type scale entirely.

Single-channel SDF has a known failure: it rounds off sharp corners, because a corner is where two
edges are equidistant and one distance value cannot represent both. **Multi-channel** SDF stores
three distances in RGB, assigned so that adjacent edges across a corner share exactly one channel;
the median of the three reconstructs the true distance *and* preserves the corner.

The **T** in MTSDF is a fourth channel holding the true (single-channel) distance. It buys two
things: outlines, glows and shadows at a uniform width — which the median cannot give, because it
is exact at the edge but not at a fixed offset from it — and a ground truth for error correction.

```wgsl
let sd = median3(sample.r, sample.g, sample.b) - 0.5;
let alpha = clamp(sd * screen_range + 0.5, 0.0, 1.0);
```

## The generator

`crates/sphere-text/src/mtsdf.rs` is a pure-Rust implementation of Chlumsky's method. The
`msdfgen` crate is a C++ binding and is therefore not an option here.

**Outline extraction.** `ttf-parser` yields contours as line, quadratic and cubic segments.
Coordinates are normalised into em units by dividing by `units_per_em`, and TrueType's y-up
convention is flipped to Sphere's y-down exactly once, in the outline collector.

**Edge colouring.** Each contour is walked and a vertex is classified as a corner when the angle
between the incoming and outgoing tangents exceeds ~3°. Edges are assigned a colour mask from
{yellow, magenta, cyan} such that adjacent edges across a corner share exactly one channel. Three
cases need special handling and all three are implemented: a fully smooth contour (an `o`), a
teardrop with one corner (a comma), and a two-corner contour — the last two require splitting an
edge into thirds, which is what stops a comma's terminal from smearing.

**Distance.** Exact for lines. For quadratics, the true cubic `dC/dt = 0` is solved with a robust
solver (trigonometric branch for three real roots, Cardano otherwise, `f64` internally because
`r² − q³` loses most of its precision near a double root). For cubics, iterative refinement from
several seeds. The RGB channels use *pseudo*-distance, which extends the endpoint tangents to
infinity — that is what prevents the classic MSDF corner-clipping artifact.

**Error correction.** Because MTSDF carries the true distance, a texel whose median falls on the
opposite side of the threshold from alpha is provably an artifact and is clamped. Skipping this
leaves visible notches on diagonal stems.

**Orientation.** Rather than assuming a winding convention, orientation is re-derived per glyph
from the total signed area, and each texel cross-checks the edge-derived sign against a nonzero
winding scanline test. That makes glyf (clockwise) and CFF (counter-clockwise) outlines reach the
same code correctly.

### Resolution and cost

```rust
pub struct GlyphRasterConfig {
    pub em_size_px: f32,   // 48 by default; 32 is fine for UI, 64 for display sizes
    pub range_px: f32,     // 4 — the smoothing budget, and the widest outline possible
    pub angle_threshold: f32,
    pub error_correction: bool,
    pub max_texels: u32,   // 512 — a malformed font can claim a huge outline
}
```

`range_px` is paid for twice: as padding around every glyph in the atlas, and as a ceiling on how
wide a synthetic outline can be. `max_texels` exists because a malformed font can report an outline
spanning thousands of ems, and without a cap that becomes a multi-gigabyte allocation driven by
file contents.

## The small-text exception

Below roughly 12 **device** pixels, a distance field runs out of resolution before a glyph runs out
of detail, and stems start to shimmer. DAW interfaces live at 10, 11, 12 and 13 px, so this is not
an edge case.

```rust
pub enum TextRasterMode { Auto, Mtsdf, Bitmap }
```

`Auto` chooses per glyph on the *physical* size — `font_size × scale_factor` — not the logical one.
That distinction matters: 10 logical px on a 2× display is 20 device px and belongs on the field
path. There is a test asserting exactly that, because choosing on the logical size alone would
wrongly send crisp HiDPI text to the fallback.

The fallback is an analytic-coverage scanline rasteriser: signed area accumulated per cell, then
prefix-summed along each scanline. Exact for polygons, no supersampling. It is deliberately
isolated in `raster.rs` and its output goes to separate atlas pages — it exists to make 11 px labels
crisp, not to become the main path.

## The atlas

**Paged, not one giant texture.** Pages default to 2048², clamped at startup to the adapter's
maximum. A single unbounded texture would fail late and badly on a device that cannot hold it.

**Separate page sets per format.** An RGBA8 field page and an R8 coverage page cannot share
storage.

**Shelf packing**, implemented in `atlas.rs` rather than pulled from a crate, because it is core
functionality and the eviction policy has to be integrated with it. Glyphs are placed into the
first shelf whose height fits within a slack threshold, else a new shelf opens.

**One texel of padding** between glyphs. Without it, bilinear sampling bleeds a neighbour into a
glyph's edge — a real, visible defect, and there is a test for it.

**Dirty-region upload.** A page is 16 MB; uploading it every frame would dominate the frame budget.
`take_dirty_regions` returns only what changed, borrowing the atlas's own mirror so the upload costs
no copy. In steady state, once every glyph on screen has been rasterised, it returns nothing — the
demo reports `glyph texels up: 0`.

**Graceful failure.** A glyph larger than a page, or a full atlas at the page limit, returns
`FontError::AtlasFull`. It never panics. One missing glyph is recoverable; a blank window is not.

## Fallback

A codepoint the primary face lacks walks a chain built from its script: Latin, Thai, Japanese,
Chinese, Korean, Arabic, Hebrew, Cyrillic, Devanagari, plus an emoji slot. Platform-specific family
lists are behind `cfg(target_os)` — Segoe UI, Leelawadee UI, Yu Gothic UI, Microsoft YaHei, Malgun
Gothic on Windows; Noto and DejaVu on Linux; Hiragino, PingFang and Apple Color Emoji on macOS —
but none of them is required. When no named family matches, the database is scanned for any face
containing the codepoint.

Neither DirectWrite nor CoreText is mandatory anywhere in this stack.

## Shaping and clusters

Script itemisation treats `Common` and `Inherited` as continuations of the surrounding run rather
than starting a new one; otherwise every space and comma splits a run and shaping quality collapses.

Every shaped glyph carries a `cluster: Range<usize>` mapping back to **original string** byte
indices, not run-local ones. Without that, a click can be turned into a pixel offset but not into a
text index, and caret placement and selection are impossible. Getting it wrong is easy and the
symptom is subtle, so the cluster ranges are tested for monotonicity and full coverage.

Letter spacing is applied after shaping and only at grapheme cluster boundaries, so it is never
inserted inside a ligature or a combining sequence.

## Caching

| Cache | Key | Prevents |
|---|---|---|
| Shaping | `(text, style, max_width)` | Reshaping a paragraph every frame |
| Glyph raster | `GlyphKey` | Rasterising a glyph twice |

Both are bounded by bytes with LRU eviction and hit/miss counters. The shaping cache stores the
source text alongside the fingerprint so a hash collision cannot silently return the wrong layout;
collisions are counted and expected to stay at zero.

`GlyphKey` carries a `variation_hash` for variable fonts and a `bitmap_size` that is **zero for
distance fields**. That zero is the whole point: keying a field by size would defeat MTSDF and
multiply the atlas by the number of sizes in the interface. A test asserts that requesting the same
glyph at 48 px and 96 px yields the same atlas placement.

## The shader

Smoothing is derived from the glyph's actual on-screen size, never from a constant:

```wgsl
// px_range arrives as the field's range in destination pixels at the nominal
// size; the vertex stage multiplies it by the transform's scale.
let screen_range = max(inst.params.x * transform_scale(inst.indices.z), 1.0);
let alpha = clamp(sd * screen_range + 0.5, 0.0, 1.0);
```

A fixed threshold produces text that is crisp at one size and either blurry or aliased at every
other, and breaks entirely when a panel is zoomed. Folding the transform's scale in at the vertex
stage is what keeps zoomed text as sharp as unzoomed text with no re-rasterisation.

Outlines read the alpha channel rather than the median, because the median is exact at the edge but
not at a fixed offset from it — a median-derived outline varies in width around a corner.

Atlas pages are stored `Rgba8Unorm`, **not** sRGB. A distance field is geometry, not colour;
gamma-decoding it on sample would corrupt every glyph edge.
