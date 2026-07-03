// Panel pass. One instance per off-grid modal-chrome region draws a soft drop
// shadow, an optional interior fill, and a hairline stroke frame around a cell
// rectangle with rounded corners. Unlike an overlay it is not opaque: it is
// chrome layered with the grid, so it draws before the grid text and the framed
// cells render over the fill.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// Border style codes, matching the protocol's border style ordering.
const STYLE_LIGHT: u32 = 0u;
const STYLE_HEAVY: u32 = 1u;
const STYLE_DOUBLE: u32 = 2u;
const STYLE_ROUNDED: u32 = 3u;

// Drop-shadow color and peak alpha. The alpha falls off to zero across the
// shadow margin, so this is the opacity directly beneath the box edge.
const SHADOW_COLOR: vec3<f32> = vec3<f32>(0.0, 0.0, 0.0);
const SHADOW_ALPHA: f32 = 0.4;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    // Pixel position within the shadow-expanded quad, top-left origin.
    @location(0) quad_px: vec2<f32>,
    // Box rectangle in the same quad-pixel space.
    @location(1) @interpolate(flat) box_min: vec2<f32>,
    @location(2) @interpolate(flat) box_max: vec2<f32>,
    // Shadow displacement (xy) and blur radius (z), in pixels.
    @location(3) @interpolate(flat) shadow: vec3<f32>,
    @location(4) @interpolate(flat) fill: vec3<f32>,
    @location(5) @interpolate(flat) border: vec3<f32>,
    @location(6) @interpolate(flat) corner_radius: f32,
    // 1.0 to paint the interior fill, 0.0 to leave the cells showing through.
    @location(7) @interpolate(flat) fill_flag: f32,
    @location(8) @interpolate(flat) style: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) cell_pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) fill: vec3<f32>,
    @location(3) border: vec3<f32>,
    @location(4) shadow_offset: vec2<f32>,
    @location(5) shadow_margin: f32,
    @location(6) corner_radius: f32,
    @location(7) fill_flag: f32,
    @location(8) style: u32,
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

    let box_min_px = cell_pos * globals.cell_size;
    let box_size_px = size * globals.cell_size;

    // Expand the quad so the offset, blurred shadow is fully contained on every
    // side. The down-right edge must reach box + offset + margin, so pad by the
    // larger offset component plus the margin.
    let pad = shadow_margin + max(abs(shadow_offset.x), abs(shadow_offset.y));
    let quad_min_px = box_min_px - vec2<f32>(pad, pad);
    let quad_size_px = box_size_px + vec2<f32>(2.0 * pad, 2.0 * pad);

    let pixel = quad_min_px + corner * quad_size_px;
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.quad_px = corner * quad_size_px;
    out.box_min = vec2<f32>(pad, pad);
    out.box_max = vec2<f32>(pad, pad) + box_size_px;
    out.shadow = vec3<f32>(shadow_offset.x, shadow_offset.y, shadow_margin);
    out.fill = fill;
    out.border = border;
    out.corner_radius = corner_radius;
    out.fill_flag = fill_flag;
    out.style = style;
    return out;
}

// Signed distance to a rounded rectangle of half-size `half` and corner radius
// `r` centered at the origin, negative inside.
fn rounded_box_sdf(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

// Anti-aliased coverage of a stroke `d` pixels from its centerline, weighted by
// the border style: a heavy line is thicker, a double line is two parallel
// hairlines, and light and rounded are a single hairline.
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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.quad_px;

    let center = (in.box_min + in.box_max) * 0.5;
    let half = (in.box_max - in.box_min) * 0.5;
    let radius = min(in.corner_radius, min(half.x, half.y));
    let box_sdf = rounded_box_sdf(p - center, half, radius);

    // Hairline frame straddling the perimeter, weighted by the border style.
    let stroke = line_coverage(in.style, abs(box_sdf));
    // Optional interior fill, faded across the rounded edge.
    let interior = 1.0 - smoothstep(-1.0, 1.0, box_sdf);
    let fill_alpha = in.fill_flag * interior;

    // Exterior distance to the shadow rectangle (the box shifted by the offset),
    // faded across the blur margin. A zero margin means no shadow.
    let offset = in.shadow.xy;
    let margin = in.shadow.z;
    let shadow_min = in.box_min + offset;
    let shadow_max = in.box_max + offset;
    let d = max(vec2<f32>(0.0, 0.0), max(shadow_min - p, p - shadow_max));
    let shadow_alpha = select(
        0.0,
        SHADOW_ALPHA * (1.0 - smoothstep(0.0, margin, length(d))),
        margin > 0.0
    );

    // Composite bottom-up: the shadow, then the optional fill, then the stroke.
    var color = SHADOW_COLOR;
    var alpha = shadow_alpha;
    color = mix(color, in.fill, fill_alpha);
    alpha = max(alpha, fill_alpha);
    color = mix(color, in.border, stroke);
    alpha = max(alpha, stroke);
    return vec4<f32>(color, alpha);
}
