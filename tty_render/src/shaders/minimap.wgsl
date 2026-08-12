// Minimap pass. One instance per run block, plus a background and a viewport
// thumb quad, filling the right-edge minimap strip. Unlike the bar pass, a
// minimap quad is given in absolute pixels rather than cell-fraction units,
// since a minimap column is a fraction of a pixel and the cell grid is too
// coarse. The globals still carry the cell size so the occluder test can map a
// panel's cell rect to pixels.
//
// A quad smaller than a pixel is the ordinary case here, not an edge case, so
// the pass resolves coverage analytically. The vertex stage draws the quad
// grown out to whole pixels and hands the true rect along. The fragment stage
// emits how much of that rect its own pixel holds as alpha. Without it, and
// with no MSAA, a run renders only where it crosses a pixel center: short runs
// vanish, equal runs come out unequal, and the fractional y of a scrolled strip
// pops blocks in and out.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    panel_count: u32,
    // 1 discards a fragment inside any occluder regardless of seq; 0 keeps the
    // seq test. The minimap never composites under a pool, so it always passes 0.
    occlude_all: u32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// The live modal boxes. A minimap fragment is discarded inside any occluder
// whose seq exceeds the strip's own, so a box hides the strip beneath its body.
@group(0) @binding(1)
var<storage, read> occluders: array<Occluder>;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
    @location(1) @interpolate(flat) seq: u32,
    // The quad's true pixel rect, unrounded, which the fragment stage measures
    // its coverage against.
    @location(2) @interpolate(flat) rect_min: vec2<f32>,
    @location(3) @interpolate(flat) rect_max: vec2<f32>,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) seq: u32,
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

    let rect_min = origin;
    let rect_max = origin + size;

    // Grow the drawn quad out to whole pixels, a pixel wide at the least, so
    // every pixel the true rect touches gets a fragment to measure. The strip's
    // scissor crops whatever the growth adds past its edge.
    let quad_min = floor(rect_min);
    let quad_max = max(ceil(rect_max), quad_min + vec2<f32>(1.0, 1.0));

    let pixel = quad_min + corner * (quad_max - quad_min);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.seq = seq;
    out.rect_min = rect_min;
    out.rect_max = rect_max;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Discard where a box declared later (higher seq) covers this strip.
    let frag = in.clip.xy;
    for (var j = 0u; j < globals.panel_count; j = j + 1u) {
        let o = occluders[j];
        if globals.occlude_all == 1u || o.seq > in.seq {
            let sdf = occluder_sdf(
                frag,
                o.cell,
                o.size,
                globals.cell_size,
                o.corner_radius,
                o.inset_x
            );
            if sdf < -0.5 {
                discard;
            }
        }
    }

    // How much of the true rect this fragment's own pixel box holds. A run a
    // third of a pixel wide reads as a third-alpha column rather than either
    // vanishing or claiming a whole pixel, and a strip scrolled by half a pixel
    // hands half its intensity to the row below instead of jumping there.
    let pixel_min = floor(frag);
    let covered = min(in.rect_max, pixel_min + vec2<f32>(1.0, 1.0))
        - max(in.rect_min, pixel_min);
    let overlap = clamp(covered, vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0));

    return vec4<f32>(in.color.rgb, in.color.a * overlap.x * overlap.y);
}
