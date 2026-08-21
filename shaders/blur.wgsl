// Separable Gaussian blur for layer filters and backdrop blur.
//
// Two passes, horizontal then vertical. A separable kernel turns an O(r^2)
// convolution into O(r), which is the difference between a usable backdrop blur
// and a dropped frame.
//
//!include common/math.wgsl

struct BlurParams {
    /// Texel size of the source, `1.0 / dimensions`.
    texel: vec2<f32>,
    /// `(1, 0)` for the horizontal pass, `(0, 1)` for the vertical.
    direction: vec2<f32>,
    /// Gaussian standard deviation, in source texels.
    sigma: f32,
    /// How many taps to take on each side of centre.
    radius: i32,
    _pad: vec2<f32>,
}

@group(0) @binding(0) var src_texture: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;
@group(0) @binding(2) var<uniform> blur: BlurParams;

struct BlurOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> BlurOut {
    // A single oversized triangle covers the target with no index buffer and
    // no diagonal seam between two triangles.
    let uv = vec2<f32>(
        f32((vertex_index << 1u) & 2u),
        f32(vertex_index & 2u),
    );
    var out: BlurOut;
    out.position = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(uv.x, 1.0 - uv.y);
    return out;
}

@fragment
fn fs_main(in: BlurOut) -> @location(0) vec4<f32> {
    let sigma = max(blur.sigma, 1e-3);
    let inv_two_sigma_sq = 1.0 / (2.0 * sigma * sigma);

    var total = textureSample(src_texture, src_sampler, in.uv);
    var weight_sum = 1.0;

    // Symmetric taps, accumulated in pairs. The kernel is normalised at the end
    // rather than precomputed, so a truncated radius still integrates to one
    // and the image does not darken at the edges.
    for (var i = 1; i <= blur.radius; i = i + 1) {
        let offset = f32(i) * blur.texel * blur.direction;
        let w = exp(-f32(i) * f32(i) * inv_two_sigma_sq);
        total = total
            + textureSample(src_texture, src_sampler, in.uv + offset) * w
            + textureSample(src_texture, src_sampler, in.uv - offset) * w;
        weight_sum = weight_sum + 2.0 * w;
    }

    return total / weight_sum;
}
