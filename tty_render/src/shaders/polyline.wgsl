// Stroked-path pass. One instance per path expands to the quad bounding it and
// the fragment stage resolves a capsule SDF, so a diagonal reads smooth at cell
// scale and a zero-length segment reads as a round dot. Endpoints are given in
// cell-fraction units and scaled by the live cell size, so a path tracks font
// zoom like a bar does.
//
// The whole path resolves in one fragment, taking the nearest of its segments,
// because two capsules meeting at a shared endpoint overlap and composite their
// anti-aliased fringes twice. Half coverage over half coverage reads three
// quarters, which beads every joint.
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
    // Cell fractions every endpoint is shifted down by, so a gliding pool moves
    // its paths without rebuilding them. Zero for the live grid.
    shift_rows: f32,
    pad0: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// The live modal boxes. A path fragment is discarded inside any occluder whose
// seq exceeds the path's own, so a box hides the chrome beneath its body.
@group(0) @binding(1)
var<storage, read> occluders: array<Occluder>;

// Pixels the quad is grown by past the stroke, giving the SDF room to ramp
// coverage to zero instead of clipping the edge at the quad boundary.
const AA_MARGIN: f32 = 1.0;

// Points one instance carries, matching MAX_PATH_POINTS on the Rust side. Slots
// past the count repeat the last point, so the count alone decides how much of
// the array the fragment stage reads.
const MAX_PATH_POINTS: u32 = 12u;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) color: vec3<f32>,
    @location(1) @interpolate(flat) seq: u32,
    // In pixels, so the fragment stage uses it as the capsule radius directly.
    @location(2) @interpolate(flat) half_width: f32,
    @location(3) @interpolate(flat) point_count: u32,
    // The path's points in pixels, converted once a vertex rather than once a
    // fragment.
    @location(4) @interpolate(flat) p01: vec4<f32>,
    @location(5) @interpolate(flat) p23: vec4<f32>,
    @location(6) @interpolate(flat) p45: vec4<f32>,
    @location(7) @interpolate(flat) p67: vec4<f32>,
    @location(8) @interpolate(flat) p89: vec4<f32>,
    @location(9) @interpolate(flat) p1011: vec4<f32>,
}

// A point pair moved to pixels, which is how each packed vec4 of the instance
// reaches the fragment stage.
fn pair_px(pair: vec4<f32>, shift: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(
        (pair.xy + shift) * globals.cell_size,
        (pair.zw + shift) * globals.cell_size
    );
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) p01_cells: vec4<f32>,
    @location(1) p23_cells: vec4<f32>,
    @location(2) p45_cells: vec4<f32>,
    @location(3) p67_cells: vec4<f32>,
    @location(4) p89_cells: vec4<f32>,
    @location(5) p1011_cells: vec4<f32>,
    @location(6) bounds: vec4<f32>,
    @location(7) color_width: vec4<f32>,
    @location(8) seq_count: vec2<u32>,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0)
    );
    let corner = corners[vertex_index];

    let shift = vec2<f32>(0.0, globals.shift_rows);
    let half_width_px = color_width.w * globals.cell_size.x;

    // The quad bounds the whole path rather than one segment, so it is axis
    // aligned. A path of several segments has no single direction to orient to,
    // and the SDF clips the corners the box adds anyway.
    let reach = vec2<f32>(half_width_px + AA_MARGIN, half_width_px + AA_MARGIN);
    let min_px = (bounds.xy + shift) * globals.cell_size - reach;
    let max_px = (bounds.zw + shift) * globals.cell_size + reach;
    let pixel = mix(min_px, max_px, corner);

    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color_width.xyz;
    out.seq = seq_count.x;
    out.half_width = half_width_px;
    out.point_count = min(seq_count.y, MAX_PATH_POINTS);
    out.p01 = pair_px(p01_cells, shift);
    out.p23 = pair_px(p23_cells, shift);
    out.p45 = pair_px(p45_cells, shift);
    out.p67 = pair_px(p67_cells, shift);
    out.p89 = pair_px(p89_cells, shift);
    out.p1011 = pair_px(p1011_cells, shift);
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
            let sdf = occluder_sdf(
                frag,
                o.cell,
                o.size,
                globals.cell_size,
                o.corner_radius,
                o.inset_x
            );
            if sdf < -0.5 {
                discard;
            }
        }
    }

    // The nearest segment decides the coverage, so the path blends once no
    // matter how many segments meet at the fragment.
    var points = array<vec2<f32>, MAX_PATH_POINTS>(
        in.p01.xy, in.p01.zw,
        in.p23.xy, in.p23.zw,
        in.p45.xy, in.p45.zw,
        in.p67.xy, in.p67.zw,
        in.p89.xy, in.p89.zw,
        in.p1011.xy, in.p1011.zw
    );

    var sdf = capsule_sdf(frag, points[0], points[1], in.half_width);
    for (var i = 1u; i + 1u < in.point_count; i = i + 1u) {
        sdf = min(
            sdf,
            capsule_sdf(frag, points[i], points[i + 1u], in.half_width)
        );
    }

    let alpha = coverage(sdf);
    if alpha <= 0.0 {
        discard;
    }
    return vec4<f32>(in.color, alpha);
}
