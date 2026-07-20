// Panel pass. One instance per off-grid modal-chrome region draws a soft drop
// shadow, an optional interior fill, and a hairline stroke frame around a cell
// rectangle with rounded corners. Unlike an overlay it is not opaque: it is
// chrome layered with the grid, so it draws before the grid text and the framed
// cells render over the fill.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    count: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// Every panel instance's raw floats, for self-occlusion. Each instance spans
// INSTANCE_STRIDE floats; the box anchor cell is floats 0-1 and the size in
// cells is floats 2-3, matching the PanelInstance memory layout.
@group(0) @binding(1)
var<storage, read> instances: array<f32>;

const INSTANCE_STRIDE: u32 = 18u;

// Border style codes, matching the protocol's border style ordering.
const STYLE_LIGHT: u32 = 0u;
const STYLE_HEAVY: u32 = 1u;
const STYLE_DOUBLE: u32 = 2u;
const STYLE_ROUNDED: u32 = 3u;

// Drop-shadow color and peak alpha. The shadow paints only outside the box
// exterior (fs_main gates it by interior coverage); this is its peak opacity
// just past the box edge, falling to zero across the shadow margin.
const SHADOW_COLOR: vec3<f32> = vec3<f32>(0.0, 0.0, 0.0);
const SHADOW_ALPHA: f32 = 0.22;
// Peak opacity of an overhang shadow's interior bottom band, at the box's bottom
// edge, falling to zero across the margin as it rises. Fainter than SHADOW_ALPHA
// so it reads as a soft cast rather than a hard line.
const SHADOW_ALPHA_OVERHANG: f32 = 0.14;

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
    // This panel's draw index, so the fragment shader can occlude against every
    // later (higher-index, on-top) panel.
    @location(9) @interpolate(flat) instance: u32,
    // Shadow mode: 0.0 drop (exterior, offset), 1.0 tucked (exterior, clipped at
    // the box bottom), 2.0 overhang (a small interior band along the bottom edge).
    @location(10) @interpolate(flat) shadow_mode: f32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
    @location(0) cell_pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) fill: vec3<f32>,
    @location(3) border: vec3<f32>,
    @location(4) shadow_offset: vec2<f32>,
    @location(5) shadow_margin: f32,
    @location(6) corner_radius: f32,
    @location(7) fill_flag: f32,
    @location(8) style: u32,
    @location(9) inset_x: f32,
    @location(10) shadow_mode: f32,
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
    // Shave inset_x off each x edge so the frame, fill, corners, and shadow all
    // draw narrower than the cell rect; the quad still spans the full rect, so
    // the inset strip rasterizes as transparent cells.
    out.box_min = vec2<f32>(pad + inset_x, pad);
    out.box_max = vec2<f32>(pad + box_size_px.x - inset_x, pad + box_size_px.y);
    out.shadow = vec3<f32>(shadow_offset.x, shadow_offset.y, shadow_margin);
    out.fill = fill;
    out.border = border;
    out.corner_radius = corner_radius;
    out.fill_flag = fill_flag;
    out.style = style;
    out.instance = instance_index;
    out.shadow_mode = shadow_mode;
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
    // Self-occlusion: discard fragments falling inside the box rect of any later
    // panel (a higher index draws on top), so a lower box's shadow, fill, and
    // stroke cannot show through an upper box's body. A later panel's exterior
    // shadow lies outside its box rect, so shadows past a box edge keep blending.
    let frag = in.clip.xy;
    for (var j = in.instance + 1u; j < globals.count; j = j + 1u) {
        let base = j * INSTANCE_STRIDE;
        let cell_j = vec2<f32>(instances[base], instances[base + 1u]);
        let size_j = vec2<f32>(instances[base + 2u], instances[base + 3u]);
        let inset_j = instances[base + 16u];
        let box_min = cell_j * globals.cell_size + vec2<f32>(inset_j, 0.0);
        let box_max = (cell_j + size_j) * globals.cell_size - vec2<f32>(inset_j, 0.0);
        if frag.x >= box_min.x && frag.x < box_max.x && frag.y >= box_min.y
            && frag.y < box_max.y {
            discard;
        }
    }

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
    // A tucked shadow (mode 1) paints nothing below the box's bottom edge, so the
    // seam with whatever sits below the panel stays clean.
    let clip = select(1.0, step(p.y, in.box_max.y), in.shadow_mode > 0.5 && in.shadow_mode < 1.5);
    // Exterior shadow for drop (mode 0) and tucked (mode 1), gated to the box
    // exterior so an unfilled panel's interior is not washed by its own shadow.
    // `interior` is 1 inside the box and 0 outside.
    let shadow_base = select(
        0.0,
        SHADOW_ALPHA * (1.0 - smoothstep(0.0, margin, length(d))),
        margin > 0.0 && in.shadow_mode < 1.5
    ) * (1.0 - interior) * clip;
    // Overhang (mode 2): a small interior band rising from the box's bottom edge,
    // so the panel reads as tucked under whatever overhangs it above.
    let overhang = select(
        0.0,
        SHADOW_ALPHA_OVERHANG * (1.0 - smoothstep(0.0, margin, in.box_max.y - p.y)) * interior,
        in.shadow_mode > 1.5
    );

    // Composite bottom-up: the exterior shadow, the optional fill, the overhang
    // band cast onto that fill, then the stroke.
    var color = SHADOW_COLOR;
    var alpha = shadow_base;
    color = mix(color, in.fill, fill_alpha);
    alpha = max(alpha, fill_alpha);
    color = mix(color, SHADOW_COLOR, overhang);
    alpha = max(alpha, overhang);
    color = mix(color, in.border, stroke);
    alpha = max(alpha, stroke);
    return vec4<f32>(color, alpha);
}
