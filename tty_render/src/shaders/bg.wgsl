// Instanced per-cell background fill. One instance per grid cell, carrying only
// a packed color. The six quad corners come from the vertex index and the cell
// coordinate from the instance index, so nothing but the color is uploaded.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    cursor_corners_01: vec4<f32>,
    cursor_corners_23: vec4<f32>,
    scroll_y: f32,
    // Occluder count the cell fragment shader loops over, and the flag that
    // bypasses the seq test. Both non-zero only on an occludable pool composite,
    // so the live cell fill and the cursor leave panel_count zero and never loop.
    panel_count: u32,
    occlude_all: u32,
    // Grid width, which vs_main divides the instance index by to recover the
    // cell coordinate. The cursor draw carries no grid and leaves it zero.
    cols: u32,
    // Rows the instance buffer is rotated by, and the grid height that rotation
    // wraps at. Display row r lives at slot (r + row_offset) % rows, so a
    // scrolled frame moves this number rather than re-uploading every cell.
    row_offset: u32,
    rows: u32,
    // Cell the grid's own (0, 0) is drawn at, which vs_main adds to every cell
    // coordinate. A pool composite hands over a grid sized to its region, so this
    // is what puts its cells on the screen. Zero for the live grid. Also keeps
    // cursor_color 16-byte aligned.
    origin_cells: vec2<f32>,
    cursor_color: vec4<f32>,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// The live modal boxes. Read only by the cell fragment shader on a pool
// composite, where occlude_all is set, so a pooled cell inside any box is
// discarded whatever its seq.
@group(0) @binding(1)
var<storage, read> occluders: array<Occluder>;

// True when the fragment at `frag` (physical px) lies inside a box that hides
// it. With occlude_all set, any panel rect hides a pooled cell regardless of
// seq; a zero panel_count (the live fill and the cursor) skips the loop.
fn occluded(frag: vec2<f32>) -> bool {
    for (var j = 0u; j < globals.panel_count; j = j + 1u) {
        let o = occluders[j];
        if globals.occlude_all == 1u {
            let sdf = occluder_sdf(
                frag,
                o.cell,
                o.size,
                globals.cell_size,
                o.corner_radius,
                o.inset_x
            );
            if sdf < -0.5 {
                return true;
            }
        }
    }
    return false;
}

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
    @location(0) color: vec4<f32>,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0)
    );

    // The instance stream is exactly row-major over the grid and both draws start
    // at instance zero, so the coordinate the quad needs is the index itself
    // rather than 8 bytes per cell uploaded to say the same thing.
    //
    // The rows are rotated by row_offset, which is how a scroll costs an
    // integer instead of a full re-upload. Slot s therefore holds the row
    // s - row_offset, wrapped, which is the inverse of where a row is written.
    let height = max(globals.rows, 1u);
    let slot_row = instance_index / globals.cols;
    let row = (slot_row + height - globals.row_offset % height) % height;
    let cell = vec2<f32>(f32(instance_index % globals.cols), f32(row));

    // Snap each cell edge to a whole pixel so consecutive cells share an exact
    // integer boundary and each spans whole pixels, leaving no fractional sliver
    // (the dark seam) between same-color cells. Scroll is added after the snap so
    // smooth scrolling stays fractional and the grid only snaps once it settles.
    let pixel = round((cell + globals.origin_cells + corners[vertex_index]) * globals.cell_size)
        + vec2<f32>(0.0, globals.scroll_y);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color.rgb;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // A composited pane pool's cell beneath a later box is discarded so its
    // background cannot show through the box body. The live cell fill leaves
    // panel_count zero, so this never fires for it.
    if occluded(in.clip.xy) {
        discard;
    }

    return vec4<f32>(in.color, 1.0);
}

// Cursor block. One quad, no instance data: its four corners ride in the
// globals uniform as fractional cell coordinates, so it can sit between cells
// while it eases and need not stay rectangular. Drawn after the glyphs and
// alpha-blended, it tints the cells it covers.

struct CursorVsOut {
    @builtin(position) clip: vec4<f32>,
}

@vertex
fn vs_cursor(@builtin(vertex_index) vertex_index: u32) -> CursorVsOut {
    var corners = array<vec2<f32>, 4>(
        globals.cursor_corners_01.xy,
        globals.cursor_corners_01.zw,
        globals.cursor_corners_23.xy,
        globals.cursor_corners_23.zw
    );
    // Two triangles over [TL, TR, BL, BR], matching vs_main's winding.
    var indices = array<u32, 6>(0u, 1u, 2u, 2u, 1u, 3u);

    let cell = corners[indices[vertex_index]];
    let pixel = cell * globals.cell_size + vec2<f32>(0.0, globals.scroll_y);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: CursorVsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    return out;
}

@fragment
fn fs_cursor() -> @location(0) vec4<f32> {
    return globals.cursor_color;
}
