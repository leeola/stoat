// Hand-drawn mark pass. One instance draws one whole sketch, or one convex
// fill, and the fragment stage resolves a signed distance so the wobble reads
// smooth at any font size.
//
// The whole mark resolves in one fragment, taking the nearest of the revealed
// segments of every stroke it carries, for the reason polyline.wgsl gives: two
// capsules meeting at a shared endpoint composite their anti-aliased fringes
// twice, and half coverage over half coverage reads three quarters, which beads
// every joint. A mark's base pass and the overlay that doubles it run the same
// path a pixel apart, so drawing each as its own instance doubles the blend
// along the mark's whole length rather than only at a joint.
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

// Every stroke's points, end to end, in physical pixels. A span names its own
// run with point_offset and reveal_count.
@group(0) @binding(2)
var<storage, read> points: array<vec2<f32>>;

// One revealed stroke. Its own box is here rather than on the instance so a
// fragment skips the strokes whose ink is nowhere near it, which is what keeps
// a per-mark quad from costing every fragment every stroke.
struct Span {
    bounds: vec4<f32>,
    point_offset: u32,
    // Whole points of the stroke that are revealed. The pen sits between the
    // last two, at reveal_t along that final segment.
    reveal_count: u32,
    reveal_t: f32,
    pad: u32,
}

// One entry per revealed stroke, in mark order. An instance names its own run
// with span_first and span_count.
@group(0) @binding(3)
var<storage, read> spans: array<Span>;

// Pixels the quad is grown by past the mark, giving the distance field room to
// ramp coverage to zero instead of clipping the edge at the quad boundary.
const AA_MARGIN: f32 = 1.0;

const KIND_STROKE: u32 = 0u;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
    @location(1) @interpolate(flat) seq: u32,
    // Half a stroke's width, or the corner radius a fill grows its inset quad
    // back out by.
    @location(2) @interpolate(flat) half_width: f32,
    // This mark's run of spans. A fill carries a span_count of zero and names
    // the first of its four inset corners in the point buffer with span_first.
    @location(3) @interpolate(flat) span_first: u32,
    @location(4) @interpolate(flat) span_count: u32,
    @location(5) @interpolate(flat) kind: u32,
    // Pixels this mark rides down by. The quad moves in vs_main, so the
    // fragment stage has to measure its distance fields at the same offset or
    // the ink stays behind while its box slides off it.
    @location(6) @interpolate(flat) dy: f32,
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

    // The quad bounds the whole mark rather than one stroke or one segment,
    // because a wobbling path has no single direction to orient a tight box to.
    // The distance field clips the corners the box adds anyway.
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
    out.span_first = span.x;
    out.span_count = span.z;
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

// Signed distance from `q` to the convex quad `a`,`b`,`c`,`d`, wound in order
// and grown by `r`, which rounds its corners with that radius.
//
// The quad is a fill's inset body, so the rounded shape reaches `r` past it,
// which is what subtracting `r` states. The distance to the nearest edge
// segment is exact outside a corner, where the largest half-plane distance
// falls short, and the half-plane test decides the sign.
fn rounded_quad_sdf(
    q: vec2<f32>,
    a: vec2<f32>,
    b: vec2<f32>,
    c: vec2<f32>,
    d: vec2<f32>,
    r: f32,
) -> f32 {
    var corners = array<vec2<f32>, 4>(a, b, c, d);
    var inside = -1.0e9;
    var nearest = 1.0e9;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let a0 = corners[i];
        let a1 = corners[(i + 1u) % 4u];
        let edge = a1 - a0;
        let len = max(length(edge), 0.0001);
        // The outward normal of a clockwise-wound quad in screen space, where y
        // grows downward.
        let normal = vec2<f32>(edge.y, -edge.x) / len;
        inside = max(inside, dot(q - a0, normal));
        let t = clamp(dot(q - a0, edge) / max(dot(edge, edge), 0.0001), 0.0, 1.0);
        nearest = min(nearest, distance(q, a0 + edge * t));
    }
    return select(nearest, -nearest, inside <= 0.0) - r;
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
        sdf = 1.0e9;
        let reach = in.half_width + AA_MARGIN;

        // The nearest revealed segment of the whole mark decides the coverage,
        // so the mark blends once no matter how many strokes and joints meet at
        // the fragment.
        for (var s = 0u; s < in.span_count; s = s + 1u) {
            let span = spans[in.span_first + s];

            // A stroke whose own box does not reach this fragment cannot hold
            // the nearest ink, so its segments are never walked. Without this a
            // mark's quad costs every fragment every stroke the mark carries.
            if at.x < span.bounds.x - reach || at.x > span.bounds.z + reach
                || at.y < span.bounds.y - reach || at.y > span.bounds.w + reach {
                continue;
            }

            let base = span.point_offset;
            for (var i = 0u; i + 1u < span.reveal_count; i = i + 1u) {
                sdf = min(
                    sdf,
                    capsule_sdf(at, points[base + i], points[base + i + 1u], in.half_width)
                );
            }
            // The pen tip sits partway along the segment after the revealed run,
            // so the stroke grows smoothly instead of snapping point to point.
            if span.reveal_t > 0.0 {
                let last = base + span.reveal_count - 1u;
                let tip = mix(points[last], points[last + 1u], span.reveal_t);
                sdf = min(sdf, capsule_sdf(at, points[last], tip, in.half_width));
            }
        }
    } else {
        let base = in.span_first;
        sdf = rounded_quad_sdf(
            at,
            points[base],
            points[base + 1u],
            points[base + 2u],
            points[base + 3u],
            in.half_width
        );
    }

    let alpha = coverage(sdf) * in.color.a;
    if alpha <= 0.0 {
        discard;
    }
    return vec4<f32>(in.color.rgb, alpha);
}
