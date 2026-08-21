// The text pipeline: MTSDF glyphs with optional outline, plus the grayscale
// bitmap fallback for very small sizes.
//
// The key property is that smoothing is derived from the glyph's *actual*
// on-screen size, not from a constant. A fixed threshold produces text that is
// crisp at one size and either blurry or aliased at every other, and breaks
// entirely the moment a panel is zoomed. `mtsdf_edge_ramp` below is where that
// size becomes a ramp, and where small text picks up its optical weight
// compensation.
//
//!include common/math.wgsl
//!include common/frame.wgsl

// Keep in sync with `spherekit_render::primitives::glyph_flags`.
const BITMAP: u32 = 1u;
const OUTLINE: u32 = 2u;
const CLIP_ROUNDED: u32 = 4u;
const SUBPIXEL: u32 = 8u;

@group(1) @binding(0) var glyph_atlas: texture_2d<f32>;
@group(1) @binding(1) var glyph_sampler: sampler;

/// Bends the coverage ramp so the linear blend and the sRGB encode downstream
/// reproduce a gamma-space blend.
///
/// Coverage is a geometric fraction of a pixel. SphereKit blends in linear light,
/// which is right for every other primitive and wrong for this one: half
/// coverage of white on black lands in the buffer as linear 0.5, which the sRGB
/// surface shows as 0.735. The grey pixel beside a stem comes out nearly as
/// bright as the stem, and a one-pixel edge reads as a two-pixel glow. That glow
/// is what soft text is.
///
/// `contrast` carries both the strength and the direction, because the two
/// directions need mirrored curves rather than reciprocal exponents — the blend
/// is linear in the *destination*, so it is always the end of the ramp nearest
/// the background that has to bend. See `GlyphRun::coverage_contrast`, and
/// `spherekit_render::scene::alpha_from_coverage`, which is this function on the
/// CPU and must not drift from it.
fn apply_coverage_contrast(coverage: f32, contrast: f32) -> f32 {
    let c = clamp(coverage, 0.0, 1.0);
    let k = abs(contrast);
    if (k <= 1.0) {
        return c;
    }
    if (contrast > 0.0) {
        // Light text on dark: steepen the low end, which is the end that the
        // encode lifts.
        return pow(c, k);
    }
    // Dark text on light: the mirror image, steepening the high end.
    return 1.0 - pow(1.0 - c, k);
}

/// The on-screen em size at and above which the small-text compensation is
/// inert. Keep in sync with `spherekit_render::scene::SMALL_TEXT_MAX_DEVICE_PX`.
const SMALL_TEXT_MAX_DEVICE_PX: f32 = 24.0;
/// The widest outward edge bias, in device pixels. Keep in sync with
/// `spherekit_render::scene::MAX_EDGE_BIAS_PX`.
const MAX_EDGE_BIAS_PX: f32 = 0.15;
/// The narrowest antialiasing ramp, in device pixels. Keep in sync with
/// `spherekit_render::scene::MIN_EDGE_RAMP_PX`.
const MIN_EDGE_RAMP_PX: f32 = 0.65;

/// The distance-to-coverage ramp for one glyph, as `vec2(scale, offset)`.
///
/// Below `SMALL_TEXT_MAX_DEVICE_PX` the outline is pushed outward by a fraction
/// of a pixel, because a thin dark stroke reads lighter than its coverage says
/// it should — the compensation FreeType calls stem darkening. The field is not
/// geometrically thin at these sizes and this does not pretend to correct it;
/// see `spherekit_render::scene::MAX_EDGE_BIAS_PX`, which carries the
/// measurements and the reasoning.
///
/// The bias has to be paid for out of the field's own range. A texel stores
/// `distance / range + 0.5` clamped into a byte, so beyond half a range the
/// field is saturated and the shader's coverage sits at exactly zero. Offsetting
/// that saturated value lifts the whole padded quad off the background — a grey
/// box around every glyph, not a heavier one. So the bias is capped at the
/// range's spare capacity and the ramp narrowed to absorb it, which keeps the
/// far background at hard zero by construction.
///
/// At `bias == 0` this reduces exactly to `max(px_range, 1)` and an offset of
/// zero, which is what the shader did before the compensation existed.
fn mtsdf_edge_ramp(em_px: f32, px_range: f32) -> vec2<f32> {
    let range = max(px_range, 1e-3);
    let smallness = clamp(1.0 - max(em_px, 0.0) / SMALL_TEXT_MAX_DEVICE_PX, 0.0, 1.0);
    let headroom = max((range - MIN_EDGE_RAMP_PX) * 0.5, 0.0);
    let bias = min(MAX_EDGE_BIAS_PX * smallness, headroom);
    let ramp = max(min(range - 2.0 * bias, 1.0), 1e-4);
    return vec2<f32>(range / ramp, bias / ramp);
}

struct GlyphIn {
    /// `[x, y, w, h]` of the glyph quad in local logical pixels.
    @location(0) bounds: vec4<f32>,
    /// `[u0, v0, u1, v1]` in normalised atlas coordinates.
    @location(1) uv: vec4<f32>,
    /// Linear premultiplied fill colour.
    @location(2) color: vec4<f32>,
    /// Linear premultiplied outline colour.
    @location(3) outline_color: vec4<f32>,
    /// `[px_range, outline_width, coverage_contrast, em_px]`, the two sizes in
    /// destination pixels at the glyph's nominal scale.
    @location(4) params: vec4<f32>,
    /// `[flags, atlas_page, transform_index, clip_index]`.
    @location(5) indices: vec4<u32>,
}

struct GlyphOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) world: vec2<f32>,
    @location(2) @interpolate(flat) color: vec4<f32>,
    @location(3) @interpolate(flat) outline_color: vec4<f32>,
    /// `[edge_scale, outline_width, coverage_contrast, edge_offset]`, the first
    /// and last as returned by `mtsdf_edge_ramp`.
    @location(4) @interpolate(flat) params: vec4<f32>,
    @location(5) @interpolate(flat) indices: vec4<u32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, inst: GlyphIn) -> GlyphOut {
    let corner = vec2<f32>(
        f32(vertex_index & 1u),
        f32((vertex_index >> 1u) & 1u),
    );
    let local = inst.bounds.xy + corner * inst.bounds.zw;

    var out: GlyphOut;
    out.position = to_clip_space(apply_transform(inst.indices.z, local));
    out.uv = mix(inst.uv.xy, inst.uv.zw, corner);
    out.world = local;
    out.color = inst.color;
    out.outline_color = inst.outline_color;
    // `px_range` and `em_px` arrive as destination-pixel sizes at the glyph's
    // nominal scale. Folding in the transform's scale here is what keeps zoomed
    // text as sharp as unzoomed text without a second rasterisation, and it is
    // also why the small-text compensation is resolved here rather than on the
    // CPU: only the vertex stage knows the size the glyph will actually appear
    // at.
    let zoom = transform_scale(inst.indices.z);
    let ramp = mtsdf_edge_ramp(inst.params.w * zoom, inst.params.x * zoom);
    out.params = vec4<f32>(ramp.x, inst.params.y, inst.params.z, ramp.y);
    out.indices = inst.indices;
    return out;
}

@fragment
fn fs_main(in: GlyphOut) -> @location(0) vec4<f32> {
    let flags = in.indices.x;
    let clip = clip_coverage(in.indices.w, in.world);
    let sample = textureSample(glyph_atlas, glyph_sampler, in.uv);

    if ((flags & BITMAP) != 0u) {
        // The bitmap fallback stores coverage in red. It is already
        // size-specific, so it needs no distance reconstruction at all — but it
        // wants the same blend-space correction the field path gets.
        return in.color * apply_coverage_contrast(sample.r, in.params.z) * clip;
    }

    // Median of the three channels reconstructs the true signed distance while
    // preserving the sharp corners that a single-channel field rounds off.
    let sd = median3(sample.r, sample.g, sample.b) - 0.5;
    // `edge_scale` is the field range over the ramp width, and `edge_offset` the
    // small-text bias measured in ramp widths. Both come from `mtsdf_edge_ramp`
    // in the vertex stage; at sizes at or above `SMALL_TEXT_MAX_DEVICE_PX` the
    // offset is zero and the scale is the plain `max(screen_range, 1)`.
    let edge_scale = in.params.x;
    let edge_offset = in.params.w;
    let fill_alpha =
        apply_coverage_contrast(clamp(sd * edge_scale + edge_offset + 0.5, 0.0, 1.0), in.params.z);

    if ((flags & OUTLINE) != 0u) {
        // The alpha channel carries the *true* distance, which is what makes a
        // uniform-width outline possible: the multi-channel median is exact at
        // the edge but not at a fixed offset from it.
        let true_sd = sample.a - 0.5;
        // The outline width is authored in logical pixels; convert into the
        // same normalised distance units the field uses.
        let half_width = in.params.y * 0.5 * edge_scale;
        // Corrected on the same curve as the fill, and for the same reason the
        // ring below is a difference of two alphas: subtracting a bent value
        // from a raw one is not a coverage at all, and it shows up as the
        // outline colour bleeding inside the glyph's own edge. Both ends of the
        // subtraction have to sit on one scale. The fill's exponent is reused
        // rather than derived for the outline colour, because there is one
        // parameter and the ring is at most a couple of pixels wide.
        // The bias moves the outer ring by exactly what it moved the fill by, so
        // a compensated glyph keeps the outline width it asked for instead of
        // eating into it.
        let outline_alpha = apply_coverage_contrast(
            clamp(true_sd * edge_scale + edge_offset + 0.5 + half_width, 0.0, 1.0),
            in.params.z,
        );
        // Fill over outline, both premultiplied. The correction is monotonic, so
        // the ring stays non-negative wherever it was before.
        let outline = in.outline_color * max(outline_alpha - fill_alpha, 0.0);
        let fill = in.color * fill_alpha;
        return (fill + outline * (1.0 - in.color.a * fill_alpha)) * clip;
    }

    return in.color * fill_alpha * clip;
}
