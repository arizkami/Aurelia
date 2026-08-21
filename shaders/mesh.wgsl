// The mesh pipeline: tessellated paths and realtime audio geometry.
//
// Mesh vertices carry their transform baked in and their colour per vertex, so
// this pipeline is deliberately thin. A realtime spectrum generating triangles
// every frame should reach the GPU through as little shader as possible.
//
//!include common/math.wgsl
//!include common/frame.wgsl

@group(1) @binding(0) var mesh_texture: texture_2d<f32>;
@group(1) @binding(1) var mesh_sampler: sampler;

struct MeshIn {
    /// Position in scene-absolute logical pixels; the transform is already baked.
    @location(0) position: vec2<f32>,
    /// Texture coordinate, ignored when the batch has no texture.
    @location(1) uv: vec2<f32>,
    /// Linear premultiplied colour.
    @location(2) color: vec4<f32>,
}

struct MeshOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

@vertex
fn vs_main(v: MeshIn) -> MeshOut {
    var out: MeshOut;
    let device = v.position * frame.scale_factor
        - frame.target_origin * frame.scale_factor;
    out.position = to_clip_space(device);
    out.uv = v.uv;
    out.color = v.color;
    return out;
}

@fragment
fn fs_main(in: MeshOut) -> @location(0) vec4<f32> {
    return in.color;
}

@fragment
fn fs_textured(in: MeshOut) -> @location(0) vec4<f32> {
    return textureSample(mesh_texture, mesh_sampler, in.uv) * in.color;
}
