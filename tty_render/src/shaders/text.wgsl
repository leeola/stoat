// Instanced per-glyph text pass. One instance per visible cell glyph; the six
// quad corners are generated from the vertex index, so the only vertex buffer
// is the per-glyph instance stream.
//
// The fragment composites the glyph over its cell background in linear light,
// then encodes back to sRGB, so thin glyphs on dark backgrounds keep their
// weight. The cell background travels in the instance because the shader
// cannot read the framebuffer; the background pass has already painted that
// same color underneath, so an opaque composited write is correct.

struct Globals {
    resolution: vec2<f32>,
    cell_size: vec2<f32>,
    scroll_y: f32,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

@group(1) @binding(0) var mask_atlas: texture_2d<f32>;
@group(1) @binding(1) var color_atlas: texture_2d<f32>;
@group(1) @binding(2) var atlas_sampler: sampler;

const KIND_MASK: u32 = 0u;
const KIND_COLOR: u32 = 1u;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) fg: vec3<f32>,
    @location(2) @interpolate(flat) bg: vec3<f32>,
    @location(3) @interpolate(flat) kind: u32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) pos: vec2<f32>,
    @location(1) dim: vec2<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) fg: vec3<f32>,
    @location(4) bg: vec3<f32>,
    @location(5) kind: u32,
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
    out.bg = bg;
    out.kind = kind;
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let cutoff = c <= vec3<f32>(0.04045);
    let lower = c / 12.92;
    let higher = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let cutoff = c <= vec3<f32>(0.0031308);
    let lower = c * 12.92;
    let higher = pow(c, vec3<f32>(1.0 / 2.4)) * 1.055 - 0.055;
    return select(higher, lower, cutoff);
}

fn srgb_to_linear_1(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn linear_to_srgb_1(c: f32) -> f32 {
    if c <= 0.0031308 {
        return c * 12.92;
    }
    return pow(c, 1.0 / 2.4) * 1.055 - 0.055;
}

fn luminance(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

// Remap glyph coverage so the linear-space blend reproduces the heavier weight
// of a gamma-incorrect blend, keeping thin stems from washing out. The blend
// is evaluated on luminance in sRGB space and mapped back to the alpha a linear
// interpolation from bg to fg would need. Lifted from ghostty's linear
// correction.
fn correct_coverage(coverage: f32, fg_lin: vec3<f32>, bg_lin: vec3<f32>) -> f32 {
    let fg_l = luminance(fg_lin);
    let bg_l = luminance(bg_lin);
    if abs(fg_l - bg_l) <= 0.001 {
        return coverage;
    }
    let blend = linear_to_srgb_1(fg_l) * coverage + linear_to_srgb_1(bg_l) * (1.0 - coverage);
    let blend_l = srgb_to_linear_1(blend);
    return clamp((blend_l - bg_l) / (fg_l - bg_l), 0.0, 1.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let bg_lin = srgb_to_linear(in.bg);

    if in.kind == KIND_COLOR {
        let texel = textureSampleLevel(color_atlas, atlas_sampler, in.uv, 0.0);
        let fg_lin = srgb_to_linear(texel.rgb);
        let blended = mix(bg_lin, fg_lin, texel.a);
        return vec4<f32>(linear_to_srgb(blended), 1.0);
    }

    let coverage = textureSampleLevel(mask_atlas, atlas_sampler, in.uv, 0.0).r;
    let fg_lin = srgb_to_linear(in.fg);
    let a = correct_coverage(coverage, fg_lin, bg_lin);
    let blended = mix(bg_lin, fg_lin, a);
    return vec4<f32>(linear_to_srgb(blended), 1.0);
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

    let pixel = cell_pos + corner * globals.cell_size;
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
    let pos = in.local * globals.cell_size;
    let coverage = underline_coverage(in.style, pos, globals.cell_size);
    return vec4<f32>(in.color, coverage);
}
