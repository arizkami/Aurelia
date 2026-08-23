// The quad pipeline: rectangles, rounded rectangles, borders, gradients,
// textured quads and box shadows.
//
// One pipeline covers all of them because they share a shape (a rectangle with
// four radii) and differ only in how the fragment is coloured. Splitting them
// would multiply pipeline switches across a frame that is overwhelmingly made
// of exactly this primitive.
//
//!include common/math.wgsl
//!include common/frame.wgsl
//!include common/shadow.wgsl

// Keep in sync with `spherekit_render::primitives::quad_flags`.
const FILL_SOLID: u32 = 1u;
const FILL_GRADIENT: u32 = 2u;
const FILL_TEXTURE: u32 = 4u;
const SHADOW: u32 = 8u;
const SHADOW_INSET: u32 = 16u;
const CLIP_ROUNDED: u32 = 32u;
const BORDER: u32 = 64u;

@group(1) @binding(0) var quad_texture: texture_2d<f32>;
@group(1) @binding(1) var quad_sampler: sampler;

struct QuadIn {
    /// `[x, y, w, h]` in local logical pixels.
    @location(0) bounds: vec4<f32>,
    /// `[tl, tr, br, bl]` radii in logical pixels.
    @location(1) radii: vec4<f32>,
    /// Linear premultiplied fill or shadow colour.
    @location(2) color: vec4<f32>,
    /// Border colour, or the source rectangle for textured quads.
    @location(3) border_color: vec4<f32>,
    /// `[border_width, blur_sigma]`.
    @location(4) params: vec2<f32>,
    /// `[flags, gradient, transform_index, clip_index]`.
    @location(5) indices: vec4<u32>,
}

struct QuadOut {
    @builtin(position) position: vec4<f32>,
    /// Position within the quad's local logical space.
    @location(0) local: vec2<f32>,
    /// Position in scene-absolute logical space, for clip evaluation.
    @location(1) world: vec2<f32>,
    @location(2) @interpolate(flat) bounds: vec4<f32>,
    @location(3) @interpolate(flat) radii: vec4<f32>,
    @location(4) @interpolate(flat) color: vec4<f32>,
    @location(5) @interpolate(flat) border_color: vec4<f32>,
    @location(6) @interpolate(flat) params: vec2<f32>,
    @location(7) @interpolate(flat) indices: vec4<u32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, inst: QuadIn) -> QuadOut {
    // A triangle strip over the unit square. Generating corners from the vertex
    // index means no vertex buffer at all for the most common primitive.
    let corner = vec2<f32>(
        f32(vertex_index & 1u),
        f32((vertex_index >> 1u) & 1u),
    );

    let flags = inst.indices.x;
    // A shadow's blurred footprint extends beyond its shape, so the quad has to
    // be grown or the tail would be clipped by its own geometry.
    var pad = 0.0;
    if ((flags & SHADOW) != 0u) {
        pad = inst.params.y * 3.0 + 1.0;
    } else if ((flags & BORDER) != 0u) {
        // Borders are drawn inset, but antialiasing still needs a half pixel.
        pad = 1.0;
    } else {
        pad = 1.0;
    }

    let origin = inst.bounds.xy - vec2<f32>(pad);
    let extent = inst.bounds.zw + vec2<f32>(pad * 2.0);
    let local = origin + corner * extent;

    var out: QuadOut;
    out.position = to_clip_space(apply_transform(inst.indices.z, local));
    out.local = local;
    out.world = local;
    out.bounds = inst.bounds;
    out.radii = inst.radii;
    out.color = inst.color;
    out.border_color = inst.border_color;
    out.params = inst.params;
    out.indices = inst.indices;
    return out;
}

@fragment
fn fs_main(in: QuadOut) -> @location(0) vec4<f32> {
    let flags = in.indices.x;
    let center = in.bounds.xy + in.bounds.zw * 0.5;
    let half_extent = max(in.bounds.zw * 0.5, vec2<f32>(0.0));
    let p = in.local - center;

    var color = in.color;
    var coverage: f32;

    if ((flags & SHADOW) != 0u) {
        let blur = shadow_coverage(half_extent, in.radii, p, max(in.params.y, 1e-3));
        if ((flags & SHADOW_INSET) != 0u) {
            // An inset shadow is the complement of the blur, confined to the
            // shape itself.
            let inside = coverage_from_distance(sd_rounded_box(p, half_extent, in.radii));
            coverage = (1.0 - blur) * inside;
        } else {
            coverage = blur;
        }
    } else {
        let d = sd_rounded_box(p, half_extent, in.radii);
        coverage = coverage_from_distance(d);

        if ((flags & FILL_GRADIENT) != 0u) {
            color = sample_gradient(in.indices.y, in.local);
        } else if ((flags & FILL_TEXTURE) != 0u) {
            // The source rectangle rides in the border-colour slot for
            // textured quads, which keeps the instance at 96 bytes.
            let uv01 = clamp((in.local - in.bounds.xy) / max(in.bounds.zw, vec2<f32>(1e-4)),
                             vec2<f32>(0.0), vec2<f32>(1.0));
            let uv = in.border_color.xy + uv01 * in.border_color.zw;
            let texel = textureSample(quad_texture, quad_sampler, uv);
            color = texel * in.color;
        } else if ((flags & FILL_SOLID) == 0u) {
            color = vec4<f32>(0.0);
        }

        if ((flags & BORDER) != 0u) {
            let w = max(in.params.x, 0.0);
            // The inner edge is the same rounded box shrunk by the border
            // width; radii shrink with it so the corner stays concentric
            // rather than developing a flat spot.
            let inner_half = max(half_extent - vec2<f32>(w), vec2<f32>(0.0));
            let inner_radii = max(in.radii - vec4<f32>(w), vec4<f32>(0.0));
            let inner_d = sd_rounded_box(p, inner_half, inner_radii);
            let inner_cov = coverage_from_distance(inner_d);
            let ring = max(coverage - inner_cov, 0.0);
            // Both colours are premultiplied, so compositing the fill under the
            // border is a plain source-over.
            let fill = color * inner_cov;
            let border = in.border_color * ring;
            return (border + fill * (1.0 - in.border_color.a * ring))
                * clip_coverage(in.indices.w, in.world);
        }
    }

    return color * coverage * clip_coverage(in.indices.w, in.world);
}
