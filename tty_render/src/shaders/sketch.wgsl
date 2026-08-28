// Hand-drawn mark pass. One instance draws one stroke of one sketch, or one
// convex fill, and the fragment stage resolves a signed distance so the wobble
// reads smooth at any font size.
//
// The whole stroke resolves in one fragment, taking the nearest of its
// revealed segments, for the reason polyline.wgsl gives: two capsules meeting
// at a shared endpoint composite their anti-aliased fringes twice, and half
// coverage over half coverage reads three quarters, which beads every joint. A
// generated stroke has dozens of joints, so the bead becomes the whole look.
//
// The points live in a storage buffer rather than the instance. polyline.wgsl
// packs its twelve inline because one bind group there serves the live grid and
// every composited pool. A sketch never draws on a pool, so one buffer with no
// frame boundary to reset against is exactly what this pass can use, and a
// generated stroke has far more points than an instance can carry.
//
// Coordinates arrive already in physical pixels, because the wobble was
// generated against the live cell size. Nothing here scales, and nothing snaps
// to whole pixels. Snapping the ends of a wobbling curve by different amounts
// makes it crawl as it scrolls.

struct Globals {
    resolution: vec2<f32>,
    // Occluders arrive in whole-cell units, so hiding a mark under a box needs
    // the live cell rectangle even though nothing else in this pass does.
    cell_size: vec2<f32>,
    panel_count: u32,
    // Three scalars rather than a vec3, whose 16-byte alignment would push the
    // struct to 48 bytes where the Rust side is 32.
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// The live modal boxes. A mark fragment is discarded inside any occluder whose
// seq exceeds the mark's own, so a box hides the chrome beneath its body.
@group(0) @binding(1)
var<storage, read> occluders: array<Occluder>;

// Every stroke's points, end to end, in physical pixels. An instance names its
// own span with point_offset and reveal_count.
@group(0) @binding(2)
var<storage, read> points: array<vec2<f32>>;

// Pixels the quad is grown by past the mark, giving the distance field room to
// ramp coverage to zero instead of clipping the edge at the quad boundary.
const AA_MARGIN: f32 = 1.0;

const KIND_STROKE: u32 = 0u;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
    @location(1) @interpolate(flat) seq: u32,
    @location(2) @interpolate(flat) half_width: f32,
    @location(3) @interpolate(flat) point_offset: u32,
    // Whole points of the stroke that are revealed. The pen sits between the
    // last two, at reveal_t along that final segment.
    @location(4) @interpolate(flat) reveal_count: u32,
    @location(5) @interpolate(flat) reveal_t: f32,
    @location(6) @interpolate(flat) kind: u32,
    // Pixels this mark rides down by. The quad moves in vs_main, so the
    // fragment stage has to measure its distance fields at the same offset or
    // the ink stays behind while its box slides off it.
    @location(7) @interpolate(flat) dy: f32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) bounds: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) width_seq: vec4<f32>,
    @location(3) span: vec4<u32>,
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

    let half_width_px = width_seq.x;
    let dy = width_seq.z;

    // The quad bounds the whole stroke rather than one segment, because a
    // wobbling path has no single direction to orient a tight box to. The
    // distance field clips the corners the box adds anyway.
    let reach = vec2<f32>(half_width_px + AA_MARGIN, half_width_px + AA_MARGIN);
    let shift = vec2<f32>(0.0, dy);
    let min_px = bounds.xy + shift - reach;
    let max_px = bounds.zw + shift + reach;
    let pixel = mix(min_px, max_px, corner);

    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.seq = span.y;
    out.half_width = half_width_px;
    out.point_offset = span.x;
    out.reveal_count = span.z;
    out.reveal_t = width_seq.y;
    out.kind = span.w;
    out.dy = dy;
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

// Signed distance from `q` to the convex quad `a`,`b`,`c`,`d`, wound in order.
// Each edge's outward half-plane distance is taken, and the largest of the four
// is the distance to the shape: negative inside every edge, positive outside
// any one of them.
fn quad_sdf(q: vec2<f32>, a: vec2<f32>, b: vec2<f32>, c: vec2<f32>, d: vec2<f32>) -> f32 {
    var corners = array<vec2<f32>, 4>(a, b, c, d);
    var worst = -1.0e9;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let a0 = corners[i];
        let a1 = corners[(i + 1u) % 4u];
        let edge = a1 - a0;
        let len = max(length(edge), 0.0001);
        // The outward normal of a clockwise-wound quad in screen space, where y
        // grows downward.
        let normal = vec2<f32>(edge.y, -edge.x) / len;
        worst = max(worst, dot(q - a0, normal));
    }
    return worst;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Discard where a box declared later (higher seq) covers this mark, so a
    // sketch beneath a modal cannot show through it.
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

    // The points were generated at the mark's rest position, so a ridden mark
    // is measured against the fragment pulled back by the same shift its quad
    // was pushed forward by.
    let at = frag - vec2<f32>(0.0, in.dy);

    var sdf: f32;
    if in.kind == KIND_STROKE {
        // Nothing is revealed until the pen has left the first point.
        if in.reveal_count < 2u && in.reveal_t <= 0.0 {
            discard;
        }

        let base = in.point_offset;
        sdf = 1.0e9;
        // The nearest revealed segment decides the coverage, so the stroke
        // blends once no matter how many of its joints meet at the fragment.
        for (var i = 0u; i + 1u < in.reveal_count; i = i + 1u) {
            sdf = min(
                sdf,
                capsule_sdf(at, points[base + i], points[base + i + 1u], in.half_width)
            );
        }
        // The pen tip sits partway along the segment after the revealed run, so
        // the stroke grows smoothly instead of snapping point to point.
        if in.reveal_t > 0.0 {
            let last = base + in.reveal_count - 1u;
            let tip = mix(points[last], points[last + 1u], in.reveal_t);
            sdf = min(sdf, capsule_sdf(at, points[last], tip, in.half_width));
        }
    } else {
        let base = in.point_offset;
        sdf = quad_sdf(
            at,
            points[base],
            points[base + 1u],
            points[base + 2u],
            points[base + 3u]
        );
    }

    let alpha = coverage(sdf) * in.color.a;
    if alpha <= 0.0 {
        discard;
    }
    return vec4<f32>(in.color.rgb, alpha);
}
