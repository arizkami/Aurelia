// Gaussian-blurred rounded rectangles, evaluated analytically.
//
// SphereKit composes WGSL by textual include (see `spherekit_wgpu::shader`), so
// this file must be self-contained and must not declare bindings.
//
//!include common/math.wgsl
//
// ## What this computes
//
// The coverage of a rounded rectangle convolved with a two-dimensional
// Gaussian — which is what a drop shadow *is*, rather than what it is usually
// approximated by. Written out, the value at a point `p` is
//
//     C(p) = ∫∫ box(x, y) · G(p.x - x, p.y - y) dx dy
//
// and because a Gaussian is separable it splits into an inner integral that is
// exact and an outer one that is not:
//
//     C(p) = ∫ [ width coverage of the slice at y ] · G(p.y - y) dy
//
// The inner term is the convolution of an interval with a one-dimensional
// Gaussian, and that has a closed form in `erf`. The outer one has no closed
// form once the sides curve, so it is a midpoint sum over the slices that
// actually matter.
//
// ## Why not a blur pass
//
// A separable two-pass blur needs a render target, two draw calls and a
// round trip through memory for every shadowed box on the screen. This is one
// fragment shader over a quad the shape of the shadow's own footprint, so a
// panel with a shadow costs one instance in the same batch as everything else.
//
// ## The integration bounds, which are the whole correctness story
//
// The integral runs over the **intersection** of two ranges: where the kernel
// still has weight (`p.y ± 3σ`, which holds 99.7 % of it) and where the box
// actually exists (`±half_extent.y`). Outside the first, the weight is zero;
// outside the second, the slice is empty. Clamping to the intersection and
// summing `slice · G · dy` gives the true integral, because `G` is normalised
// and integrates to one over the whole line.
//
// The tempting shortcut — sum the slices, then divide by the total weight of
// the samples taken — is wrong, and wrong in a way that looks plausible. It
// turns the integral into a weighted *average* of the slices that happened to
// be sampled, so a point one sigma outside the edge reports the average
// coverage of the half of the kernel that overlaps the box rather than the
// fraction of the whole kernel that does. The result is a shadow that is
// roughly twice as dark as it should be through the middle of its falloff and
// then collapses too early, which reads as a smudge with a hard inner edge
// instead of a shadow. It is the reason this file exists.

/// How many slices the vertical integral is taken in.
///
/// The samples are spread over the intersection described above, so they are
/// dense exactly where the answer is changing: a point far outside the box gets
/// a narrow range and eight samples across it, not eight samples across six
/// sigma of mostly-empty space. Eight is where the midpoint sum stops being
/// visible against 8-bit output; four is visibly banded on a large blur.
const SHADOW_SLICES: i32 = 8;

/// `sqrt(2 · pi)`, the Gaussian's normalising constant.
const SQRT_TAU: f32 = 2.5066282746310002;

/// The normalised one-dimensional Gaussian.
///
/// Normalised, so the sum of `shadow_gaussian(y) · dy` over the whole line is
/// one and the integral below needs no correction factor.
fn shadow_gaussian(x: f32, sigma: f32) -> f32 {
    return exp(-0.5 * (x * x) / (sigma * sigma)) / (SQRT_TAU * sigma);
}

/// Coverage of one horizontal slice of a rounded box, blurred in x only.
///
/// `y` is the slice's signed distance from the box centre and `px` the sample's
/// own x. The slice is an interval, and the convolution of an interval with a
/// Gaussian is exactly `(erf(a) - erf(b)) / 2` — no approximation beyond the
/// `erf` series itself.
///
/// Each side gets its own radius. Two of SphereKit's four corners can differ on
/// the same edge, and a single radius would quietly square off whichever one it
/// did not pick.
fn shadow_slice(px: f32, y: f32, half_extent: vec2<f32>, r: vec4<f32>, sigma: f32) -> f32 {
    let ay = abs(y);
    if (ay >= half_extent.y) {
        return 0.0;
    }
    // `Corners` order is [top_left, top_right, bottom_right, bottom_left], so
    // the pair in play is the top one above the centre line and the bottom one
    // below it.
    var left_r: f32;
    var right_r: f32;
    if (y < 0.0) {
        left_r = r.x;
        right_r = r.y;
    } else {
        left_r = r.w;
        right_r = r.z;
    }
    let limit = min(half_extent.x, half_extent.y);
    left_r = clamp(left_r, 0.0, limit);
    right_r = clamp(right_r, 0.0, limit);

    // How far into the corner arc this slice has climbed, per side. Zero along
    // the straight part of the edge, where the half-width is the full extent.
    let into_left = max(ay - (half_extent.y - left_r), 0.0);
    let into_right = max(ay - (half_extent.y - right_r), 0.0);
    let left = -(half_extent.x - left_r
        + sqrt(max(left_r * left_r - into_left * into_left, 0.0)));
    let right = half_extent.x - right_r
        + sqrt(max(right_r * right_r - into_right * into_right, 0.0));

    let k = 1.0 / (sigma * 1.4142135623730951);
    let e = erf2(vec2<f32>((px - left) * k, (px - right) * k));
    return clamp((e.x - e.y) * 0.5, 0.0, 1.0);
}

/// Coverage of a Gaussian-blurred rounded box at `p`, relative to its centre.
///
/// Returns `0..=1`: one deep inside the shape, one half exactly on a straight
/// edge, and falling to nothing by three sigma outside it.
fn shadow_coverage(half_extent: vec2<f32>, r: vec4<f32>, p: vec2<f32>, sigma: f32) -> f32 {
    // Below a fraction of a pixel there is nothing to integrate and the answer
    // is the sharp shape, antialiased the same way every other quad is.
    if (sigma < 0.02) {
        return coverage_from_distance(sd_rounded_box(p, half_extent, r));
    }
    if (half_extent.x <= 0.0 || half_extent.y <= 0.0) {
        return 0.0;
    }

    let reach = 3.0 * sigma;
    let low = max(p.y - reach, -half_extent.y);
    let high = min(p.y + reach, half_extent.y);
    if (high <= low) {
        return 0.0;
    }

    let step = (high - low) / f32(SHADOW_SLICES);
    var total = 0.0;
    for (var i = 0; i < SHADOW_SLICES; i = i + 1) {
        let y = low + (f32(i) + 0.5) * step;
        total = total + shadow_slice(p.x, y, half_extent, r, sigma)
            * shadow_gaussian(p.y - y, sigma)
            * step;
    }
    // Three sigma leaves 0.3 % of the kernel unaccounted for, so a point deep
    // inside a large box lands on 0.997 rather than 1.0. Scaling it back up is
    // one multiply and removes a seam that would otherwise show where a solid
    // shadow meets the shape it was cast from.
    return clamp(total * 1.0027, 0.0, 1.0);
}
