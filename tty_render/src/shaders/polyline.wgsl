// Stroked-path pass. One instance per segment expands to an oriented quad and
// the fragment stage resolves a capsule SDF, so a diagonal reads smooth at cell
// scale and a zero-length segment reads as a round dot. Endpoints are given in
// cell-fraction units and scaled by the live cell size, so a path tracks font
// zoom like a bar does.
//
// The half-width arrives in the same cell-fraction units and is scaled here
// too, or every stroke would collapse to a sub-pixel hairline. It scales by the
// cell's width alone rather than per axis: the SDF measures euclidean distance,
// so one scalar is what keeps a dot round and a diagonal evenly thick.
//
// Unlike bar.wgsl this never snaps to whole pixels. Snapping each vertex of a
// diagonal would move the two ends by different amounts and make the line
// wobble as it scrolls, so the pass follows minimap.wgsl and passes pixel
// coordinates through untouched, leaving the SDF to anti-alias the edge.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    panel_count: u32,
    // 1 discards a fragment inside any occluder regardless of seq, for a pool
    // composite that sits under every box; 0 keeps the seq test.
    occlude_all: u32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// One rect per live modal box, in whole-cell units, plus its declaration-order
// seq. A path fragment is discarded inside any occluder whose seq exceeds the
// path's own, so a box hides the chrome beneath its body.
struct Occluder {
    cell: vec2<f32>,
    size: vec2<f32>,
    seq: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(1)
var<storage, read> occluders: array<Occluder>;

// Pixels the quad is grown by past the stroke, giving the SDF room to ramp
// coverage to zero instead of clipping the edge at the quad boundary.
const AA_MARGIN: f32 = 1.0;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) color: vec3<f32>,
    @location(1) @interpolate(flat) seq: u32,
    // The segment in pixels, carried to the fragment stage so it can measure
    // its own distance to the capsule's spine.
    @location(2) @interpolate(flat) p0: vec2<f32>,
    @location(3) @interpolate(flat) p1: vec2<f32>,
    // Also in pixels, so the fragment stage can use it as the capsule radius
    // directly.
    @location(4) @interpolate(flat) half_width: f32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) p0_cells: vec2<f32>,
    @location(1) p1_cells: vec2<f32>,
    @location(2) half_width: f32,
    @location(3) color: vec3<f32>,
    @location(4) seq: u32,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0)
    );
    let corner = corners[vertex_index];

    let p0 = p0_cells * globals.cell_size;
    let p1 = p1_cells * globals.cell_size;
    let half_width_px = half_width * globals.cell_size.x;

    // A zero-length segment has no direction to orient by, so it falls back to
    // the axis-aligned square that bounds its cap. The SDF still resolves that
    // to a disc, which is how a single-point path draws a dot.
    let span = p1 - p0;
    let length = max(length(span), 0.0001);
    var along = vec2<f32>(1.0, 0.0);
    if length > 0.0001 {
        along = span / length;
    }
    let across = vec2<f32>(-along.y, along.x);

    let reach = half_width_px + AA_MARGIN;
    let center = (p0 + p1) * 0.5;
    let half_span = length * 0.5 + reach;
    let pixel = center + along * (corner.x * half_span) + across * (corner.y * reach);

    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.seq = seq;
    out.p0 = p0;
    out.p1 = p1;
    out.half_width = half_width_px;
    return out;
}

// Coverage from a signed distance in pixels, a ~1px anti-aliased edge.
fn coverage(sdf: f32) -> f32 {
    return clamp(0.5 - sdf, 0.0, 1.0);
}

// Signed distance from `q` to the capsule of radius `r` around segment `a`-`b`.
// Projecting onto the segment and clamping to its ends is what rounds the caps,
// and a zero-length segment degenerates to a disc for free.
fn capsule_sdf(q: vec2<f32>, a: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let span = b - a;
    let denom = max(dot(span, span), 0.0001);
    let t = clamp(dot(q - a, span) / denom, 0.0, 1.0);
    return distance(q, a + span * t) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Discard where a box declared later (higher seq) covers this path, so a
    // line beneath a modal cannot show through it.
    let frag = in.clip.xy;
    for (var j = 0u; j < globals.panel_count; j = j + 1u) {
        let o = occluders[j];
        if globals.occlude_all == 1u || o.seq > in.seq {
            let box_min = o.cell * globals.cell_size;
            let box_max = (o.cell + o.size) * globals.cell_size;
            if frag.x >= box_min.x && frag.x < box_max.x && frag.y >= box_min.y
                && frag.y < box_max.y {
                discard;
            }
        }
    }

    let alpha = coverage(capsule_sdf(frag, in.p0, in.p1, in.half_width));
    if alpha <= 0.0 {
        discard;
    }
    return vec4<f32>(in.color, alpha);
}
