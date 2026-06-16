// Overlay pass. One instance per floating region draws a filled box outlined by
// a one-pixel border, anchored at a cell and sized in cells. The region is
// opaque, so it occludes the cells beneath it; drawn last, it sits on top.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) size_px: vec2<f32>,
    @location(2) @interpolate(flat) fill: vec3<f32>,
    @location(3) @interpolate(flat) border: vec3<f32>,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) cell_pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) fill: vec3<f32>,
    @location(3) border: vec3<f32>,
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

    let pixel = (cell_pos + corner * size) * globals.cell_size;
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.local = corner;
    out.size_px = size * globals.cell_size;
    out.fill = fill;
    out.border = border;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let pos = in.local * in.size_px;
    let border_px = 1.0;
    let on_border = pos.x < border_px
        || pos.y < border_px
        || pos.x > in.size_px.x - border_px
        || pos.y > in.size_px.y - border_px;
    let color = select(in.fill, in.border, on_border);
    return vec4<f32>(color, 1.0);
}
