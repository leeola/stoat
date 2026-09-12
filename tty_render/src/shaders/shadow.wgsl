// Gaussian drop shadow of a rounded rectangle, shared by the passes that cast
// one. WGSL has no include, so this module is prepended to each pass's source.
//
// A blurred box is the box convolved with a gaussian. Convolution separates, and
// the x direction has a closed form through the error function, so only y is
// sampled. Four samples across the six sigma the kernel spans are enough,
// because what is sampled is already smooth in y.

const M_PI_F: f32 = 3.14159265;

// A standard gaussian, which weights each y sample by its distance.
fn gaussian(x: f32, sigma: f32) -> f32 {
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (sqrt(2.0 * M_PI_F) * sigma);
}

// An approximation of the error function, which the gaussian integral needs.
fn erf(v: vec2<f32>) -> vec2<f32> {
    let s = sign(v);
    let a = abs(v);
    let r1 = 1.0 + (0.278393 + (0.230389 + (0.000972 + 0.078108 * a) * a) * a) * a;
    let r2 = r1 * r1;
    return s - s / (r2 * r2);
}

// Coverage at `x` of one horizontal slice of the rounded rect, blurred along x.
//
// `y` is the slice's offset from the rect's center. Within `corner` of the top
// or bottom the slice is narrower than the rect, by the amount the corner arc
// cuts away, so the blur follows the round corner rather than the square one the
// rect alone would present.
fn blur_along_x(x: f32, y: f32, sigma: f32, corner: f32, half_size: vec2<f32>) -> f32 {
    let delta = min(half_size.y - corner - abs(y), 0.0);
    let curved = half_size.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    let integral = 0.5 + 0.5 * erf((x + vec2<f32>(-curved, curved)) * (sqrt(0.5) / sigma));
    return integral.y - integral.x;
}

// Coverage at `p` of the rounded rect `shadow_min`..`shadow_max` blurred by
// `sigma`, from 1 well inside the rect to 0 three sigma outside it.
//
// The caller scales this by the shadow's own peak opacity. A blurred edge splits
// its coverage evenly across itself, so the rect's own edge carries half that
// peak rather than all of it.
fn shadow_alpha(
    p: vec2<f32>,
    shadow_min: vec2<f32>,
    shadow_max: vec2<f32>,
    radius: f32,
    sigma: f32
) -> f32 {
    let half_size = (shadow_max - shadow_min) * 0.5;
    let center_to_point = p - (shadow_min + shadow_max) * 0.5;

    // The kernel reaches three sigma and the rect bounds the rest, so the
    // samples land where the signal is rather than across the whole quad.
    let low = center_to_point.y - half_size.y;
    let high = center_to_point.y + half_size.y;
    let start = clamp(-3.0 * sigma, low, high);
    let end = clamp(3.0 * sigma, low, high);

    let step_y = (end - start) / 4.0;
    var y = start + step_y * 0.5;
    var alpha = 0.0;
    for (var i = 0; i < 4; i += 1) {
        let slice = blur_along_x(
            center_to_point.x,
            center_to_point.y - y,
            sigma,
            radius,
            half_size
        );
        alpha += slice * gaussian(y, sigma) * step_y;
        y += step_y;
    }
    return alpha;
}
