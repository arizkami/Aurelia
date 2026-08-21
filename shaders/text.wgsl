// The text pipeline: MTSDF glyphs with optional outline, plus the grayscale
// bitmap fallback for very small sizes.
//
// The key property is that smoothing is derived from the glyph's *actual*
// on-screen size, not from a constant. A fixed threshold produces text that is
// crisp at one size and either blurry or aliased at every other, and breaks
// entirely the moment a panel is zoomed.
//
//!include common/math.wgsl
//!include common/frame.wgsl

// Keep in sync with `sphere_render::primitives::glyph_flags`.
const BITMAP: u32 = 1u;
const OUTLINE: u32 = 2u;
const CLIP_ROUNDED: u32 = 4u;

@group(1) @binding(0) var glyph_atlas: texture_2d<f32>;
@group(1) @binding(1) var glyph_sampler: sampler;

/// Applies the perceptual coverage correction.
///
/// Coverage is a geometric fraction, and compositing it in linear light — which
/// is physically right and what every other primitive here does — makes
/// light-on-dark text bloom: half coverage of white on black is linear 0.5,
/// which is sRGB 0.735, visibly heavier than a gamma-space rasteriser would
/// produce. An exponent above one pulls the midtones back and restores the
/// intended stroke weight. See `GlyphRun::coverage_gamma` for why this is a
/// knob and not a constant.
fn apply_coverage_gamma(coverage: f32, gamma: f32) -> f32 {
    if (abs(gamma - 1.0) < 0.001) {
        return coverage;
    }
    return pow(clamp(coverage, 0.0, 1.0), gamma);
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
    /// `[px_range, outline_width, coverage_gamma, unused]`.
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
    /// `[screen_px_range, outline_width, coverage_gamma, unused]`.
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
    // `px_range` arrives as the field's range in destination pixels at the
    // glyph's nominal size. Folding in the transform's scale here is what keeps
    // zoomed text as sharp as unzoomed text without a second rasterisation.
    let screen_range = max(inst.params.x * transform_scale(inst.indices.z), 1.0);
    out.params = vec4<f32>(screen_range, inst.params.y, inst.params.z, 0.0);
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
        // wants the same perceptual correction the field path gets.
        return in.color * apply_coverage_gamma(sample.r, in.params.z) * clip;
    }

    // Median of the three channels reconstructs the true signed distance while
    // preserving the sharp corners that a single-channel field rounds off.
    let sd = median3(sample.r, sample.g, sample.b) - 0.5;
    let screen_range = in.params.x;
    let fill_alpha = apply_coverage_gamma(clamp(sd * screen_range + 0.5, 0.0, 1.0), in.params.z);

    if ((flags & OUTLINE) != 0u) {
        // The alpha channel carries the *true* distance, which is what makes a
        // uniform-width outline possible: the multi-channel median is exact at
        // the edge but not at a fixed offset from it.
        let true_sd = sample.a - 0.5;
        // The outline width is authored in logical pixels; convert into the
        // same normalised distance units the field uses.
        let half_width = in.params.y * 0.5 * screen_range;
        let outline_alpha = clamp(true_sd * screen_range + 0.5 + half_width, 0.0, 1.0);
        // Fill over outline, both premultiplied.
        let outline = in.outline_color * max(outline_alpha - fill_alpha, 0.0);
        let fill = in.color * fill_alpha;
        return (fill + outline * (1.0 - in.color.a * fill_alpha)) * clip;
    }

    return in.color * fill_alpha * clip;
}
