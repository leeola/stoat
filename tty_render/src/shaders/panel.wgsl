// Panel pass. One instance per off-grid modal-chrome region draws a soft drop
// shadow, an optional interior fill, and a hairline stroke frame around a cell
// rectangle with rounded corners. Unlike an overlay it is not opaque: it is
// chrome layered with the grid, so the body draws before the grid text and the
// framed cells render over the fill. The stroke draws after that text instead:
// a frame surrounds the cells it frames, and ink reaching a cell edge would
// otherwise break the line around it.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    count: u32,
    // Physical pixels per logical pixel. The stroke weights below are stated in
    // logical pixels, so a 2x display draws them twice as wide and the frame
    // keeps its weight against the box it frames.
    scale_factor: f32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// Every panel instance's raw floats, for self-occlusion. Each instance spans
// INSTANCE_STRIDE floats; the box anchor cell is floats 0-1 and the size in
// cells is floats 2-3, matching the PanelInstance memory layout.
@group(0) @binding(1)
var<storage, read> instances: array<f32>;

const INSTANCE_STRIDE: u32 = 28u;

// Bits of a panel's shape word past its four corner bits, one per bottom
// corner that a fillet joins to the surface below.
const FILLET_BOTTOM_LEFT: u32 = 16u;
const FILLET_BOTTOM_RIGHT: u32 = 32u;
// The bit of a panel that no pane pool takes as an occluder: a backdrop, or a
// panel that rides a pool this frame. A pool's composite paints over its box.
const PASSES_POOLS: u32 = 64u;

// The hatch's line pitch in logical pixels, measured along x plus y.
const HATCH_PITCH: f32 = 7.0;

// Border style codes, matching the protocol's border style ordering.
const STYLE_LIGHT: u32 = 0u;
const STYLE_HEAVY: u32 = 1u;
const STYLE_DOUBLE: u32 = 2u;
const STYLE_ROUNDED: u32 = 3u;
const STYLE_NONE: u32 = 4u;
const STYLE_DASHED: u32 = 5u;

// Drop-shadow color and peak alpha. The shadow paints only outside the box
// exterior (fs_main gates it by interior coverage); this is its opacity where
// the blur covers the shadow rect whole, which is well inside the rect. Its own
// edge carries half of it, and three sigma out it reaches zero.
const SHADOW_COLOR: vec3<f32> = vec3<f32>(0.0, 0.0, 0.0);
const SHADOW_ALPHA: f32 = 0.32;
// Peak opacity of a halo shadow, the alpha of the boards'
// `box-shadow: 0 -10px 14px -10px rgba(0, 0, 0, 0.5)` and its leftward twin
// on a docked box. As for SHADOW_ALPHA, the box's own edge carries half of it.
const SHADOW_ALPHA_HALO: f32 = 0.5;
// Opacity of a tucked shadow's interior bottom band, the alpha of the boards'
// `box-shadow: inset 0 -6px 8px -6px rgba(0, 0, 0, 0.6)`. That shadow's
// negative spread cancels its offset, so only its blur reaches inside the box.
// The blur is a gaussian edge at the bottom edge, which lays half this opacity
// at the edge and fades out across the margin as it rises.
const SHADOW_ALPHA_TUCKED: f32 = 0.6;
// Peak opacity of an overhang shadow's interior bottom band, at the box's bottom
// edge, falling to zero across the margin as it rises. Fainter than SHADOW_ALPHA
// so it reads as a soft cast rather than a hard line.
const SHADOW_ALPHA_OVERHANG: f32 = 0.14;
// Peak opacity of a cast shadow, the alpha of the boards'
// `box-shadow: 0 4px 8px rgba(0, 0, 0, 0.55)` under a pinned row.
const SHADOW_ALPHA_CAST: f32 = 0.55;

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
    // 1.0 to paint the interior fill, 0.0 to leave the cells showing through,
    // and 2.0 to paint a backdrop's fill only outside its cells.
    @location(7) @interpolate(flat) fill_flag: f32,
    @location(8) @interpolate(flat) style: u32,
    // This panel's draw index, so the fragment shader can occlude against every
    // later (higher-index, on-top) panel.
    @location(9) @interpolate(flat) instance: u32,
    // Shadow mode: 0.0 drop (exterior, offset), 1.0 tucked (an interior band along
    // the bottom edge), 2.0 overhang (a fainter, shorter interior band along the
    // bottom edge), 3.0 halo (exterior, clipped at the box's bottom and right
    // edges), 4.0 cast
    // (exterior, offset down, clipped to below the box bottom between its sides).
    @location(10) @interpolate(flat) shadow_mode: f32,
    // The rounded corners in bits 0-3, top-left clockwise, and the fillets in
    // FILLET_BOTTOM_LEFT and FILLET_BOTTOM_RIGHT.
    @location(11) @interpolate(flat) shape: u32,
    // The hatch color, and 1.0 to stripe the fill with it.
    @location(12) @interpolate(flat) hatch: vec3<f32>,
    @location(13) @interpolate(flat) hatch_flag: f32,
    // The panel's cell rect in quad pixels, min xy then max xy, before its edges
    // move the box. A backdrop's fill stays outside it.
    @location(14) @interpolate(flat) cells: vec4<f32>,
    // A fillet's radius in physical pixels.
    @location(15) @interpolate(flat) fillet_radius: f32,
}

// One panel instance's vertex attributes, read by both vertex stages.
struct PanelIn {
    @location(0) cell_pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) fill: vec3<f32>,
    @location(3) border: vec3<f32>,
    @location(4) shadow_offset: vec2<f32>,
    @location(5) shadow_margin: f32,
    @location(6) corner_radius: f32,
    @location(7) fill_flag: f32,
    @location(8) style: u32,
    @location(9) shadow_mode: f32,
    @location(10) shape: u32,
    @location(11) hatch: vec3<f32>,
    @location(12) hatch_flag: f32,
    @location(13) edge_px: vec4<f32>,
    @location(14) edge_modes: u32,
    @location(15) fillet_radius: f32,
}

// The shadow, the fill, and the bottom band.
//
// The interior rectangle sits two pixels past the corner arcs, where the fill
// alone reaches a pixel, so an unfilled panel leaves it out. A backdrop's fill
// paints nothing inside its cells, so a backdrop leaves it out too. A tucked or
// overhang panel casts its band inside the bottom edge even with no fill, so
// the bottom inset also clears the band.
@vertex
fn vs_under(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
    panel: PanelIn,
) -> VsOut {
    let radius = panel_radius(panel);
    let side = radius + 2.0;
    let bottom = select(
        side,
        max(radius, panel.shadow_margin) + 2.0,
        casts_band(panel.shadow_mode)
    );
    return panel_vertex(
        vertex_index,
        instance_index,
        panel,
        vec4<f32>(side, side, side, bottom),
        panel.fill_flag > 0.5 && panel.fill_flag < 1.5
    );
}

// The frame stroke.
//
// The widest frame band ends three logical pixels in, so past that band and
// the corner arcs the stroke is alpha 0 and the interior never draws.
@vertex
fn vs_stroke(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
    panel: PanelIn,
) -> VsOut {
    let side = max(panel_radius(panel), 3.0 * globals.scale_factor) + 2.0;
    return panel_vertex(vertex_index, instance_index, panel, vec4<f32>(side), false);
}

// The corner radius the box draws with, held to its shorter half side, as
// box_radius reads it back in the fragment stages.
fn panel_radius(panel: PanelIn) -> f32 {
    let offsets = edge_offsets(panel.edge_px, panel.edge_modes, globals.cell_size);
    let half = (panel.size * globals.cell_size - offsets.wx - offsets.yz) * 0.5;
    return min(panel.corner_radius, min(half.x, half.y));
}

// Vertex `vertex_index` of the five regions that tile a panel's
// shadow-padded quad, six vertices each: the top band, the bottom band, the
// left band, the right band, and last the interior rectangle.
//
// `inset` shrinks the box to the interior on the left, top, right, and bottom.
// A box too small for it leaves the interior empty, and the bands meet rather
// than cross. With `interior` false the interior quad lands outside clip space
// and draws nothing, and the interior also shrinks into the cells. So a strip
// that the box reaches across beside its cells stays in the bands and draws.
//
// A vertex carries the quad_px that one quad over the whole padded rect gives
// its position, so a fragment reads the same inputs in every region. Each
// corner picks a region edge whole rather than interpolating to it, so two
// regions that share an edge put it on the same bits and no pixel on it draws
// twice or not at all.
fn panel_vertex(
    vertex_index: u32,
    instance_index: u32,
    panel: PanelIn,
    inset: vec4<f32>,
    interior: bool,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0)
    );
    let corner = corners[vertex_index % 6u];
    let region = vertex_index / 6u;

    let box_min_px = panel.cell_pos * globals.cell_size;
    let box_size_px = panel.size * globals.cell_size;
    let offsets = edge_offsets(panel.edge_px, panel.edge_modes, globals.cell_size);

    // Expand the quad so the offset, blurred shadow is fully contained on every
    // side. The down-right edge must reach box + offset + margin, so pad by the
    // larger offset component plus the margin. A fillet reaches its radius past
    // the box's side, so a panel with one pads at least that far. A box that
    // reaches out across the cells beside it pads that much further.
    let shadow_pad = panel.shadow_margin + max(abs(panel.shadow_offset.x), abs(panel.shadow_offset.y));
    let filleted = (panel.shape & (FILLET_BOTTOM_LEFT | FILLET_BOTTOM_RIGHT)) != 0u;
    let reach = max(max(max(-offsets.x, -offsets.y), max(-offsets.z, -offsets.w)), 0.0);
    let pad = reach + max(shadow_pad, select(0.0, panel.fillet_radius, filleted));
    let quad_min_px = box_min_px - vec2<f32>(pad, pad);
    let quad_size_px = box_size_px + vec2<f32>(2.0 * pad, 2.0 * pad);

    // Move each edge of the cell rect by its offset, so the frame, fill,
    // corners, and shadow all draw on the box the edges state. The quad still
    // spans the padded rect, so a strip between the box and a cell edge
    // rasterizes as transparent.
    let cells_min = vec2<f32>(pad, pad);
    let cells_max = cells_min + box_size_px;
    let box_min = cells_min + offsets.wx;
    let box_max = cells_max - offsets.yz;

    var inner_min = min(box_min + inset.xy, box_max);
    var inner_max = max(box_max - inset.zw, inner_min);
    if !interior {
        inner_min = min(max(inner_min, cells_min), cells_max);
        inner_max = max(min(inner_max, cells_max), inner_min);
    }

    var lo = inner_min;
    var hi = inner_max;
    switch region {
        case 0u: {
            lo = vec2<f32>(0.0, 0.0);
            hi = vec2<f32>(quad_size_px.x, inner_min.y);
        }
        case 1u: {
            lo = vec2<f32>(0.0, inner_max.y);
            hi = quad_size_px;
        }
        case 2u: {
            lo = vec2<f32>(0.0, inner_min.y);
            hi = vec2<f32>(inner_min.x, inner_max.y);
        }
        case 3u: {
            lo = vec2<f32>(inner_max.x, inner_min.y);
            hi = vec2<f32>(quad_size_px.x, inner_max.y);
        }
        default: {}
    }
    let local = select(lo, hi, corner > vec2<f32>(0.5, 0.5));

    var out: VsOut;
    if region == 4u && !interior {
        out.clip = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        return out;
    }

    let pixel = quad_min_px + local;
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.quad_px = local;
    out.box_min = box_min;
    out.box_max = box_max;
    out.shadow = vec3<f32>(panel.shadow_offset.x, panel.shadow_offset.y, panel.shadow_margin);
    out.fill = panel.fill;
    out.border = panel.border;
    out.corner_radius = panel.corner_radius;
    out.fill_flag = panel.fill_flag;
    out.style = panel.style;
    out.instance = instance_index;
    out.shadow_mode = panel.shadow_mode;
    out.shape = panel.shape;
    out.hatch = panel.hatch;
    out.hatch_flag = panel.hatch_flag;
    out.cells = vec4<f32>(cells_min, cells_max);
    out.fillet_radius = panel.fillet_radius;
    return out;
}

// Lay a layer of `color` at `alpha` over the premultiplied `under`, returning
// the accumulated color in rgb and the coverage in a.
//
// Two partly covered layers cover more together than either does alone. Taking
// the larger of the two alphas instead understates that, and because the
// pipeline blends unpremultiplied, an understated alpha also weakens the
// layer's own color and lets the ground behind show through: a ring wherever
// the ramps overlap.
fn over(under: vec4<f32>, color: vec3<f32>, alpha: f32) -> vec4<f32> {
    return vec4<f32>(
        color * alpha + under.rgb * (1.0 - alpha),
        alpha + under.a * (1.0 - alpha)
    );
}

// Coverage of the band that runs from `near` to `far` pixels inward from the
// edge, for a point `d` pixels inward.
//
// Each end ramps over one pixel and covers the exact area of it that falls in
// the band, so a band one pixel deep is one pixel of ink. A point outside the
// edge has a negative `d` and takes none.
fn band(d: f32, near: f32, far: f32) -> f32 {
    return clamp(min(d - near, far - d) + 0.5, 0.0, 1.0);
}

// Coverage of the frame at a point `d` pixels inward from the box edge,
// weighted by the border style: a heavy line is thicker, a double line is two
// parallel hairlines, light and rounded are a single hairline, and none draws
// nothing.
//
// The frame sits inside the box rather than straddling its perimeter, so the
// box covers exactly the pixels it declares and a frame drawn without a shadow
// behind it is the same frame as one drawn with a shadow.
//
// The heavy width and the double line's separation are logical pixels, scaled
// to the display, so a heavy frame stays visibly heavier than a light one at
// any density. The light, rounded, and dashed hairline is one physical pixel on
// purpose: it is the thinnest line the display draws, so a denser screen
// renders it finer rather than fatter. A dashed frame takes the light band
// here, and dash_mask cuts its gaps.
fn line_coverage(style: u32, d: f32) -> f32 {
    let s = globals.scale_factor;
    if style == STYLE_NONE {
        return 0.0;
    }
    if style == STYLE_HEAVY {
        return band(d, 0.0, 2.0 * s);
    }
    if style == STYLE_DOUBLE {
        return max(band(d, 0.0, s), band(d, 2.0 * s, 3.0 * s));
    }
    return band(d, 0.0, 1.0);
}

/// Discard a fragment that falls inside the box rect of any later panel.
///
/// A higher index draws on top, so a lower box's shadow, fill, and stroke must
/// not show through an upper box's body. A later panel's exterior shadow lies
/// outside its box rect, so shadows past a box edge keep blending.
///
/// Both fragment stages run this. A stroke hidden behind a later panel has to
/// stay hidden whichever pass draws it.
fn occluded(frag: vec2<f32>, instance: u32) -> bool {
    for (var j = instance + 1u; j < globals.count; j = j + 1u) {
        let base = j * INSTANCE_STRIDE;
        let cell_j = vec2<f32>(instances[base], instances[base + 1u]);
        let size_j = vec2<f32>(instances[base + 2u], instances[base + 3u]);
        let edge_px_j = vec4<f32>(
            instances[base + 22u],
            instances[base + 23u],
            instances[base + 24u],
            instances[base + 25u]
        );
        let sdf = occluder_sdf(
            frag,
            cell_j,
            size_j,
            globals.cell_size,
            instances[base + 13u],
            edge_px_j,
            bitcast<u32>(instances[base + 26u]),
            bitcast<u32>(instances[base + 17u]) & 15u
        );
        if sdf < -0.5 {
            return true;
        }
    }
    return false;
}

/// Whether `frag` lies inside the box of a panel that a pane pool's composite
/// leaves to the live grid, which is every panel but the ones that pass pools.
///
/// The walk covers every instance, earlier and later alike, since a composite
/// leaves each such box whichever panel declared it first.
fn boxed(frag: vec2<f32>) -> bool {
    for (var j = 0u; j < globals.count; j = j + 1u) {
        let base = j * INSTANCE_STRIDE;
        let shape_j = bitcast<u32>(instances[base + 17u]);
        if (shape_j & PASSES_POOLS) != 0u {
            continue;
        }
        let cell_j = vec2<f32>(instances[base], instances[base + 1u]);
        let size_j = vec2<f32>(instances[base + 2u], instances[base + 3u]);
        let edge_px_j = vec4<f32>(
            instances[base + 22u],
            instances[base + 23u],
            instances[base + 24u],
            instances[base + 25u]
        );
        let sdf = occluder_sdf(
            frag,
            cell_j,
            size_j,
            globals.cell_size,
            instances[base + 13u],
            edge_px_j,
            bitcast<u32>(instances[base + 26u]),
            shape_j & 15u
        );
        if sdf < -0.5 {
            return true;
        }
    }
    return false;
}

/// The panel's signed distance field and the coverages derived from it, shared
/// by both fragment stages so each measures the same box.
struct Coverage {
    stroke: f32,
    fill: f32,
    shadow: f32,
    band: f32,
}

/// Whether shadow `mode` casts a band inside the box's bottom edge, as tucked
/// (1) and overhang (2) do, rather than a shadow outside it.
fn casts_band(mode: f32) -> bool {
    return mode > 0.5 && mode < 2.5;
}

/// The corner radius the box draws with, held to its shorter half side.
fn box_radius(in: VsOut) -> f32 {
    let half = (in.box_max - in.box_min) * 0.5;
    return min(in.corner_radius, min(half.x, half.y));
}

/// Signed distance in pixels from the fragment to the panel's rounded box,
/// negative inside.
///
/// The fragment's quadrant names the corner it lies toward, bit 0 top-left and
/// then clockwise, and a corner the shape's mask leaves out stays square.
fn box_distance(in: VsOut) -> f32 {
    let center = (in.box_min + in.box_max) * 0.5;
    let half = (in.box_max - in.box_min) * 0.5;
    let p = in.quad_px - center;
    let corner = select(select(0u, 1u, p.x > 0.0), select(3u, 2u, p.x > 0.0), p.y > 0.0);
    let rounded = ((in.shape >> corner) & 1u) == 1u;
    return rounded_box_sdf(p, half, select(0.0, box_radius(in), rounded));
}

/// Coverage of the fillet beside the bottom corner at `corner`, reaching out
/// to the `side` of it (-1 left, 1 right).
///
/// The fillet is the square of side `r` past the corner and above it, less the
/// quarter disc of radius `r` centered on the square's outer top corner. It
/// curves the box's straight side into the surface below.
fn fillet_coverage(p: vec2<f32>, corner: vec2<f32>, side: f32, r: f32) -> f32 {
    let outer = corner + vec2<f32>(side * r, -r);
    let lo = min(corner, outer);
    let hi = max(corner, outer);
    let square = max(max(lo.x - p.x, p.x - hi.x), max(lo.y - p.y, p.y - hi.y));
    let disc = r - distance(p, outer);
    return clamp(0.5 - max(square, disc), 0.0, 1.0);
}

/// The share of the fill the fragment takes, before its coverage of the box.
///
/// A backdrop's fill paints only outside the panel's own cells, so the cells
/// keep every tint they draw. A pixel is inside the cells when its center is.
fn fill_weight(in: VsOut) -> f32 {
    let inside_cells = all(in.quad_px >= in.cells.xy) && all(in.quad_px < in.cells.zw);
    return select(min(in.fill_flag, 1.0), 0.0, in.fill_flag > 1.5 && inside_cells);
}

/// The fill's color at the fragment.
///
/// A hatched panel takes the hatch color on its lines, one logical pixel wide
/// and HATCH_PITCH apart along x plus y, which run at 135 degrees. The lines
/// are placed by the framebuffer pixel, so they run on unbroken across
/// neighboring panels.
fn fill_color(in: VsOut) -> vec3<f32> {
    let diagonal = (in.clip.x + in.clip.y) / globals.scale_factor;
    let on_line = diagonal - HATCH_PITCH * floor(diagonal / HATCH_PITCH) < 1.0;
    return select(in.fill, in.hatch, in.hatch_flag > 0.5 && on_line);
}

/// 1.0 where a dashed frame's band is on at the fragment, 0.0 in a gap.
///
/// The dashes run clockwise from the box's top-left corner, 3 logical px on
/// and 2 off. The fragment measures its place along the side of the box it is
/// nearest. A tie goes to the top or the bottom side, so a corner pixel
/// continues that side's run.
fn dash_mask(in: VsOut) -> f32 {
    let size = in.box_max - in.box_min;
    let to_min = in.quad_px - in.box_min;
    let to_max = in.box_max - in.quad_px;
    let near = min(to_min, to_max);

    let along_top = to_min.x;
    let along_right = size.x + to_min.y;
    let along_bottom = size.x + size.y + to_max.x;
    let along_left = 2.0 * size.x + size.y + to_max.y;
    let along = select(
        select(along_left, along_right, to_max.x < to_min.x),
        select(along_bottom, along_top, to_min.y <= to_max.y),
        near.y <= near.x
    );

    let period = 5.0 * globals.scale_factor;
    return select(0.0, 1.0, fract(along / period) < 0.6);
}

fn coverage_of(in: VsOut) -> Coverage {
    let p = in.quad_px;
    let radius = box_radius(in);
    let box_sdf = box_distance(in);

    // Frame band inside the box edge, weighted by the border style.
    var stroke = line_coverage(in.style, -box_sdf);
    if in.style == STYLE_DASHED {
        stroke *= dash_mask(in);
    }
    // Optional interior fill, covering the exact area of the pixel that falls
    // inside the rounded edge. The ramp is one pixel wide, so an opaque box
    // meets what surrounds it at a crisp edge.
    let interior = clamp(0.5 - box_sdf, 0.0, 1.0);
    // A fillet carries the fill past a bottom corner, outside the box.
    let r = in.fillet_radius;
    let bottom_left = vec2<f32>(in.box_min.x, in.box_max.y);
    let fillet = max(
        select(0.0, fillet_coverage(p, bottom_left, -1.0, r), (in.shape & FILLET_BOTTOM_LEFT) != 0u),
        select(0.0, fillet_coverage(p, in.box_max, 1.0, r), (in.shape & FILLET_BOTTOM_RIGHT) != 0u)
    );
    let fill_alpha = fill_weight(in) * max(interior, fillet);

    // The shadow rectangle is the box shifted by the offset, blurred by a
    // gaussian that reaches zero at the margin. A zero margin means no shadow.
    let offset = in.shadow.xy;
    let margin = in.shadow.z;
    let sigma = margin / 3.0;
    let shadow_min = in.box_min + offset;
    let shadow_max = in.box_max + offset;
    // A halo (mode 3) paints nothing below the box's bottom edge or right of
    // its right edge, so a box docked in the window's bottom-right corner meets
    // what sits below and right of it with no shadow between them. A cast
    // (mode 4) paints only below that edge and between the box's sides, as a
    // pane clips the shadow of a pinned row as wide as the pane. Each cut ends
    // over one pixel rather than mid-pixel.
    let casts_down = in.shadow_mode > 3.5;
    let halo = in.shadow_mode > 2.5 && !casts_down;
    let beside = p.x < in.box_min.x || p.x > in.box_max.x;
    let halo_clip = clamp(in.box_max.y - p.y + 0.5, 0.0, 1.0)
        * clamp(in.box_max.x - p.x + 0.5, 0.0, 1.0);
    let clip = select(
        select(1.0, halo_clip, halo),
        select(clamp(p.y - in.box_max.y + 0.5, 0.0, 1.0), 0.0, beside),
        casts_down
    );
    // Exterior shadow for drop (mode 0), halo (mode 3), and cast (mode 4), gated
    // to the box exterior so an unfilled panel's interior is not washed by its
    // own shadow. `interior` is 1 inside the box and 0 outside.
    let peak = select(select(SHADOW_ALPHA, SHADOW_ALPHA_HALO, halo), SHADOW_ALPHA_CAST, casts_down);
    let shadow_base = select(
        0.0,
        peak * shadow_alpha(p, shadow_min, shadow_max, radius, sigma),
        margin > 0.0 && !casts_band(in.shadow_mode)
    ) * (1.0 - interior) * clip;
    // Tucked (mode 1) and overhang (mode 2): a band inside the box that rises
    // from its bottom edge and fades out across the margin. Tucked shades a
    // raised card with the gaussian edge an inset blur casts, and the fainter
    // overhang reads as a panel tucked under whatever overhangs it above.
    let rise = in.box_max.y - p.y;
    let tucked = SHADOW_ALPHA_TUCKED * 0.5 * (1.0 - erf(vec2<f32>(rise * sqrt(0.5) / sigma)).x);
    let overhang = SHADOW_ALPHA_OVERHANG * (1.0 - smoothstep(0.0, margin, rise));
    let band = select(
        0.0,
        select(overhang, tucked, in.shadow_mode < 1.5) * interior,
        casts_band(in.shadow_mode)
    );

    return Coverage(stroke, fill_alpha, shadow_base, band);
}

/// Hand an accumulated premultiplied color back as the unpremultiplied color the
/// pipeline's blend expects.
///
/// A zero-coverage fragment paints nothing, so what the guard returns for it
/// does not matter.
fn unpremultiply(acc: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(acc.rgb / max(acc.a, 1.0e-5), acc.a);
}

/// Everything of the panel that sits beneath the text it frames: the exterior
/// shadow, the optional fill, and the bottom band cast onto that fill.
@fragment
fn fs_under(in: VsOut) -> @location(0) vec4<f32> {
    if occluded(in.clip.xy, in.instance) {
        discard;
    }

    // More than a pixel inside the edge, the interior coverage is whole, so the
    // exterior shadow is zero, and above a band's margin the band is zero too.
    // The fill alone reaches such a pixel. These fragments return before
    // coverage_of, which pays for the blur on every fragment it runs for, and a
    // modal's box is most of its quad.
    let above_band = !casts_band(in.shadow_mode) || in.box_max.y - in.quad_px.y > in.shadow.z;
    if box_distance(in) < -1.0 && above_band {
        let fill = fill_weight(in);
        if fill <= 0.0 {
            discard;
        }
        return unpremultiply(over(vec4<f32>(0.0), fill_color(in), fill));
    }

    let c = coverage_of(in);

    var acc = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    acc = over(acc, SHADOW_COLOR, c.shadow);
    acc = over(acc, fill_color(in), c.fill);
    acc = over(acc, SHADOW_COLOR, c.band);
    return unpremultiply(acc);
}

/// The panel's exterior shadow alone, drawn again over a pane pool's composite.
///
/// A composite repaints its region opaque outside the boxes it leaves to the
/// live grid. Inside such a box the live grid still holds the shadow the under
/// pass drew, so this draw passes it and nothing darkens twice. A later box
/// hides the shadow here as it hides it in the under pass, which keeps the
/// shadow off a later box that a pool paints over.
@fragment
fn fs_shadow(in: VsOut) -> @location(0) vec4<f32> {
    if occluded(in.clip.xy, in.instance) || boxed(in.clip.xy) {
        discard;
    }
    let shadow = coverage_of(in).shadow;
    if shadow <= 0.0 {
        discard;
    }
    return vec4<f32>(SHADOW_COLOR, shadow);
}

/// The panel's frame stroke alone, so it can be recorded above the text the
/// frame surrounds.
///
/// Drawing it separately rather than in one composite gives the same pixels:
/// alpha compositing is associative, so blending the stroke over the result of
/// the under draw equals compositing both and blending once.
@fragment
fn fs_stroke(in: VsOut) -> @location(0) vec4<f32> {
    if occluded(in.clip.xy, in.instance) {
        discard;
    }
    let c = coverage_of(in);

    var acc = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    acc = over(acc, in.border, c.stroke);
    return unpremultiply(acc);
}
