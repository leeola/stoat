// Cell border pass. One instance per bordered cell draws a quad over the cell;
// the fragment paints a line along every present edge in that edge's color and
// weight, leaving the rest transparent so it alpha-blends over the background.
// Where two adjacent edges of a cell are both Rounded, the square join is
// replaced by a quarter-circle arc.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    scroll_y: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

const EDGE_TOP: u32 = 1u;
const EDGE_RIGHT: u32 = 2u;
const EDGE_BOTTOM: u32 = 4u;
const EDGE_LEFT: u32 = 8u;

const STYLE_LIGHT: u32 = 0u;
const STYLE_HEAVY: u32 = 1u;
const STYLE_DOUBLE: u32 = 2u;
const STYLE_ROUNDED: u32 = 3u;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) top_color: vec3<f32>,
    @location(2) @interpolate(flat) right_color: vec3<f32>,
    @location(3) @interpolate(flat) bottom_color: vec3<f32>,
    @location(4) @interpolate(flat) left_color: vec3<f32>,
    @location(5) @interpolate(flat) edges: u32,
    @location(6) @interpolate(flat) styles: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) cell_pos: vec2<f32>,
    @location(1) top_color: vec3<f32>,
    @location(2) right_color: vec3<f32>,
    @location(3) bottom_color: vec3<f32>,
    @location(4) left_color: vec3<f32>,
    @location(5) edges: u32,
    @location(6) styles: u32,
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

    let pixel = (cell_pos + corner) * globals.cell_size + vec2<f32>(0.0, globals.scroll_y);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.local = corner;
    out.top_color = top_color;
    out.right_color = right_color;
    out.bottom_color = bottom_color;
    out.left_color = left_color;
    out.edges = edges;
    out.styles = styles;
    return out;
}

// The 8-bit style code for one edge, unpacked from the packed `styles` word.
fn edge_style(styles: u32, shift: u32) -> u32 {
    return (styles >> shift) & 0xffu;
}

// Coverage of a straight line `d` pixels in from its edge, by weight. Light and
// Rounded draw a single line hugging the edge; Heavy widens it; Double adds a
// second line one pixel further in.
fn line_coverage(style: u32, d: f32) -> f32 {
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

// Coverage of the quarter-circle border line of radius `r` centered at `center`.
// Full at the outer radius so it meets the straight runs, fading inward, and
// zero past `r` so the cell corner outside the arc stays transparent.
fn arc_coverage(pos: vec2<f32>, center: vec2<f32>, r: f32) -> f32 {
    let dist = length(pos - center);
    return clamp(1.5 - abs((r - dist) - 0.5), 0.0, 1.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let cell = globals.cell_size;
    let pos = in.local * cell;
    let r = min(cell.x, cell.y) * 0.5;

    let top_s = edge_style(in.styles, 0u);
    let right_s = edge_style(in.styles, 8u);
    let bottom_s = edge_style(in.styles, 16u);
    let left_s = edge_style(in.styles, 24u);

    let has_top = (in.edges & EDGE_TOP) != 0u;
    let has_right = (in.edges & EDGE_RIGHT) != 0u;
    let has_bottom = (in.edges & EDGE_BOTTOM) != 0u;
    let has_left = (in.edges & EDGE_LEFT) != 0u;

    // A rounded corner replaces the square join inside its quadrant, so test the
    // quadrants first and return the arc when both adjacent edges are Rounded.
    let tl = has_top && has_left && top_s == STYLE_ROUNDED && left_s == STYLE_ROUNDED;
    if tl && pos.x < r && pos.y < r {
        return vec4<f32>(in.top_color, arc_coverage(pos, vec2<f32>(r, r), r));
    }
    let tr = has_top && has_right && top_s == STYLE_ROUNDED && right_s == STYLE_ROUNDED;
    if tr && pos.x > cell.x - r && pos.y < r {
        return vec4<f32>(in.top_color, arc_coverage(pos, vec2<f32>(cell.x - r, r), r));
    }
    let bl = has_bottom && has_left && bottom_s == STYLE_ROUNDED && left_s == STYLE_ROUNDED;
    if bl && pos.x < r && pos.y > cell.y - r {
        return vec4<f32>(in.bottom_color, arc_coverage(pos, vec2<f32>(r, cell.y - r), r));
    }
    let br = has_bottom && has_right && bottom_s == STYLE_ROUNDED && right_s == STYLE_ROUNDED;
    if br && pos.x > cell.x - r && pos.y > cell.y - r {
        return vec4<f32>(in.bottom_color, arc_coverage(pos, vec2<f32>(cell.x - r, cell.y - r), r));
    }

    // Outside any rounded quadrant: the straight per-edge lines, each in its own
    // color, taking the edge with the most coverage at this pixel.
    var color = vec3<f32>(0.0);
    var coverage = 0.0;
    if has_top {
        let c = line_coverage(top_s, pos.y);
        if c > coverage {
            coverage = c;
            color = in.top_color;
        }
    }
    if has_bottom {
        let c = line_coverage(bottom_s, cell.y - pos.y);
        if c > coverage {
            coverage = c;
            color = in.bottom_color;
        }
    }
    if has_left {
        let c = line_coverage(left_s, pos.x);
        if c > coverage {
            coverage = c;
            color = in.left_color;
        }
    }
    if has_right {
        let c = line_coverage(right_s, cell.x - pos.x);
        if c > coverage {
            coverage = c;
            color = in.right_color;
        }
    }
    return vec4<f32>(color, coverage);
}
