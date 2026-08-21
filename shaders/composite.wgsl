// Compositing an offscreen layer back into its parent.
//
// Runs as a fullscreen-triangle pass over the layer's destination rectangle.
// Only layers that genuinely need group semantics — group opacity, a
// non-fixed-function blend mode, a filter — ever reach this shader.
//
//!include common/math.wgsl
//!include common/frame.wgsl

struct CompositeParams {
    /// Destination rectangle in scene-absolute logical pixels.
    dest: vec4<f32>,
    /// Opacity applied while compositing.
    opacity: f32,
    /// Blend mode; see `sphere_core::BlendMode`.
    blend: u32,
    /// Saturation multiplier, 1.0 for no change.
    saturation: f32,
    _pad: f32,
}

@group(1) @binding(0) var layer_texture: texture_2d<f32>;
@group(1) @binding(1) var layer_sampler: sampler;
@group(1) @binding(2) var<uniform> params: CompositeParams;

struct CompositeOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> CompositeOut {
    let corner = vec2<f32>(
        f32(vertex_index & 1u),
        f32((vertex_index >> 1u) & 1u),
    );
    let local = params.dest.xy + corner * params.dest.zw;
    let device = local * frame.scale_factor - frame.target_origin * frame.scale_factor;

    var out: CompositeOut;
    out.position = to_clip_space(device);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: CompositeOut) -> @location(0) vec4<f32> {
    var c = textureSample(layer_texture, layer_sampler, in.uv);

    if (abs(params.saturation - 1.0) > 1e-4) {
        // Luminance weights are applied to unpremultiplied colour, then the
        // result is re-premultiplied; desaturating premultiplied values
        // directly would drag the colour toward black as alpha falls.
        let a = max(c.a, 1e-5);
        let straight = c.rgb / a;
        let luma = dot(straight, vec3<f32>(0.2126, 0.7152, 0.0722));
        let sat = mix(vec3<f32>(luma), straight, params.saturation);
        c = vec4<f32>(sat * a, c.a);
    }

    return c * params.opacity;
}
