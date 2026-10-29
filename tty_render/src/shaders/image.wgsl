// Image pass. One instance per placement: a pixel rectangle textured from the
// image the client transmitted, scaled into the cell box the placement names.
//
// The quad rides in absolute pixels rather than cell-fraction units. A
// placement's box is measured out in cells, but its edges land wherever the
// scaling and the intra-cell offset put them, so the cell grid is the wrong
// unit to carry it in.
//
// The texture holds premultiplied texels and the output is premultiplied too,
// matching the text pass's color branch, so a partially transparent image
// composites over whatever the passes before it left in the framebuffer.

struct Globals {
    resolution: vec2<f32>,
    pad0: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> globals: Globals;

@group(1) @binding(0)
var image: texture_2d<f32>;

@group(1) @binding(1)
var image_sampler: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    // The pixel y at and below which the placement paints nothing. The quad
    // keeps the height of the cell box the image scales into, so the clip is
    // what ends a placement above the last row where that row starts.
    @location(1) @interpolate(flat) clip_y: f32,
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_min: vec2<f32>,
    @location(3) uv_max: vec2<f32>,
    @location(4) clip_y: f32,
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
    let pixel = origin + corner * size;

    var out: VsOut;
    out.clip = vec4<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0,
        0.0,
        1.0
    );
    out.uv = mix(uv_min, uv_max, corner);
    out.clip_y = clip_y;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if in.clip.y >= in.clip_y {
        discard;
    }

    // Premultiplied on the way into the texture, because the sampler filters
    // linearly and a filter has to average color the alpha is already folded
    // into. Multiplying here instead would halve the color of every sample that
    // straddles a transparent texel a second time.
    return textureSample(image, image_sampler, in.uv);
}
