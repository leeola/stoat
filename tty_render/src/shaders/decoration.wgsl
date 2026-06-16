// Cell-edge border pass. One instance per present edge draws a quad over the
// cell; the fragment paints a line along that one edge at the requested weight
// and leaves the rest transparent, so it alpha-blends over the background.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

const EDGE_TOP: u32 = 0u;
const EDGE_RIGHT: u32 = 1u;
const EDGE_BOTTOM: u32 = 2u;
const EDGE_LEFT: u32 = 3u;

const STYLE_LIGHT: u32 = 0u;
const STYLE_HEAVY: u32 = 1u;
const STYLE_DOUBLE: u32 = 2u;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) color: vec3<f32>,
    @location(2) @interpolate(flat) edge: u32,
    @location(3) @interpolate(flat) style: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) cell_pos: vec2<f32>,
    @location(1) color: vec3<f32>,
    @location(2) edge: u32,
    @location(3) style: u32,
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

    let pixel = (cell_pos + corner) * globals.cell_size;
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.local = corner;
    out.color = color;
    out.edge = edge;
    out.style = style;
    return out;
}

// Distance in pixels from the named edge.
fn edge_distance(edge: u32, pos: vec2<f32>, cell: vec2<f32>) -> f32 {
    if edge == EDGE_TOP {
        return pos.y;
    }
    if edge == EDGE_BOTTOM {
        return cell.y - pos.y;
    }
    if edge == EDGE_LEFT {
        return pos.x;
    }
    return cell.x - pos.x;
}

// Coverage of a line `d` pixels in from the edge, by weight. A single line hugs
// the edge; Double adds a second line one pixel further in.
fn border_coverage(style: u32, d: f32) -> f32 {
    if style == STYLE_HEAVY {
        return clamp(2.5 - d + 0.5, 0.0, 1.0);
    }
    if style == STYLE_DOUBLE {
        let inner = clamp(1.0 - d + 0.5, 0.0, 1.0);
        let outer = clamp(min(d - 2.0, 3.0 - d) + 0.5, 0.0, 1.0);
        return max(inner, outer);
    }
    return clamp(1.0 - d + 0.5, 0.0, 1.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let pos = in.local * globals.cell_size;
    let d = edge_distance(in.edge, pos, globals.cell_size);
    let coverage = border_coverage(in.style, d);
    return vec4<f32>(in.color, coverage);
}
