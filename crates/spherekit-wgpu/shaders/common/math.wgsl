// Shared math helpers.
//
// SphereKit composes WGSL by textual include (see `spherekit_wgpu::shader`), so this
// file must be self-contained and must not declare bindings.

const TAU: f32 = 6.283185307179586;

/// The middle of three values. Reconstructing the true distance from a
/// multi-channel distance field is exactly this operation, so it is the single
/// most performance-relevant helper in the text path.
fn median3(a: f32, b: f32, c: f32) -> f32 {
    return max(min(a, b), min(max(a, b), c));
}

/// Signed distance to an axis-aligned rounded box.
///
/// `p` is relative to the box centre, `b` is the half-extent, and `r` holds the
/// four radii in `[top_left, top_right, bottom_right, bottom_left]` order —
/// the same order SphereKit's `Corners<T>` uses on the CPU, so the two cannot
/// drift apart.
///
/// Negative inside, positive outside, and the gradient has unit length almost
/// everywhere, which is what makes a single `fwidth` a correct antialiasing
/// width regardless of the shape's scale.
fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: vec4<f32>) -> f32 {
    var radius: f32;
    if (p.y < 0.0) {
        radius = select(r.y, r.x, p.x < 0.0);
    } else {
        radius = select(r.z, r.w, p.x < 0.0);
    }
    radius = min(radius, min(b.x, b.y));
    let q = abs(p) - b + vec2<f32>(radius);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - radius;
}

/// Converts a signed distance into coverage using screen-space derivatives.
///
/// A fixed threshold would break the moment the shape is scaled or zoomed;
/// `fwidth` measures how much the distance changes across one actual pixel, so
/// the antialiasing band is always exactly one pixel wide no matter the
/// transform.
fn coverage_from_distance(d: f32) -> f32 {
    let w = max(fwidth(d), 1e-5);
    return clamp(0.5 - d / w, 0.0, 1.0);
}

/// Abramowitz and Stegun 7.1.26 error-function approximation.
///
/// Accurate to about 5e-4, which is far below one 8-bit quantisation step, and
/// it is what makes an analytic Gaussian box shadow possible without a blur
/// pass.
fn erf2(x: vec2<f32>) -> vec2<f32> {
    let s = sign(x);
    let a = abs(x);
    var r = 1.0 + (0.278393 + (0.230389 + 0.078108 * a * a) * a) * a;
    r = r * r;
    r = r * r;
    return s - s / r;
}
