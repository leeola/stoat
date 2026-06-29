// Instanced per-cell background fill. One instance per grid cell; the six
// quad corners are generated from the vertex index, so the only vertex buffer
// is the per-cell instance stream.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    cursor_corners_01: vec4<f32>,
    cursor_corners_23: vec4<f32>,
    scroll_y: f32,
    pad: f32,
    pad2: f32,
    pad3: f32,
    cursor_color: vec4<f32>,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

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
