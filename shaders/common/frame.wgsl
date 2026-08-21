// Per-frame bindings, shared by every Sphere pipeline.
//
// Group 0 is bound once per pass and never rebound per draw call, which is why
// transforms, clips and gradients are indexed tables rather than per-instance
// payloads: a bind is a state change, an index is free.

struct FrameUniforms {
    /// Render target size in device pixels.
    viewport: vec2<f32>,
    /// The surface's scale factor.
    scale_factor: f32,
    /// Seconds since start, for animated effects.
    time: f32,
    /// The current target's origin in scene-absolute logical pixels.
    ///
    /// Offscreen layers are allocated only as large as their content, so this
    /// is subtracted to bring scene coordinates into target space. On the
    /// surface it is zero.
    target_origin: vec2<f32>,
    _pad: vec2<f32>,
}

struct Transform {
    /// The linear part, `[a, b, c, d]`.
    matrix: vec4<f32>,
    /// `[tx, ty, 0, 0]`, already scaled to device pixels.
    translation: vec4<f32>,
}

struct Clip {
    /// `[min_x, min_y, max_x, max_y]` in logical pixels.
    bounds: vec4<f32>,
    /// Corner radii in `[tl, tr, br, bl]` order; all zero for a plain rect.
    radii: vec4<f32>,
}

const MAX_GRADIENT_STOPS: u32 = 8u;

struct Gradient {
    /// Linear: `[x0, y0, x1, y1]`. Radial: `[cx, cy, rx, ry]`.
    /// Sweep: `[cx, cy, start_turns, sweep_turns]`.
    geometry: vec4<f32>,
    /// Stop offsets, tail-padded with the final offset.
    offsets: array<vec4<f32>, 2>,
    /// Stop colors, linear premultiplied.
    colors: array<vec4<f32>, 8>,
    /// 0 linear, 1 radial, 2 sweep.
    kind: u32,
    /// How many entries are real.
    stop_count: u32,
    _pad: vec2<u32>,
}

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(0) @binding(1) var<storage, read> transforms: array<Transform>;
@group(0) @binding(2) var<storage, read> clips: array<Clip>;
@group(0) @binding(3) var<storage, read> gradients: array<Gradient>;

/// Applies an indexed transform to a point in logical space, then converts the
/// result to device pixels relative to the current target.
fn apply_transform(index: u32, p: vec2<f32>) -> vec2<f32> {
    let t = transforms[index];
    let world = vec2<f32>(
        t.matrix.x * p.x + t.matrix.z * p.y,
        t.matrix.y * p.x + t.matrix.w * p.y,
    ) * frame.scale_factor + t.translation.xy;
    return world - frame.target_origin * frame.scale_factor;
}

/// The transform's approximate uniform scale, used to widen or narrow
/// distance-field smoothing under zoom.
///
/// The square root of the determinant is the area-preserving mean of the two
/// axis scales, which behaves better than either axis alone under skew.
fn transform_scale(index: u32) -> f32 {
    let t = transforms[index];
    let det = t.matrix.x * t.matrix.w - t.matrix.y * t.matrix.z;
    return sqrt(max(abs(det), 1e-8));
}

/// Converts a device-pixel position into clip space.
///
/// Sphere's y axis points down; WebGPU's clip space points up, so y is flipped
/// exactly here and nowhere else.
fn to_clip_space(device_px: vec2<f32>) -> vec4<f32> {
    let ndc = vec2<f32>(
        device_px.x / max(frame.viewport.x, 1.0) * 2.0 - 1.0,
        1.0 - device_px.y / max(frame.viewport.y, 1.0) * 2.0,
    );
    return vec4<f32>(ndc, 0.0, 1.0);
}

/// Coverage contributed by a clip region, evaluated in *logical* space.
///
/// A plain rectangular clip is already enforced by the scissor rect, so this
/// returns 1.0 for it and only does real work when the clip is rounded. Making
/// every fragment pay for the rounded case would be pure waste in the common
/// path.
fn clip_coverage(index: u32, local_logical: vec2<f32>) -> f32 {
    let c = clips[index];
    if (c.radii.x <= 0.0 && c.radii.y <= 0.0 && c.radii.z <= 0.0 && c.radii.w <= 0.0) {
        return 1.0;
    }
    let center = (c.bounds.xy + c.bounds.zw) * 0.5;
    let half_extent = max((c.bounds.zw - c.bounds.xy) * 0.5, vec2<f32>(0.0));
    let d = sd_rounded_box(local_logical - center, half_extent, c.radii);
    return coverage_from_distance(d);
}

/// Where along a gradient's ramp a point falls, in `0..=1`.
fn gradient_position(g: Gradient, local: vec2<f32>) -> f32 {
    if (g.kind == 0u) {
        let d = g.geometry.zw - g.geometry.xy;
        let len_sq = dot(d, d);
        if (len_sq < 1e-9) {
            return 0.0;
        }
        return clamp(dot(local - g.geometry.xy, d) / len_sq, 0.0, 1.0);
    } else if (g.kind == 1u) {
        let d = (local - g.geometry.xy) / max(g.geometry.zw, vec2<f32>(1e-4));
        return clamp(length(d), 0.0, 1.0);
    }
    let d = local - g.geometry.xy;
    let angle = atan2(d.y, d.x) / TAU;
    let span = select(g.geometry.w, 1.0, abs(g.geometry.w) < 1e-6);
    return clamp(fract(angle - g.geometry.z) / span, 0.0, 1.0);
}

/// Reads a stop offset out of the packed `vec4` pair.
fn gradient_offset(g: Gradient, i: u32) -> f32 {
    let v = g.offsets[i / 4u];
    let lane = i % 4u;
    if (lane == 0u) { return v.x; }
    if (lane == 1u) { return v.y; }
    if (lane == 2u) { return v.z; }
    return v.w;
}

/// Samples a gradient's ramp, returning a linear premultiplied color.
///
/// Stops are already sorted and tail-padded by the CPU, so this is a fixed
/// eight-iteration loop with no branching on stop count and no sorting.
fn sample_gradient(index: u32, local: vec2<f32>) -> vec4<f32> {
    let g = gradients[index];
    if (g.stop_count == 0u) {
        return vec4<f32>(0.0);
    }
    let t = gradient_position(g, local);
    var result = g.colors[0];
    for (var i = 1u; i < MAX_GRADIENT_STOPS; i = i + 1u) {
        if (i >= g.stop_count) {
            break;
        }
        let a = gradient_offset(g, i - 1u);
        let b = gradient_offset(g, i);
        let span = b - a;
        // Coincident stops are a legal way to author a hard colour break.
        let local_t = select(0.0, clamp((t - a) / span, 0.0, 1.0), span > 1e-6);
        let step_taken = select(0.0, 1.0, t >= a);
        result = mix(result, mix(g.colors[i - 1u], g.colors[i], local_t), step_taken);
    }
    return result;
}
