// Color-bar pass. One instance per bar fills a sub-cell rectangle in a solid
// color, off the cell grid, so a gutter can pack thin status bars and a hairline
// separator into a fraction of a cell. The rectangle is given in cell-fraction
// units and scaled by the cell size of the grid it sits on, so it tracks font
// zoom. A bar with a corner radius rounds the corners its mask names, which
// draws a dot, a pill, or one share of a round-ended meter.

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
    // The display scale, which turns the hatch's logical pixel pitch into
    // framebuffer pixels.
    scale_factor: f32,
    // Cell the grid's own (0, 0) is drawn at, which vs_main adds before the
    // pixel conversion. A pool composite is positioned within its region, so
    // this is what puts it on the screen. Zero for the live grid.
    origin_cells: vec2<f32>,
    // The cell size the occluder rects scale by, which is the live grid's. A
    // pool composited at its own font size lays its bars out at cell_size, but
    // the boxes over it are cells of the live grid.
    occluder_cell: vec2<f32>,
    // The pixel y the last row's top paints at, and that row, which rows.wgsl
    // places the bar edges and the occluder rows by. A pool composite carries a
    // last_row that no row reaches.
    bottom_row_y: f32,
    last_row: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// The live modal boxes. A bar fragment is discarded inside any occluder whose
// seq exceeds the bar's own, so a box hides the lower chrome beneath its body.
@group(0) @binding(1)
var<storage, read> occluders: array<Occluder>;

// The hatch's line pitch in logical pixels, measured along x plus y, which a
// panel's hatch shares so the stripes run on from one to the other.
const HATCH_PITCH: f32 = 7.0;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) color: vec3<f32>,
    @location(1) @interpolate(flat) seq: u32,
    // The hatch color, and 1.0 to stripe the fill with it.
    @location(2) @interpolate(flat) hatch: vec4<f32>,
    // The pixels the bar covers, min then max, which its corners round within.
    @location(3) @interpolate(flat) rect: vec4<f32>,
    // The corner radius in framebuffer pixels, 0 for a square bar.
    @location(4) @interpolate(flat) radius: f32,
    // The corners that round, bit 0 the top-left and then clockwise.
    @location(5) @interpolate(flat) corners: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(3) seq: u32,
    @location(4) hatch: vec4<f32>,
    @location(5) radius: f32,
    @location(6) corner_mask: u32,
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
    //
    // A bar under two pixels takes its width from its declared size instead.
    // The width of a snapped bar is the difference of two roundings, so it
    // moves by a pixel with the phase the bar lands on, and at that size a
    // pixel is half of it: a gutter's marks read at different weights from row
    // to row. Above it the two-edge snap stands, because a fixed width puts a
    // bar's right edge off where its neighbour's left edge snapped to and a
    // tiled run gaps.
    //
    // Both edges go through row_y, so a bar that ends at the top of the last
    // row ends where that row paints, not on the uniform pitch.
    let shift_px = vec2<f32>(0.0, globals.shift_rows * globals.cell_size.y);
    let min_px = round(cell_px(origin + globals.origin_cells)) + shift_px;
    let snapped = max(
        round(cell_px(origin + globals.origin_cells + size)) + shift_px,
        min_px + vec2<f32>(1.0, 1.0)
    );
    let size_px = size * globals.cell_size;
    let held = min_px + max(vec2<f32>(1.0, 1.0), round(size_px));
    let max_px = select(snapped, held, size_px < vec2<f32>(2.0, 2.0));
    let pixel = min_px + corner * (max_px - min_px);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.seq = seq;
    out.hatch = hatch;
    // A gliding pool puts the quad's edges between pixels. The rect is the
    // span of pixel centers the rasterizer covers, so a gliding bar's straight
    // edges stay whole as a square bar's do.
    out.rect = vec4<f32>(ceil(min_px - 0.5), ceil(max_px - 0.5));
    // floor(x + 0.5) rounds a half up as the panel pass rounds its radius, so a
    // bar and a panel at one logical radius round alike at a fractional scale.
    out.radius = floor(radius * globals.scale_factor + 0.5);
    out.corners = corner_mask;
    return out;
}

/// The bar's color at the fragment.
///
/// A hatched bar takes the hatch color on its lines, one logical pixel wide and
/// HATCH_PITCH apart along x plus y, which run at 135 degrees. The lines are
/// placed by the framebuffer pixel less the pool's glide, so they run on
/// unbroken across neighboring bars and travel with a gliding pool's content.
fn bar_color(in: VsOut) -> vec3<f32> {
    let shifted = in.clip.y - globals.shift_rows * globals.cell_size.y;
    let diagonal = (in.clip.x + shifted) / globals.scale_factor;
    let on_line = diagonal - HATCH_PITCH * floor(diagonal / HATCH_PITCH) < 1.0;
    return select(in.color, in.hatch.rgb, in.hatch.w > 0.5 && on_line);
}

/// The share of the fragment's pixel that the bar covers.
///
/// The fragment's quadrant names the corner it lies toward, as occluder_sdf
/// names it, and that corner rounds when the mask holds its bit. The radius is
/// held to the bar's shorter side, not to half of it. A share of a round-ended
/// meter narrower than the meter's radius then still rounds its outer corners
/// at that radius. A square bar covers every pixel whole.
fn bar_coverage(in: VsOut) -> f32 {
    if in.radius <= 0.0 {
        return 1.0;
    }
    let half = (in.rect.zw - in.rect.xy) * 0.5;
    let p = in.clip.xy - (in.rect.xy + in.rect.zw) * 0.5;
    let corner = select(select(0u, 1u, p.x > 0.0), select(3u, 2u, p.x > 0.0), p.y > 0.0);
    let rounded = ((in.corners >> corner) & 1u) == 1u;
    let radius = select(0.0, min(in.radius, 2.0 * min(half.x, half.y)), rounded);
    return clamp(0.5 - rounded_box_sdf(p, half, radius), 0.0, 1.0);
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
                globals.occluder_cell,
                o.corner_radius,
                o.edge_px,
                o.edge_modes,
                o.corners
            );
            if sdf < -0.5 {
                discard;
            }
        }
    }

    return vec4<f32>(bar_color(in), bar_coverage(in));
}
