// Status icon pass. One instance per icon draws a quad over a size-by-size cell
// block. The fragment paints a signed-distance shape per kind in the icon color
// and nothing elsewhere, so it alpha-blends over whatever it sits on.
//
// Each kind is an outline with its glyph cut out of it, the way a status icon
// is drawn in an icon set: an error is a disc crossed out, a warning is a
// rounded triangle holding a bang, and info is a disc holding an i. A filled
// silhouette carries none of that meaning, and a reader tells one apart from
// the next by its glyph rather than by its outline.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    panel_count: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// The live modal boxes. An icon fragment is discarded inside any occluder whose
// seq exceeds the icon's own, so a box hides the lower chrome beneath its body.
@group(0) @binding(1)
var<storage, read> occluders: array<Occluder>;

const KIND_ERROR: u32 = 0u;
const KIND_WARNING: u32 = 1u;
const KIND_INFO: u32 = 2u;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) extent: vec2<f32>,
    @location(2) @interpolate(flat) color: vec3<f32>,
    @location(3) @interpolate(flat) kind: u32,
    @location(4) @interpolate(flat) seq: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) cell: vec2<f32>,
    @location(1) size: f32,
    @location(2) color: vec3<f32>,
    @location(3) kind: u32,
    @location(4) offset: vec2<f32>,
    @location(5) seq: u32,
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

    let extent = vec2<f32>(size, size) * globals.cell_size;
    let pixel = cell * globals.cell_size + offset + corner * extent;
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.local = corner;
    out.extent = extent;
    out.color = color;
    out.kind = kind;
    out.seq = seq;
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

// Signed distance to the equilateral triangle of size `r` centered at the
// origin, apex up. `q.y` is negated first, because the pass works in screen
// space where y grows downward.
//
// The exact distance matters at the three vertices. Taking the max of the edge
// half-planes under-estimates it there, which widens the anti-aliasing ramp and
// bulges each tip.
fn triangle_sdf(q: vec2<f32>, r: f32) -> f32 {
    let k = sqrt(3.0);
    var p = vec2<f32>(q.x, -q.y);
    p.x = abs(p.x) - r;
    p.y = p.y + r / k;
    if p.x + k * p.y > 0.0 {
        p = vec2<f32>(p.x - k * p.y, -k * p.x - p.y) / 2.0;
    }
    p.x = p.x - clamp(p.x, -2.0 * r, 0.0);
    return -length(p) * sign(p.y);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Discard where a box declared later (higher seq) covers this icon, so a
    // lower box's status icon cannot show through an upper box.
    let frag = in.clip.xy;
    for (var j = 0u; j < globals.panel_count; j = j + 1u) {
        let o = occluders[j];
        if o.seq > in.seq {
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

    let center = in.extent * 0.5;
    let q = in.local * in.extent - center;
    let r = min(center.x, center.y) * 0.9;
    // One physical pixel is the thinnest a glyph stroke reads at, so a small
    // icon holds its glyph rather than closing it up.
    let stroke = max(0.1 * r, 0.75);

    var body: f32;
    var cutout: f32;
    if in.kind == KIND_WARNING {
        let round = 0.12 * r;
        body = triangle_sdf(q, r - round) - round;
        let bar = capsule_sdf(q, vec2<f32>(0.0, -0.25 * r), vec2<f32>(0.0, 0.2 * r), stroke);
        let dot = capsule_sdf(q, vec2<f32>(0.0, 0.5 * r), vec2<f32>(0.0, 0.5 * r), stroke);
        cutout = min(bar, dot);
    } else if in.kind == KIND_INFO {
        body = length(q) - r;
        let dot = capsule_sdf(q, vec2<f32>(0.0, -0.4 * r), vec2<f32>(0.0, -0.4 * r), stroke);
        let bar = capsule_sdf(q, vec2<f32>(0.0, -0.1 * r), vec2<f32>(0.0, 0.45 * r), stroke);
        cutout = min(dot, bar);
    } else {
        body = length(q) - r;
        let arm = 0.42 * r;
        let down = capsule_sdf(q, vec2<f32>(-arm, -arm), vec2<f32>(arm, arm), stroke);
        let up = capsule_sdf(q, vec2<f32>(-arm, arm), vec2<f32>(arm, -arm), stroke);
        cutout = min(down, up);
    }

    // Subtracting the glyph leaves the ground showing through it, so the icon
    // reads as a mark rather than as a blob of color.
    return vec4<f32>(in.color, coverage(max(body, -cutout)));
}
