//! Headless smoke test: build the grid render passes against a real device and
//! draw one frame off-screen.
//!
//! The `shader_is_valid_wgsl` unit tests validate WGSL in isolation but never
//! build a pipeline, so a bind-group-layout-versus-shader mismatch (a uniform
//! used in a stage the layout omits) only surfaces when a real device runs
//! `create_render_pipeline`. This test reaches that path and the draw path,
//! skipping when no GPU adapter is present so GPU-less CI stays green.

use stoatty_render::gpu::{headless_device, Renderer, Scroll};
use stoatty_term::grid::{Border, BorderStyle, Grid, Overlay, Rgb};
use wgpu::{
    Extent3d, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

#[test]
fn builds_passes_and_draws_a_frame_off_screen() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("headless_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let (width, height) = (256, 128);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("headless target"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&TextureViewDescriptor::default());

    let mut renderer = Renderer::new(
        &device,
        format,
        width,
        height,
        30,
        Rgb::new(0, 0, 0),
        Rgb::new(217, 217, 217),
    );

    // Populate the grid so every pass draws something: a glyph, a rounded
    // border, and an overlay with content. The cursor exercises the cursor pass.
    let (rows, cols) = renderer.grid_size();
    let mut grid = Grid::new(rows, cols);
    grid.get_mut(0, 0).ch = 'A';
    grid.get_mut(0, 0).borders.top = Some(Border {
        style: BorderStyle::Rounded,
        color: Rgb::new(255, 0, 0),
    });
    grid.set_overlays(vec![Overlay {
        top: 1,
        left: 1,
        width: 6,
        height: 3,
        fill: Rgb::new(20, 20, 40),
        border: Rgb::new(200, 200, 255),
        content_fg: Rgb::new(255, 255, 255),
        content: "ok".to_owned(),
    }]);

    // A validation error in pipeline creation (Renderer::new) or in encoding and
    // submitting the draw (render_into) triggers wgpu's default uncaptured-error
    // panic, failing this test. Both are synchronous, so reaching the end without
    // a panic is the assertion.
    renderer.render_into(
        &device,
        &queue,
        &view,
        &grid,
        Some([0.0, 0.0]),
        Scroll {
            popover: 0.0,
            grid: 0.0,
        },
    );
}
