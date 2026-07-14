// Minimap pass. One instance per run block, plus a background and a viewport
// thumb quad, filling the right-edge minimap strip. Unlike the bar pass, a
// minimap quad is given in absolute pixels rather than cell-fraction units,
// since a minimap column is a fraction of a pixel and the cell grid is too
// coarse. The globals still carry the cell size so the occluder test can map a
// panel's cell rect to pixels.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    panel_count: u32,
    // 1 discards a fragment inside any occluder regardless of seq; 0 keeps the
    // seq test. The minimap never composites under a pool, so it always passes 0.
    occlude_all: u32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// One rect per live modal box, in whole-cell units, plus its declaration-order
// seq. A minimap fragment is discarded inside any occluder whose seq exceeds the
// strip's own, so a box hides the strip beneath its body.
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

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
    @location(1) @interpolate(flat) seq: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) seq: u32,
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

    let pixel = origin + corner * size;
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.seq = seq;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Discard where a box declared later (higher seq) covers this strip.
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

    return in.color;
}
