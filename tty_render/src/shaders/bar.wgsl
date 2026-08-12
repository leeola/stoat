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
    // Cell fractions every origin is shifted down by, so a gliding pool moves
    // its bars without rebuilding them. Zero for the live grid.
    shift_rows: f32,
    pad0: u32,
    // Cell the grid's own (0, 0) is drawn at, which vs_main adds before the
    // pixel conversion. A pool composite is positioned within its region, so
    // this is what puts it on the screen. Zero for the live grid.
    origin_cells: vec2<f32>,
    pad1: vec2<u32>,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// The live modal boxes. A bar fragment is discarded inside any occluder whose
// seq exceeds the bar's own, so a box hides the lower chrome beneath its body.
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
    // with the cell grid beneath it. Cell size is fractional at most font sizes,
    // so an unsnapped bar drifts up to a pixel off its row. Each edge is floored
    // a pixel apart so a sub-pixel bar (the hairline separator is 1/16 of a
    // cell) never rounds away to nothing.
    //
    // The glide is added after the snap, which is the ordering bg.wgsl uses for
    // its own scroll. Rounding the shifted origin instead crosses pixel
    // boundaries at a different phase than the rows the bars annotate, so a
    // gliding pool's hairlines wobble a pixel against their content and settle
    // only once it stops.
    let shift_px = vec2<f32>(0.0, globals.shift_rows * globals.cell_size.y);
    let min_px = round((origin + globals.origin_cells) * globals.cell_size) + shift_px;
    let max_px = max(
        round((origin + globals.origin_cells + size) * globals.cell_size) + shift_px,
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

    return vec4<f32>(in.color, 1.0);
}
