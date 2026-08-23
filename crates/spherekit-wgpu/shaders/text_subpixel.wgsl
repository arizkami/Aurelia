// RGB subpixel text. The shared text module supplies GlyphIn/GlyphOut, the
// vertex shader, atlas bindings and coverage contrast. This file is composed
// into a separate shader module because merely declaring @blend_src outputs
// requires the device's DUAL_SOURCE_BLENDING feature, even when another entry
// point is selected.
//
//!include text.wgsl

struct SubpixelFragmentOutput {
    @location(0) @blend_src(0) foreground: vec4<f32>,
    @location(0) @blend_src(1) coverage: vec4<f32>,
}

@fragment
fn fs_subpixel(in: GlyphOut) -> SubpixelFragmentOutput {
    let clip = clip_coverage(in.indices.w, in.world);
    let sample = textureSample(glyph_atlas, glyph_sampler, in.uv);
    let coverage_rgb = vec3<f32>(
        apply_coverage_contrast(sample.r, in.params.z),
        apply_coverage_contrast(sample.g, in.params.z),
        apply_coverage_contrast(sample.b, in.params.z),
    ) * clip;

    // The atlas alpha channel is max(R, G, B), a conservative scalar coverage
    // for callers that inspect the destination alpha. Apply the same transfer
    // curve as the colour channels so source zero and source one remain on one
    // coverage scale.
    let coverage_alpha = apply_coverage_contrast(sample.a, in.params.z) * clip;
    let blend_coverage = vec4<f32>(
        in.color.a * coverage_rgb,
        in.color.a * coverage_alpha,
    );

    var out: SubpixelFragmentOutput;
    out.foreground = vec4<f32>(
        in.color.rgb * coverage_rgb,
        in.color.a * coverage_alpha,
    );
    out.coverage = blend_coverage;
    return out;
}
