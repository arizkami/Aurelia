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

/// Coverage of a Gaussian-blurred *sharp* box, evaluated analytically.
///
/// `lower` and `upper` are the box corners relative to the sample point's
/// space; `sigma` is the Gaussian standard deviation.
fn blurred_box(lower: vec2<f32>, upper: vec2<f32>, p: vec2<f32>, sigma: f32) -> f32 {
    let k = 1.0 / (sigma * 1.4142135623730951);
    let i = erf2((p - lower) * k) - erf2((p - upper) * k);
    return clamp(i.x * i.y * 0.25, 0.0, 1.0);
}

/// Coverage of a Gaussian-blurred *rounded* box.
///
/// The x direction is solved exactly with `erf`; the y direction is integrated
/// numerically over a handful of samples, each of which uses the rounded box's
/// half-width at that height. Six samples is enough that the residual error is
/// invisible at the shadow opacities UI actually uses, and it costs a fraction
/// of a separable two-pass blur plus its render target.
fn blurred_rounded_box(half_extent: vec2<f32>, r: vec4<f32>, p: vec2<f32>, sigma: f32) -> f32 {
    if (sigma < 0.02) {
        return coverage_from_distance(sd_rounded_box(p, half_extent, r));
    }
    // Three sigma captures 99.7 % of the kernel; sampling further is wasted.
    let low = max(-half_extent.y - 3.0 * sigma, p.y - 3.0 * sigma);
    let high = min(half_extent.y + 3.0 * sigma, p.y + 3.0 * sigma);
    if (high <= low) {
        return 0.0;
    }

    let steps = 6;
    let step = (high - low) / f32(steps);
    let inv_sigma = 1.0 / sigma;
    var total = 0.0;
    var weight_sum = 0.0;

    for (var i = 0; i < steps; i = i + 1) {
        let y = low + (f32(i) + 0.5) * step;
        // Gaussian weight of this slice relative to the sample point.
        let dy = (p.y - y) * inv_sigma;
        let w = exp(-0.5 * dy * dy);

        // Half-width of the rounded box at height y. Inside the straight part
        // this is the full half-extent; inside a corner it follows the arc.
        var radius: f32;
        if (y < 0.0) {
            radius = max(r.x, r.y);
        } else {
            radius = max(r.z, r.w);
        }
        radius = min(radius, min(half_extent.x, half_extent.y));
        let corner_y = half_extent.y - radius;
        var half_w = half_extent.x;
        let ay = abs(y);
        if (ay > corner_y && radius > 0.0) {
            let t = (ay - corner_y) / radius;
            half_w = half_extent.x - radius + radius * sqrt(max(0.0, 1.0 - t * t));
        }
        if (ay > half_extent.y) {
            half_w = 0.0;
        }

        if (half_w > 0.0) {
            let k = 1.0 / (sigma * 1.4142135623730951);
            let e = erf2(vec2<f32>((p.x + half_w) * k, (p.x - half_w) * k));
            total = total + w * (e.x - e.y) * 0.5;
        }
        weight_sum = weight_sum + w;
    }

    if (weight_sum <= 0.0) {
        return 0.0;
    }
    return clamp(total / weight_sum, 0.0, 1.0);
}
