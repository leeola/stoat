//! Headless GPU check that an oversized pool or cursor scissor is clamped to
//! the render target instead of tripping a validation error.
//!
//! During a live resize the app can hand `composite_pool` and `draw_cursor_over`
//! a scissor sized to a stale grid larger than the freshly shrunk drawable. wgpu
//! aborts the process when a scissor exceeds the render target, so the renderer
//! clamps every caller-supplied scissor. This drives both entry points with a
//! scissor twice the offscreen target's size inside a validation error scope and
//! asserts no error is raised. Skips when no GPU adapter is present, so a
//! GPU-less CI stays green.

use futures::executor;
use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::{
    ErrorFilter, Extent3d, PollType, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureViewDescriptor,
};

#[test]
fn oversized_scissors_are_clamped_not_validated() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("scissor_clamp_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let [_, cell_h] = cell_size(font_size, 1.0);
    let (width, height) = (128u32, cell_h.round() as u32 * 3);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("scissor clamp target"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&TextureViewDescriptor::default());

    let mut renderer = Renderer::new(
        &device,
        format,
        [width, height],
        build_font_system(),
        FontConfig {
            size: font_size,
            scale_factor: 1.0,
            family: &["JetBrains Mono".to_owned()],
            ligatures: true,
        },
        black,
        white,
    );

    let (rows, cols) = renderer.grid_size();
    let base = Grid::new(rows, cols);
    let pool = Grid::new(rows, cols);
    let no_decoration = Damage::Partial(Vec::new());
    let resolution = [width as f32, height as f32];
    let corners = [[0.0, 1.0], [1.0, 1.0], [0.0, 2.0], [1.0, 2.0]];

    // Twice the target in each axis. Without clamping, encoding this scissor
    // raises the validation error that aborts the process in the live app.
    let oversized = [0, 0, width * 2, height * 2];

    let scope = device.push_error_scope(ErrorFilter::Validation);

    renderer.render_into(
        &device,
        &queue,
        &view,
        &base,
        Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers: &[],
            },
            damage: &Damage::Full,
            decoration_damage: &no_decoration,
        },
    );
    renderer.composite_pool(&device, &queue, &view, &pool, oversized, 0.0, true);
    renderer.draw_cursor_over(
        &device,
        &queue,
        &view,
        resolution,
        Some(corners),
        0.0,
        Some(oversized),
    );

    let error_future = scope.pop();
    device.poll(PollType::wait_indefinitely()).expect("poll");
    let error = executor::block_on(error_future);

    assert!(
        error.is_none(),
        "a scissor larger than the target must be clamped, not validated: {error:?}"
    );
}
