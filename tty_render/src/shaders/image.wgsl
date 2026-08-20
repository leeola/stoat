// Image pass. One instance per placement: a pixel rectangle textured from the
// image the client transmitted, scaled into the cell box the placement names.
//
// The quad rides in absolute pixels rather than cell-fraction units. A
// placement's box is measured out in cells, but its edges land wherever the
// scaling and the intra-cell offset put them, so the cell grid is the wrong
// unit to carry it in.
//
// Output is premultiplied, matching the text pass's color branch, so a
// partially transparent image composites over whatever the passes before it
// left in the framebuffer.

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
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_min: vec2<f32>,
    @location(3) uv_max: vec2<f32>,
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
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSample(image, image_sampler, in.uv);
    // The stored pixels are straight alpha, so premultiply here rather than
    // asking the terminal to store a second copy in the blend's own form.
    return vec4<f32>(texel.rgb * texel.a, texel.a);
}
