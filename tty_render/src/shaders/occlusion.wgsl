// Shared occlusion geometry, prepended to every pass that hides fragments under
// a modal box. WGSL has no include, so the passes would otherwise each carry
// their own copy and drift apart on the shape they occlude by.
//
// A panel is not the rect it declares. Each of its edges stands inside its
// cells or reaches across the cell beside them, and it rounds the corners its
// mask names. A pass that hides fragments by the bare rect punches a square
// notch out of whatever sits beneath each rounded corner, hides what shows in
// a chip's bare strips, and misses the strips a box reaches across.

// One rect per live modal box, in whole-cell units, plus the edges, corner
// radius, and corner mask the panel actually draws with and its
// declaration-order seq. The edge pixels and the radius are physical pixels,
// the edge modes pack as edge_offsets reads them, and the mask holds bits 0-3
// from top-left clockwise.
struct Occluder {
    cell: vec2<f32>,
    size: vec2<f32>,
    edge_px: vec4<f32>,
    seq: u32,
    corner_radius: f32,
    edge_modes: u32,
    corners: u32,
}

// The modes of a panel's edges other than an inset, which is mode 0. A mode
// word holds two bits per edge from the low pair, in the order top, right,
// bottom, left.
const EDGE_MARGIN: u32 = 1u;
const EDGE_GAP: u32 = 2u;
const EDGE_OUTSET: u32 = 3u;

// Each edge's offset into the box from its cell edge, in physical pixels, in the
// order top, right, bottom, left. A negative offset reaches out across the
// cell beside the box.
//
// An inset stands `px` inside the cell edge. A margin crosses the neighbor cell
// and stops `px` short of its far side. A gap reaches half as far as that
// margin, with the top and left halves rounded down and the bottom and right
// halves rounded up, so two neighbors that both state a gap leave `px` between
// them in a cell of any size. An outset stands `px` outside the cell edge.
fn edge_offsets(edge_px: vec4<f32>, edge_modes: u32, cell_size: vec2<f32>) -> vec4<f32> {
    let modes = vec4<u32>(edge_modes, edge_modes >> 2u, edge_modes >> 4u, edge_modes >> 6u) & vec4<u32>(3u);
    let extent = vec4<f32>(cell_size.y, cell_size.x, cell_size.y, cell_size.x);
    let reach = extent - edge_px;
    let half = reach * 0.5;
    let gap = vec4<f32>(floor(half.x), ceil(half.y), ceil(half.z), floor(half.w));
    let margin_or_inset = select(edge_px, -reach, modes == vec4<u32>(EDGE_MARGIN));
    let gap_margin_or_inset = select(margin_or_inset, -gap, modes == vec4<u32>(EDGE_GAP));
    return select(gap_margin_or_inset, -edge_px, modes == vec4<u32>(EDGE_OUTSET));
}

// Signed distance to a rounded rectangle of half-size `half` and corner radius
// `r` centered at the origin, negative inside.
fn rounded_box_sdf(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

// Signed distance from `frag`, in physical pixels, to the body a box actually
// draws, negative inside.
//
// The body is the cell rect with each edge moved by edge_offsets. The
// fragment's quadrant names the corner it lies toward, and that corner takes
// `corner_radius` when `corners` holds its bit and stays square when it does
// not.
//
// Callers discard at -0.5 rather than 0, which leaves the outer half-pixel ring
// standing so the box's own anti-aliased edge blends over the chrome beneath it
// instead of over a hard hole.
fn occluder_sdf(
    frag: vec2<f32>,
    cell: vec2<f32>,
    size: vec2<f32>,
    cell_size: vec2<f32>,
    corner_radius: f32,
    edge_px: vec4<f32>,
    edge_modes: u32,
    corners: u32,
) -> f32 {
    let offsets = edge_offsets(edge_px, edge_modes, cell_size);
    let box_min = cell * cell_size + offsets.wx;
    let box_max = (cell + size) * cell_size - offsets.yz;
    let center = (box_min + box_max) * 0.5;
    let half = (box_max - box_min) * 0.5;
    let p = frag - center;
    let corner = select(select(0u, 1u, p.x > 0.0), select(3u, 2u, p.x > 0.0), p.y > 0.0);
    let rounded = ((corners >> corner) & 1u) == 1u;
    let radius = select(0.0, min(corner_radius, min(half.x, half.y)), rounded);
    return rounded_box_sdf(p, half, radius);
}
