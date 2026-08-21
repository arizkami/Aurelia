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

Below [`BITMAP_MAX_DEVICE_PX`] — **24 device pixels** — a distance field is the wrong tool, and the
reason is sampling, not resolution.

A field is generated at 48 texels per em and drawn at the device size, so rendering at *d* device
pixels minifies it by `48 / d`. A bilinear tap averages a 2x2 texel footprint. Once the true
footprint is wider than that, most of the texels that should have contributed are never read, and
what comes back is a low-pass blur of the outline rather than a reconstruction of it. `48 / 24 = 2`,
so 24 is exactly where the footprint stops fitting.

The second reason lands in the same place. The shader's antialiasing ramp is one device pixel wide
by construction — `screen_range` is clamped to 1 — and a regular-weight stem is about `em / 9`:

| device px | field minified | stem | solid core between the two ramps |
|---|---|---|---|
| 13 | 3.7x | 1.44 px | **0.44 px** |
| 16 | 3.0x | 1.78 px | 0.78 px |
| 24 | 2.0x | 2.67 px | 1.67 px |
| 26 | 1.9x | 2.89 px | 1.89 px |

At 13 device pixels — a 13 px label on a 100 % display, which is most interface text ever written —
the two ramps very nearly meet and the stem never reaches full opacity. That *is* soft text, and no
amount of gamma correction fixes it, because the coverage genuinely is grey. At 26, the same label on
a 200 % display, there is nearly two pixels of solid core and it looks the way it should.

```bash
SPHERE_PROBE_SIZE=13 SPHERE_PROBE_ZOOM=4 cargo run -p sphere-text --example glyph_quad_probe --release -- out.png
```

writes the comparison: forced MTSDF on the left, what `Auto` picks on the right, magnified with
nearest-neighbour *after* compositing so the pixels shown are the pixels rendered.

An earlier threshold of 12 came from a different and incorrect premise — that the field runs short of
*texels*. It does not; a 48-per-em field has plenty at any interface size. The problem is reading
them.

```rust
pub enum TextRasterMode { Auto, Mtsdf, Bitmap }
```

`Auto` chooses on the *physical* size — `font_size x scale_factor` — not the logical one. That
distinction is the whole point: 13 logical px at 2x is 26 device px and belongs on the field path,
while the same 13 px at 1x does not. There is a test asserting exactly that pair.

The fallback is an analytic-coverage scanline rasteriser: signed area accumulated per cell, then
prefix-summed along each scanline. Exact for polygons, no supersampling. Its output goes to separate
atlas pages, and the batch compiler snaps its quads to the pixel grid in both axes and to the exact
texel count — that 1:1 alignment is most of why it is sharp.

**What it costs.** Bitmaps are keyed per device size, so an interface with five type sizes at one
scale factor holds five sets of glyphs rather than one size-independent set. That is the trade the
fallback exists to make, and it is why the threshold is a threshold rather than "always bitmap".
Content that zooms continuously should ask for `TextRasterMode::Mtsdf` explicitly.

## Vertical grid-fitting

Snapping a glyph's quad to the pixel grid puts its *baseline* on a whole pixel and does nothing for
the other horizontal edges — and those are most of what the eye reads: the x-height line across the
top of `n`, `o` and `x`, the cap line across `H`, the crossbar of `e`. At 13 device pixels an
x-height of 0.5 em is 6.5 pixels, so the top of every lowercase letter is two rows of grey instead of
one row of black.

`hint.rs` builds a monotonic, piecewise-linear map from unfitted em `y` to fitted em `y`. Each of the
five zones — ascender, cap height, x-height, baseline, descender — is moved to the nearest whole
pixel, and the outline between them is stretched linearly to follow.

Four properties, each with a test:

- **The baseline is a fixed point.** Zero pixels rounds to zero, so a fitted glyph sits on exactly
  the baseline layout computed. A fit that moved it would shift every line against the boxes
  around it.
- **The map is monotonic.** A non-monotonic map puts a lower point above a higher one and turns the
  glyph inside out, which is worse than no fitting at all. A face whose reported metrics are
  nonsense — zeroed, inverted, absurdly scaled — is refused rather than fitted.
- **Nothing moves more than half a pixel.** Fitting is a nudge. A larger displacement would mean a
  zone snapped to the wrong grid line and the glyph changed proportion visibly.
- **Points beyond the zones extrapolate rather than clamp**, so a tall accent or a deep tail moves
  with the part of the glyph it is attached to.

The map is applied to the **flattened polylines**, not to the curve control points. Pushing control
points through a piecewise-linear map bends the curve between them by an amount nothing accounts for;
moving the flattened vertices is exact for the geometry actually rasterised.

```bash
SPHERE_PROBE_COMPARE=fit SPHERE_PROBE_SIZE=13 SPHERE_PROBE_ZOOM=6 SPHERE_PROBE_TEXT=nxoeHm   cargo run -p sphere-text --example glyph_quad_probe --release -- out.png
```

writes unfitted against fitted, rasterised directly rather than through the atlas — the atlas has no
way to hand back an unfitted glyph, because fitting is not optional there.

### Only vertical, and only on the bitmap path

Horizontal grid-fitting means moving stems onto pixel boundaries, which changes the width of every
glyph and therefore its advance. Doing it properly needs the font's own hinting bytecode or an
autohinter that understands stem detection; doing it improperly quantises letter spacing into visible
clumps. Vertical fitting changes no advance at all, because it only ever moves `y`.

A distance field is size-independent by construction, so there is no single size to fit it to.
Fitting is a property of the bitmap path, which rasterises at one exact device size.

### Overshoot suppression

Round letters — `o`, `e`, `c`, `s`, `O`, `C`, `G`, `S` — are drawn slightly taller than flat ones so
they do not read as smaller. Measured across ten faces installed here, that overshoot is **0.0088 em
(Consolas) to 0.0166 em (Georgia)**, which is 0.11 to 0.22 device pixels at 13 px. The x-height line
snaps onto a whole pixel and the apex of `o` lands a fifth of a pixel above it — one extra grey row
that `x` does not have.

The mechanism is one line: **`shoot.fit = ref.fit`**. The round extremum is not rounded on its own,
it is *given* the position the flat one rounded to. In a piecewise-linear map that is a segment whose
two control points share a target — a flat slab. Five control points become eight:

```text
  (ascender,                    snap(ascender))
  (cap_height - cap_overshoot,  snap(cap_height))   <- slab
  (cap_height,                  snap(cap_height))
  (x_height   - x_overshoot,    snap(x_height))     <- slab
  (x_height,                    snap(x_height))
  (0.0,                         0.0)
  (0.0        + base_overshoot, 0.0)                <- slab
  (descender,                   snap(descender))
```

Rounding the shoot independently is what goes wrong: two positions a fifth of a pixel apart can round
onto different rows, or onto the same row from opposite sides, and neither is a suppression.

**The furthest, never the median.** FreeType can take a median because its capture radius is far
wider than the probe set's spread; a slab exactly as wide as the overshoot has no such slack and must
span the whole set. Measured here, a median met the objective in **30 of 100** face/size combinations
against **100 of 100** for the furthest — Tahoma's round tops spread 0.0024 em and a median slab
leaves its `o` outside, nine rows at 13 px against `x`'s seven.

**Capitals belong in the baseline set.** Arial's `O` bottoms 0.0005 em lower than its `o`, Georgia's
0.0020, Trebuchet's 0.0024. A lowercase-only probe set leaves `O` one row taller than `H` on all
three.

### The invariant that actually matters is bounded slope, not monotonicity

The obvious implementation is a point-wise capture band: `if |y - line| < band { fitted } else
{ map(y) }`. That is a non-decreasing step function of y, so it **passes a dense monotonicity sweep
with a green light** while tearing two adjacent flattened vertices a quarter of a pixel apart at the
band edge. `Rasterizer::coverage` takes `acc.abs()`, so a folded contour does not fail loudly — it
quietly produces a wrong row.

`GridFit::max_slope` is what separates the two. A slab has zero slope; a tear has infinite slope.
Measured worst over ten faces at ten sizes: **1.608**, against 1.472 for the five-knot map on the same
face and size.

A constant band cannot be tuned to work, either: it must be at least 0.0166 em to cover Georgia's cap
overshoot, and at 0.0166 it exceeds Tahoma's i-dot headroom of 0.0049 em. The ranges overlap across
faces, so no single constant separates overshoots from features. That is the measured reason the
per-face probe is not a nicety.

### The cost, and what it buys

Reading a face's zones is seventeen glyph lookups plus two `OS/2` reads — **5 to 12 µs** measured
here, once per face, memoised in `FontDatabase::vertical_zones` beside the coverage and fallback
memos. It survives cache invalidation for the same reason coverage does: it is a property of bytes
already loaded, and those cannot change.

What it buys, over ten faces at ten sizes: `o`/`e`/`s`/`c` land on exactly `x`'s height and
`O`/`G`/`S` on exactly `H`'s in **100 of 100** combinations, against 10 of 100 without it.

### The known cost: a slab can catch a feature that is not an overshoot

A slab is as wide as the face's overshoot, and on some faces a real feature sits inside that band. The
cap slab is the one that bites, because lowercase ascenders reach just past the cap line:

| face | cap slab | nearest feature above the cap line | headroom | |
|---|---|---|---|---|
| Arial | 0.0122 | `f` at −0.7280 | 0.0117 | inside |
| Tahoma | 0.0151 | `j` at −0.7319 | 0.0049 | inside |
| Verdana | 0.0151 | `j` at −0.7319 | 0.0049 | inside |
| Trebuchet MS | 0.0127 | `j` at −0.7207 | 0.0054 | inside |
| Consolas | 0.0088 | `t` at −0.6470 | 0.0088 | at the edge |

Five of ten faces pull the dot of `j`, or the hook of `f`, or the ascender of `t`, onto the cap line
along with the round capitals. The displacement is bounded by the slab — at most 0.22 device pixels
at 13 px — and it is not fixable by narrowing the band, because the same measurement that shows the
collision shows the overshoots and the headrooms overlapping across faces.

### What is still missing

**Stem darkening.** DirectWrite thickens stems slightly at small sizes to compensate for the eye
reading thin dark strokes as lighter than they are. Not implemented.

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

## The default family

A [`TextStyle`] with no family named resolves through `fontdb`'s generic sans-serif, whose default is
**Arial**. That is the Windows 95 interface font: the shell has not used it in decades, and on a
non-Latin system it has no coverage for the language the user actually reads in, so every unstyled
string fell through to a fallback chosen by accident.

`sphere-text::system_ui` fixes it by asking the platform. On Windows that means reading
`NONCLIENTMETRICSW::lfMessageFont` through `SPI_GETNONCLIENTMETRICS`, because the interface font
there is **not a constant** — the shell picks it per locale:

| Locale | `lfMessageFont` |
|---|---|
| Latin | Segoe UI |
| Japanese | Yu Gothic UI |
| Korean | Malgun Gothic |
| Traditional Chinese | Microsoft JhengHei UI |
| Thai | Leelawadee UI |

Hard-coding `Segoe UI` would put Latin metrics on a Japanese desktop. Reading it also picks up a
user's own font change, which is an accessibility setting on Windows and not merely a preference.

The monospace generic moves too, from `Courier New` — a typewriter face that hints badly at
interface sizes — to Consolas on Windows, SF Mono on macOS and DejaVu Sans Mono elsewhere. That one
*is* a constant, because no platform exposes a system-monospace setting to query, and the code says
so rather than pretending otherwise.

`cargo run -p sphere-text --example glyph_quad_probe` reports both the queried family and the family
that actually resolved, which is how to check this on a machine rather than assume it.

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

## Three things that make text look soft, and what is done about them

The distance-field maths above is correct and still produces mushy text if any of these is wrong.
All three were, and all three are fixed.

### The quad has to cover the whole field, not the ink

`generate_mtsdf` writes the glyph's ink box **outset by `range_em` on every side**, and says so at
the point of definition: the caller must draw `bounds_em.outset(range_em)`, not `bounds_em`.

The batch compiler drew `bounds_em`. The UVs still spanned the full padded image, so the whole
field was squeezed into the smaller ink box — every glyph rendered *smaller than its own advance*,
by a factor that depends on how wide the glyph is:

```text
font: Segoe UI 14 px, "Preferences"
glyph     advance     ink w    quad w ink/quad
P           9.338     7.649     9.983    0.766
r           4.662     3.944     6.278    0.628
e           7.786     6.692     9.026    0.741
```

A line came out about a quarter too small with correct spacing, which reads as text that is both
tiny and tracked out — and because the ratio varies per glyph, narrow letters shrank more than wide
ones, so the letterforms were distorted rather than merely scaled.

The probe that now lives at `glyph_quad_probe` renders the same comparison for the *rasterisation
path* rather than the quad rule, since the quad bug is fixed and has a regression test. It
re-implements `text.wgsl` on the CPU, so it demonstrates that the geometry and the path choice handed
to the GPU are right; it is not a capture of what the GPU drew, and it says so at the top of the
file. There are still no golden-image tests — see the gaps in `performance.md`.

`range_em` is zero on the bitmap path, so the outset is unconditional. A test asserts that, because
the correctness of the snapping below depends on a bitmap quad staying exactly 1:1 with its texels.

### Subpixel placement

The atlas's UV convention is *edge-aligned*: `uv_rect` bounds the outer edge of the texel block, and
the documented promise is that interpolating across a quad covering exactly `texels.size` device
pixels lands on texel centres.

Nothing was arranging for the quad to do either, so a bitmap glyph was sampled off its own texel
grid and came out as a blurred copy of itself. `snap_glyph_quad` in the batch compiler fixes it,
with a different answer per axis:

| Axis | MTSDF | Bitmap |
|---|---|---|
| Vertical | Snapped | Snapped |
| Horizontal | **Not** snapped | Snapped |
| Extent | Exact | Snapped to the atlas texel count |

The baseline is always snapped, because a fractional device y softens every glyph in the run
identically and nothing is gained by leaving it. Horizontal is where they differ: a distance field
reconstructs correctly at any subpixel x, and quantising it would visibly quantise letter spacing at
small sizes — but a bitmap has no such property, so its origin *and* its extent are snapped.

**The correction is derived from the pen, not from the quad.** This is the part that is easy to get
wrong, and getting it wrong is worse than not snapping at all. Every glyph has a different ink top —
an `x`, an `l` and a `g` all begin at different heights, at fractional offsets — so rounding each
quad's own top edge hands every glyph in the line a *different* sub-pixel shift and tears the shared
baseline apart. The pen is the one coordinate they have in common, so that is what gets rounded, and
the whole run moves by a single delta. A test builds a run from two glyphs with deliberately
different ink tops and asserts they move together.

A bitmap is the exception that may be snapped directly: `rasterize_shape` snaps its ink box outwards
to whole device pixels on purpose, so the box is already an integer number of pixels from the pen and
rounding it is both grid-exact and run-consistent.

Snapping only happens when the transform is a translation. Under rotation there is no pixel grid to
snap to, and forcing one would make text crawl as the transform animates.

### Coverage in linear light

Everything in this engine blends in linear light, which is physically correct and is what makes
gradients and translucent overlaps come out right. Text is the one primitive it is wrong for, and it
was wrong by a lot.

Coverage is a geometric fraction of a pixel. Written into a linear buffer and encoded by an sRGB
surface, half coverage of white on black shows up at **sRGB 0.735** — the single grey pixel beside a
stem comes out nearly three quarters as bright as the stem itself. A one-pixel antialiased edge
therefore reads as a two-pixel glow, and that glow is what "soft text" is. No rasteriser change fixes
it, because the coverage the rasteriser produced was correct.

Every traditional text renderer composites glyph coverage in *gamma* space instead. egui goes as far
as asking its backend for a non-sRGB framebuffer specifically so that its whole pipeline blends that
way, and warns when it is handed an sRGB one. Sphere cannot follow it there — the linear blend is
load-bearing for every other primitive — so it reproduces the same result by bending the coverage
ramp before the blend, per glyph, in the shader.

Which way it bends depends on which side of the background the text sits:

```text
light text on dark:  alpha = coverage ^ k
dark text on light:  alpha = 1 - (1 - coverage) ^ k
```

Those are **mirror images, not reciprocal exponents**. The blend is linear in the *destination*, so
it is always the end of the ramp nearest the background that has to bend. Applying one exponent in
both directions — which is what this engine did before, with 1.25 for light-on-dark and 0.8 for
dark-on-light — cancels a quarter of the error at one end and adds to it at the other.

[`GlyphRun::coverage_contrast`] carries both: `|value|` is `k`, and its sign picks the branch.
`1.0` disables the correction, which is what text over an image wants, since there is no one
background to correct against. `Label::coverage_contrast` sets it per label.

**The exponent is solved, not interpolated.** [`coverage_contrast_for`] takes the two colours, finds
their midpoint in sRGB, decodes it, and reads off the alpha a linear blend needs in order to land
there; `k` follows from inverting whichever branch applies. Black on white solves to 2.224, which is
where the documented `FULL_COVERAGE_CONTRAST` of 2.2 comes from.

Interpolating on the luminance gap instead — `1 + |delta| * 1.2`, the obvious thing to write — is
wrong in the case that matters most. Muted grey text on a dark ground has roughly a third of the
luminance contrast of white on black, but it sits almost entirely inside the steep part of the
encode and needs nearly the same exponent: **1.82 against 2.22**. An interpolated ramp hands it under
1.5, and every secondary label in the interface stays soft while the primary ones come good.

Measured against a gamma-space blend across the whole coverage ramp, as RMS and worst-case error in
8-bit levels:

| pair | solved `k` | before | after |
|---|---|---|---|
| white on black | +2.224 | 39 / 53 | 3.2 / 9.0 |
| black on white | −2.224 | 43 / 62 | 3.2 / 9.0 |
| dark theme, `#E6E9EF` on `#14161A` | +1.970 | 27 / 38 | 3.7 / 8.3 |
| dark theme muted, `#939BA8` | +1.817 | 18 / 25 | 2.5 / 5.5 |
| light theme, `#1A1D22` on `#F5F6F8` | −1.924 | 29 / 41 | 4.0 / 8.7 |
| light theme muted, `#606772` | −1.439 | 8 / 12 | 2.3 / 4.7 |

In displacement terms the old calibration moved the perceived edge by 0.18 to 0.24 px on each side
of every stem — most of half a pixel of stroke weight, on every glyph.

`the_correction_reproduces_a_gamma_space_blend` checks every one of those pairs at 33 points along
the ramp. `alpha_from_coverage` is the CPU copy of the shader's `apply_coverage_contrast`; the probe
and the tests call it rather than writing the formula a second time.

This is a *perceptual* correction, not a physical one, and the documentation says so where the
constant is defined rather than leaving it to be discovered.

### The probe used to blend in the wrong space

`glyph_quad_probe` transcribes `text.wgsl` on the CPU, and for a while it transcribed everything
except the blend: it composited straight into sRGB bytes, which *is* a gamma-space blend — the very
thing the correction exists to reproduce. So it showed text considerably crisper than the GPU was
drawing and hid the calibration bug it was built to catch, while its own doc comment claimed "the
pixels shown are exactly the pixels rendered".

It now holds a linear-light float canvas, blends premultiplied into it, and encodes to sRGB once on
the way out, which is the arrangement the hardware uses. `SPHERE_PROBE_COMPARE=blend` draws the same
glyphs twice with only the correction differing:

```bash
SPHERE_PROBE_COMPARE=blend SPHERE_PROBE_ZOOM=10 SPHERE_PROBE_TEXT=Hamburgefonstiv   cargo run -p sphere-text --example glyph_quad_probe --release -- out.png
SPHERE_PROBE_THEME=dark SPHERE_PROBE_COMPARE=blend   cargo run -p sphere-text --example glyph_quad_probe --release
```

A CPU transcription still cannot prove the GPU agreed. What it can do is stop disagreeing on purpose.

### Layers have to land on the device grid

`snap_glyph_quad` rounds a glyph to the device grid in *absolute* coordinates, knowing nothing about
which render target it will be bound into. That is fine for the surface, whose origin is zero, and it
was not fine for an offscreen layer: `begin_layer` sized the target from `bounds.round_out(scale)` —
floored origin, ceiled extent — but handed the vertex stage the raw fractional `bounds.origin` to
subtract. Every snapped glyph inside a layer therefore sat `frac(origin * scale)` off its texel and
went through the atlas's linear sampler as a real bilinear blur, and the composite then resampled a
`round_out`-sized texture into a fractional destination rect on top of that.

Both now come from one snapped rectangle: the origin is the device-aligned one the texture actually
starts at, and the extent is the *clamped* size, since an oversized layer is capped and describing
the destination with the unclamped rect would stretch the texture across it.

Nothing in the shipped widgets opens a layer today — `BuiltNode::opacity` is fixed at 1.0 — so this
was latent. It would have surfaced the first time anyone animated a panel's opacity, as "all the text
in that panel goes blurry while it fades", which is a hard thing to attribute after the fact.
