// Image mip generation. Each pass draws one triangle over a whole mip level and
// samples the level above it, so the GPU does the per-texel work of an upload
// rather than the thread that prepares the frame.
//
// Level 0 comes from the raw transmitted texels through fs_premultiply. The
// level matches its source texel for texel, so each sample lands on a texel
// center and reads that texel alone. The sampler filters linearly, and a linear
// filter only averages meaningfully over premultiplied texels. The color a
// transparent texel carries is arbitrary, and an average taken ahead of the
// multiply pulls the result toward that color.
//
// Every later level comes from the one above it through fs_copy. The center of
// a target texel falls on the corner between four texels of an even-sized level
// above, so the bilinear sample there is their 2 by 2 box average. An odd size
// resamples instead, and the clamped sampler repeats the edge texel rather than
// letting the level's edge fade toward nothing.

@group(0) @binding(0)
var source: texture_2d<f32>;

@group(0) @binding(1)
var source_sampler: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One triangle twice the target's size, so its clipped part is exactly the
// target and uv runs from 0 to 1 across it.
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    let corner = vec2<f32>(f32((vertex_index << 1u) & 2u), f32(vertex_index & 2u));

    var out: VsOut;
    out.clip = vec4<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_premultiply(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSample(source, source_sampler, in.uv);
    return vec4<f32>(texel.rgb * texel.a, texel.a);
}

@fragment
fn fs_copy(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(source, source_sampler, in.uv);
}
