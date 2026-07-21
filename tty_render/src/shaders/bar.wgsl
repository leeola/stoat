// Color-bar pass. One instance per bar fills a sub-cell rectangle in a solid
// color, off the cell grid, so a gutter can pack thin status bars and a hairline
// separator into a fraction of a cell. The rectangle is given in cell-fraction
// units and scaled by the live cell size, so it tracks font zoom.

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
// seq. A bar fragment is discarded inside any occluder whose seq exceeds the
// bar's own, so a box hides the lower chrome beneath its body.
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
    @location(0) @interpolate(flat) color: vec3<f32>,
    @location(1) @interpolate(flat) seq: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec3<f32>,
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

    // Snap both edges to whole pixels so a bar shares exact integer boundaries
    // with the cell grid beneath it, which bg.wgsl snaps the same way. Cell size
    // is fractional at most font sizes, so an unsnapped bar drifts up to a pixel
    // off its row. Each edge is floored a pixel apart so a sub-pixel bar (the
    // hairline separator is 1/16 of a cell) never rounds away to nothing.
    let min_px = round(origin * globals.cell_size);
    let max_px = max(
        round((origin + size) * globals.cell_size),
        min_px + vec2<f32>(1.0, 1.0)
    );
    let pixel = min_px + corner * (max_px - min_px);
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
    // Discard where a box declared later (higher seq) covers this bar, so a
    // gutter hairline or a lower box's bar cannot show through an upper box.
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

    return vec4<f32>(in.color, 1.0);
}
