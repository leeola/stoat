// Shared occlusion geometry, prepended to every pass that hides fragments under
// a modal box. WGSL has no include, so the passes would otherwise each carry
// their own copy and drift apart on the shape they occlude by.
//
// A panel is not the rect it declares. It draws rounded corners and shaves
// `inset_x` off each side, so a pass that hides fragments by the bare rect
// punches a square notch out of whatever sits beneath each corner.

// One rect per live modal box, in whole-cell units, plus the corner radius and
// horizontal inset the panel actually draws with and its declaration-order seq.
struct Occluder {
    cell: vec2<f32>,
    size: vec2<f32>,
    seq: u32,
    corner_radius: f32,
    inset_x: f32,
    pad0: u32,
}

// Signed distance to a rounded rectangle of half-size `half` and corner radius
// `r` centered at the origin, negative inside.
fn rounded_box_sdf(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

// Signed distance from `frag`, in physical pixels, to the body a box actually
// draws: its cell rect shaved by `inset_x` on each x edge and rounded by
// `corner_radius`. Negative inside.
//
// Callers discard at -0.5 rather than 0, which leaves the outer half-pixel ring
// standing so the box's own anti-aliased edge blends over the chrome beneath it
// instead of over a hard hole.
fn occluder_sdf(
    frag: vec2<f32>,
    cell: vec2<f32>,
    size: vec2<f32>,
    cell_size: vec2<f32>,
    corner_radius: f32,
    inset_x: f32,
) -> f32 {
    let box_min = cell * cell_size + vec2<f32>(inset_x, 0.0);
    let box_max = (cell + size) * cell_size - vec2<f32>(inset_x, 0.0);
    let center = (box_min + box_max) * 0.5;
    let half = (box_max - box_min) * 0.5;
    let radius = min(corner_radius, min(half.x, half.y));
    return rounded_box_sdf(frag - center, half, radius);
}
