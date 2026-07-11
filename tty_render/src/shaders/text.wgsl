// Instanced per-glyph text pass. One instance per visible cell glyph; the six
// quad corners are generated from the vertex index, so the only vertex buffer
// is the per-glyph instance stream.
//
// The fragment returns premultiplied-alpha coverage and the pipeline blends it
// over the framebuffer, so a glyph composites over whatever is already there
// (the background pass, a panel, a run-background rect) rather than an assumed
// color. The target is not sRGB, so fixed-function blending with raw coverage
// is a per-channel sRGB-space blend, which keeps thin glyphs on dark
// backgrounds weighted without an explicit gamma correction.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    scroll_y: f32,
    panel_count: u32,
    // 1 discards a fragment inside any occluder regardless of seq, for a pool
    // composite that sits under every box; 0 keeps the seq test.
    occlude_all: u32,
    pad0: u32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

// One rect per live modal box, in whole-cell units, plus its declaration-order
// seq. Only the static globals bound for the text-run draws carry a non-zero
// panel_count, so grid, region, overlay, and pool draws never loop here.
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

@group(1) @binding(0) var mask_atlas: texture_2d<f32>;
@group(1) @binding(1) var color_atlas: texture_2d<f32>;
@group(1) @binding(2) var atlas_sampler: sampler;

const KIND_MASK: u32 = 0u;
const KIND_COLOR: u32 = 1u;

// True when the fragment at `frag` (physical px) lies inside a box that should
// hide it. Under the seq test that is a box declared later (higher seq) than
// `seq`, so a text run beneath an upper box is hidden by it. Text-run glyphs and
// rects carry their run's seq; every other live glyph carries a sentinel seq no
// panel exceeds and leaves panel_count at zero. When occlude_all is set the seq
// test is bypassed and any panel rect hides the fragment, so a pool composite
// beneath every box is occluded whatever seq its glyphs carry.
fn occluded(frag: vec2<f32>, seq: u32) -> bool {
    for (var j = 0u; j < globals.panel_count; j = j + 1u) {
        let o = occluders[j];
        if globals.occlude_all == 1u || o.seq > seq {
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
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) fg: vec3<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) @interpolate(flat) seq: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) pos: vec2<f32>,
    @location(1) dim: vec2<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) fg: vec3<f32>,
    @location(4) kind: u32,
    @location(5) seq: u32,
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

    let pixel = pos + corner * dim + vec2<f32>(0.0, globals.scroll_y);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = mix(uv.xy, uv.zw, corner);
    out.fg = fg;
    out.kind = kind;
    out.seq = seq;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // A text-run glyph beneath a later box is discarded so it cannot show
    // through the box body. Grid, region, overlay, and pool glyphs carry a
    // sentinel seq and a zero panel count, so this never fires for them.
    if occluded(in.clip.xy, in.seq) {
        discard;
    }

    // Premultiplied-alpha coverage. The pipeline blends this over the
    // framebuffer, and because the target is not sRGB, that fixed-function
    // blend is a per-channel sRGB-space mix from the destination to `fg`.
    if in.kind == KIND_COLOR {
        let texel = textureSampleLevel(color_atlas, atlas_sampler, in.uv, 0.0);
        return vec4<f32>(texel.rgb * texel.a, texel.a);
    }

    let coverage = textureSampleLevel(mask_atlas, atlas_sampler, in.uv, 0.0).r;
    return vec4<f32>(in.fg * coverage, coverage);
}

// Decorated-underline pass. One instance per underlined cell draws a quad over
// the whole cell; the fragment paints only the underline shape and leaves the
// rest transparent, so it alpha-blends over the glyphs and background already
// drawn underneath.

const STYLE_STRAIGHT: u32 = 0u;
const STYLE_DOUBLE: u32 = 1u;
const STYLE_CURLY: u32 = 2u;
const STYLE_DOTTED: u32 = 3u;
const STYLE_DASHED: u32 = 4u;

const TAU: f32 = 6.2831853;

struct UnderlineVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) color: vec3<f32>,
    @location(2) @interpolate(flat) style: u32,
}

@vertex
fn vs_underline(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) cell_pos: vec2<f32>,
    @location(1) color: vec3<f32>,
    @location(2) style: u32,
) -> UnderlineVsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0)
    );
    let corner = corners[vertex_index];

    let pixel = cell_pos + corner * globals.cell_size + vec2<f32>(0.0, globals.scroll_y);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: UnderlineVsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.local = corner;
    out.color = color;
    out.style = style;
    return out;
}

// Coverage of a 1px-antialiased horizontal line `half` thick, centered on `y`.
fn hline(pixel_y: f32, center: f32, half: f32) -> f32 {
    return clamp(half - abs(pixel_y - center) + 0.5, 0.0, 1.0);
}

fn underline_coverage(style: u32, pos: vec2<f32>, cell: vec2<f32>) -> f32 {
    let base = cell.y * 0.87;

    if style == STYLE_DOUBLE {
        return max(hline(pos.y, base - 1.5, 0.5), hline(pos.y, base + 1.5, 0.5));
    }
    if style == STYLE_CURLY {
        let center = base + 1.3 * sin(pos.x / cell.x * TAU);
        return hline(pos.y, center, 0.6);
    }
    if style == STYLE_DOTTED {
        let on = select(0.0, 1.0, fract(pos.x / 2.0) < 0.5);
        return hline(pos.y, base, 0.75) * on;
    }
    if style == STYLE_DASHED {
        let on = select(0.0, 1.0, fract(pos.x / 4.0) < 0.6);
        return hline(pos.y, base, 0.75) * on;
    }
    return hline(pos.y, base, 0.75);
}

@fragment
fn fs_underline(in: UnderlineVsOut) -> @location(0) vec4<f32> {
    // A composited pool's underline sits under a box like its glyphs, so
    // occlude_all discards it inside any panel rect. Live underlines leave
    // panel_count at zero, so this never fires for them.
    if occluded(in.clip.xy, 0u) {
        discard;
    }

    let pos = in.local * globals.cell_size;
    let coverage = underline_coverage(in.style, pos, globals.cell_size);
    return vec4<f32>(in.color, coverage);
}

// Run-background pass. One opaque rect per scaled text run, drawn before the
// run's glyphs so they alpha-blend over it (grid cells get their background
// from the background pass instead). The rect also masks any panel hairline the
// run sits over.

struct RectVsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) color: vec3<f32>,
    @location(1) @interpolate(flat) seq: u32,
}

@vertex
fn vs_rect(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) pos: vec2<f32>,
    @location(1) dim: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(3) seq: u32,
) -> RectVsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0)
    );
    let corner = corners[vertex_index];

    let pixel = pos + corner * dim + vec2<f32>(0.0, globals.scroll_y);
    let ndc = vec2<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0
    );

    var out: RectVsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    out.seq = seq;
    return out;
}

@fragment
fn fs_rect(in: RectVsOut) -> @location(0) vec4<f32> {
    // A run-background rect beneath a later box is discarded, like the run's
    // glyphs, so the opaque box masks it instead of it showing through.
    if occluded(in.clip.xy, in.seq) {
        discard;
    }

    return vec4<f32>(in.color, 1.0);
}
