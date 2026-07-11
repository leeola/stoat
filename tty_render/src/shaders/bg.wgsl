// Instanced per-cell background fill. One instance per grid cell; the six
// quad corners are generated from the vertex index, so the only vertex buffer
// is the per-cell instance stream.

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
    pad3: f32,
    cursor_color: vec4<f32>,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// One rect per live modal box, in whole-cell units, plus its declaration-order
// seq. Read only by the cell fragment shader on a pool composite: occlude_all is
// set there, so a pooled cell inside any box rect is discarded whatever its seq.
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

// True when the fragment at `frag` (physical px) lies inside a box that hides
// it. With occlude_all set, any panel rect hides a pooled cell regardless of
// seq; a zero panel_count (the live fill and the cursor) skips the loop.
fn occluded(frag: vec2<f32>) -> bool {
    for (var j = 0u; j < globals.panel_count; j = j + 1u) {
        let o = occluders[j];
        if globals.occlude_all == 1u {
            let box_min = o.cell * globals.cell_size;
            let box_max = (o.cell + o.size) * globals.cell_size;
            if frag.x >= box_min.x && frag.x < box_max.x && frag.y >= box_min.y
                && frag.y < box_max.y {
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
    @location(0) cell: vec2<f32>,
    @location(1) color: vec3<f32>,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0)
    );

    // Snap each cell edge to a whole pixel so consecutive cells share an exact
    // integer boundary and each spans whole pixels, leaving no fractional sliver
    // (the dark seam) between same-color cells. Scroll is added after the snap so
    // smooth scrolling stays fractional and the grid only snaps once it settles.
    let pixel = round((cell + corners[vertex_index]) * globals.cell_size)
        + vec2<f32>(0.0, globals.scroll_y);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
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
