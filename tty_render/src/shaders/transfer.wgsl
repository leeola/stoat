// Display transfer pass. One full-screen triangle draws the staged frame into
// the target and re-encodes each channel for a display that decodes at gamma
// 2.2. The channel decodes with the piecewise sRGB curve and encodes again at
// 2.2, which is how a Chromium browser presents an sRGB hex on such a display.

@group(0) @binding(0)
var staged: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    // The corners (-1, -1), (3, -1), and (-1, 3) put the clip square inside
    // one triangle.
    let corner = vec2<f32>(f32((vertex_index << 1u) & 2u), f32(vertex_index & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = textureLoad(staged, vec2<i32>(position.xy), 0);
    let srgb = texel.rgb;
    let linear = select(
        pow((srgb + 0.055) / 1.055, vec3<f32>(2.4)),
        srgb / 12.92,
        srgb <= vec3<f32>(0.04045)
    );
    // pow of zero is not defined in WGSL, so black passes through as black.
    let encoded = select(
        pow(linear, vec3<f32>(1.0 / 2.2)),
        vec3<f32>(0.0),
        linear <= vec3<f32>(0.0)
    );
    return vec4<f32>(encoded, texel.a);
}
