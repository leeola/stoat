// Status icon pass. One instance per icon draws a quad over a size-by-size cell
// block. The fragment paints a signed-distance shape per kind in the icon color
// and nothing elsewhere, so it alpha-blends over whatever it sits on.
//
// Each kind is a severity mark the boards draw. An error is a filled disc. A
// warning is a rounded triangle with a bang cut out of it, and info is a disc
// with an i cut out of it.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    panel_count: u32,
    // 1 discards a fragment inside any occluder regardless of seq, for a pool
    // composite that sits under every box. 0 keeps the seq test.
    occlude_all: u32,
    // The band a line of text occupies in its cell: the baseline offset from the
    // cell's top, and the height of a capital above it, both in physical pixels.
    // An icon is placed against these rather than against the cell box, which
    // the font's ascent and descent do not fill symmetrically.
    baseline: f32,
    cap_height: f32,
    // Rows every icon is shifted down by, so a gliding pool moves its icons
    // without rebuilding them. Zero for the live grid.
    shift_rows: f32,
    pad0: u32,
    // Cell the grid's own (0, 0) is drawn at, which vs_main adds to each
    // anchor. A pool composite is positioned within its region, so this is what
    // puts it on the screen. Zero for the live grid.
    origin_cells: vec2<f32>,
    // The cell size the occluder rects scale by, which is the live grid's. A
    // pool composited at its own font size lays its icons out at cell_size, but
    // the boxes over it are cells of the live grid.
    occluder_cell: vec2<f32>,
    // The pixel y the last row's top paints at, and that row, which rows.wgsl
    // places the icon rows and the occluder rows by.
    bottom_row_y: f32,
    last_row: u32,
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

// A cut-out distance that no pixel reaches, which leaves a body whole.
const NO_CUTOUT: f32 = 1.0e9;

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
    // An icon above the last row ends where that row starts. The local coordinate
    // scales with the cut, so the shapes stay measured against the whole block.
    // A glide moves the icon and that boundary alike.
    let at = cell + globals.origin_cells;
    let shift = globals.shift_rows * globals.cell_size.y;
    let origin = cell_px(at) + offset + vec2<f32>(0.0, shift);
    let cut = clamp_bottom(origin.y, extent.y, row_clip_y(at.y + size) + shift);
    let pixel = vec2<f32>(
        origin.x + corner.x * extent.x,
        select(origin.y, cut.x, corner.y > 0.5)
    );
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.local = vec2<f32>(corner.x, corner.y * cut.y);
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
        if globals.occlude_all == 1u || o.seq > in.seq {
            let sdf = occluder_sdf(
                frag,
                o.cell,
                o.size,
                globals.occluder_cell,
                o.corner_radius,
                o.inset_x
            );
            if sdf < -0.5 {
                discard;
            }
        }
    }

    // Centered on the capitals rather than on the cell box, and sized by their
    // height, so an icon reads as a mark on the line of text beside it. A block
    // taller than one cell keeps that placement at its own middle.
    let center = vec2<f32>(
        in.extent.x * 0.5,
        globals.baseline - globals.cap_height * 0.5
            + (in.extent.y - globals.cell_size.y) * 0.5
    );
    let q = in.local * in.extent - center;
    // The cap height sets the size, and the quad bounds it, so a narrow cell
    // holds the icon rather than letting it spill past its own block.
    let r = min(
        globals.cap_height * 0.6 * (in.extent.y / globals.cell_size.y),
        min(center.x, center.y) * 0.95
    );
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
        cutout = NO_CUTOUT;
    }

    // Subtracting the glyph leaves the ground showing through it.
    return vec4<f32>(in.color, coverage(max(body, -cutout)));
}
