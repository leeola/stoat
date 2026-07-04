//! Headless smoke test for the perf HUD.
//!
//! Builds the HUD pipeline on a real device and composites it over a rendered
//! frame off-screen, so a bind-group-versus-shader mismatch or an invalid pass
//! surfaces as a wgpu uncaptured-error panic rather than only in a live window.
//! Skips when no GPU adapter is present so GPU-less CI stays green. Compiled
//! only under the `perf` feature, where the HUD and `FrameSample` exist.

#![cfg(feature = "perf")]

use std::time::Duration;
use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    perf::FrameSample,
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::{
    Extent3d, PollType, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

fn sample(cpu_ms: f32) -> FrameSample {
    FrameSample {
        acquire: Duration::from_secs_f32(cpu_ms / 1000.0),
        encode: Duration::ZERO,
        present: Duration::ZERO,
        interval: Duration::ZERO,
        gpu: None,
    }
}

#[test]
fn hud_composites_over_a_frame_off_screen() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("hud_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let (width, height) = (256, 128);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("hud headless target"),
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
    let grid = Grid::new(rows, cols);
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
            damage: &Damage::Full,
            decoration_damage: &Damage::Partial(Vec::new()),
        },
    );

    // Bars spanning all three budget bands plus an empty series (panel and
    // hairlines only), so both the full and degenerate instance sets record.
    let samples = [sample(4.0), sample(12.0), sample(24.0)];
    renderer.draw_hud_over(
        &device,
        &queue,
        &view,
        &samples,
        [width as f32, height as f32],
    );
    renderer.draw_hud_over(&device, &queue, &view, &[], [width as f32, height as f32]);

    // Reaching here without a wgpu uncaptured-error panic is the assertion.
    let _ = device.poll(PollType::wait_indefinitely());
}
