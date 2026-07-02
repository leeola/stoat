//! Under the `perf` feature, the renderer measures true GPU frame time with
//! timestamp queries and reads it back without blocking present.
//!
//! Renders a handful of off-screen frames against a real device, forcing the
//! async readback to complete each frame, and asserts a nonzero GPU duration
//! eventually lands. Skips when no adapter is present or the adapter lacks
//! `TIMESTAMP_QUERY`, so GPU-less or older CI stays green.

#![cfg(feature = "perf")]

use std::time::Duration;
use stoatty_render::gpu::{
    build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll,
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::{
    Extent3d, Features, PollType, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureViewDescriptor,
};

#[test]
fn timestamp_queries_yield_a_nonzero_gpu_duration() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("gpu_timing: no wgpu adapter available, skipping");
        return;
    };
    if !device.features().contains(Features::TIMESTAMP_QUERY) {
        eprintln!("gpu_timing: adapter lacks TIMESTAMP_QUERY, skipping");
        return;
    }

    let format = TextureFormat::Rgba8Unorm;
    let (width, height) = (256, 128);
    let target = device.create_texture(&TextureDescriptor {
        label: Some("gpu-timing target"),
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
        [width, height],
        build_font_system(),
        FontConfig {
            size: 30,
            scale_factor: 1.0,
            family: &["JetBrains Mono".to_owned()],
            ligatures: true,
        },
        Rgb::new(0, 0, 0),
        Rgb::new(217, 217, 217),
    );
    let (rows, cols) = renderer.grid_size();
    let mut grid = Grid::new(rows, cols);
    grid.get_mut(0, 0).ch = 'A';

    let full = Damage::Full;
    let empty = Damage::Partial(Vec::new());

    // A frame's GPU time is read back a few frames after it renders, so render a
    // handful of frames, forcing each frame's map to complete, until one lands.
    let mut gpu = None;
    for _ in 0..16 {
        renderer.render_into(
            &device,
            &queue,
            &view,
            &grid,
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
                damage: &full,
                decoration_damage: &empty,
            },
        );
        let _ = device.poll(PollType::wait_indefinitely());
        if let Some(duration) = renderer.take_gpu_time() {
            gpu = Some(duration);
            break;
        }
    }

    let gpu = gpu.expect("a GPU duration should land within a few frames");
    assert!(
        gpu > Duration::ZERO,
        "measured GPU duration should be nonzero, got {gpu:?}"
    );
}
