// Status icon pass. One instance per icon draws a quad over a size-by-size cell
// block; the fragment paints a signed-distance silhouette per kind -- a disc for
// error, an upward triangle for warning, a square for info -- in the icon color
// and transparent elsewhere, so it alpha-blends over whatever it sits on.

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

// One rect per live modal box, in whole-cell units, plus its declaration-order
// seq. An icon fragment is discarded inside any occluder whose seq exceeds the
// icon's own, so a box hides the lower chrome beneath its body.
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

// Signed distance to the upward triangle inscribed in the radius-`r` disc
// centered at the origin, as the max of its three edge half-planes (apex at the
// top, base across the bottom). The slant normals are (+-2, -1)/sqrt(5).
fn triangle_sdf(q: vec2<f32>, r: f32) -> f32 {
    let bottom = q.y - r;
    let left = -0.894427 * q.x - 0.447214 * (q.y + r);
    let right = 0.894427 * q.x - 0.447214 * (q.y + r);
    return max(bottom, max(left, right));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Discard where a box declared later (higher seq) covers this icon, so a
    // lower box's status icon cannot show through an upper box.
    let frag = in.clip.xy;
    for (var j = 0u; j < globals.panel_count; j = j + 1u) {
        let o = occluders[j];
        if o.seq > in.seq {
            let box_min = o.cell * globals.cell_size;
            let box_max = (o.cell + o.size) * globals.cell_size;
            if frag.x >= box_min.x && frag.x < box_max.x && frag.y >= box_min.y
                && frag.y < box_max.y {
                discard;
            }
        }
    }

    let center = in.extent * 0.5;
    let q = in.local * in.extent - center;
    let r = min(center.x, center.y) * 0.9;

    var sdf: f32;
    if in.kind == KIND_WARNING {
        sdf = triangle_sdf(q, r);
    } else if in.kind == KIND_INFO {
        sdf = max(abs(q.x), abs(q.y)) - r * 0.82;
    } else {
        sdf = length(q) - r;
    }

    return vec4<f32>(in.color, coverage(sdf));
}
