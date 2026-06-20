//! Headless smoke test: build the grid render passes against a real device and
//! draw one frame off-screen.
//!
//! The `shader_is_valid_wgsl` unit tests validate WGSL in isolation but never
//! build a pipeline, so a bind-group-layout-versus-shader mismatch (a uniform
//! used in a stage the layout omits) only surfaces when a real device runs
//! `create_render_pipeline`. This test reaches that path and the draw path,
//! skipping when no GPU adapter is present so GPU-less CI stays green.

use stoatty_render::gpu::{
    build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll,
};
use stoatty_term::{
    grid::{Bar, Border, BorderStyle, Grid, Icon, IconKind, Overlay, Rgb, ScrollRegion, TextRun},
    term::Damage,
};
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

    // Populate the grid so every pass draws something: a glyph, a rounded
    // border, and an overlay with content. The cursor exercises the cursor pass.
    let (rows, cols) = renderer.grid_size();
    let mut grid = Grid::new(rows, cols);
    grid.get_mut(0, 0).ch = 'A';
    grid.get_mut(0, 0).borders.top = Some(Border {
        style: BorderStyle::Rounded,
        color: Rgb::new(255, 0, 0),
    });
    // Two overlays: a scaled one, and a taller one whose content overflows its
    // box, so the per-overlay scissored sub-range draws and a non-zero per-overlay
    // scroll offset both run against the real device.
    grid.set_overlays(vec![
        Overlay {
            top: 1,
            left: 1,
            width: 6,
            height: 3,
            fill: Rgb::new(20, 20, 40),
            border: Rgb::new(200, 200, 255),
            content_fg: Rgb::new(255, 255, 255),
            scale: 2,
            offset: [0, 0],
            content: "ok".to_owned(),
        },
        Overlay {
            top: 1,
            left: 9,
            width: 5,
            height: 2,
            fill: Rgb::new(40, 20, 20),
            border: Rgb::new(255, 200, 200),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [3, -5],
            content: "aa\nbb\ncc\ndd".to_owned(),
        },
    ]);

    // A scroll region with a glyph inside it, scrolled by a non-zero offset, so
    // the scissored region-text draw runs against the real device too.
    grid.get_mut(0, cols - 1).ch = 'B';
    grid.set_scroll_region(Some(ScrollRegion {
        top: 0,
        left: cols as u16 - 2,
        width: 2,
        height: 2,
        offset: 1,
    }));

    // One icon of each kind, so the SDF icon pass draws all three silhouettes.
    grid.set_icons(vec![
        Icon {
            top: 5,
            left: 0,
            kind: IconKind::Error,
            color: Rgb::new(220, 50, 47),
            size: 1,
        },
        Icon {
            top: 5,
            left: 2,
            kind: IconKind::Warning,
            color: Rgb::new(255, 200, 0),
            size: 1,
        },
        Icon {
            top: 5,
            left: 4,
            kind: IconKind::Info,
            color: Rgb::new(38, 139, 210),
            size: 2,
        },
    ]);

    // A fractional, vertically-centered text run, so the text-run glyph stream
    // shapes at a sub-cell scale and draws against the real device.
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 48,
        scale: 192,
        color: Rgb::new(150, 160, 170),
        bg: Rgb::new(0, 0, 0),
        text: "127".to_owned(),
    }]);

    // Two sub-cell color bars, so the bar pass fills thin rectangles at a
    // cell-fraction position and size against the real device.
    grid.set_bars(vec![
        Bar {
            x: 0,
            y: 80,
            width: 3,
            height: 16,
            color: Rgb::new(220, 50, 47),
        },
        Bar {
            x: 30,
            y: 0,
            width: 1,
            height: 96,
            color: Rgb::new(88, 88, 88),
        },
    ]);

    // A validation error in pipeline creation (Renderer::new) or in encoding and
    // submitting the draw (render_into) triggers wgpu's default uncaptured-error
    // panic, failing this test. Both are synchronous, so reaching the end without
    // a panic is the assertion.
    renderer.render_into(
        &device,
        &queue,
        &view,
        &grid,
        Frame {
            cursor: Some([0.0, 0.0]),
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                region: 1.5,
                popovers: &[0.0, 1.0],
            },
            damage: &Damage::Full,
            decoration_damage: &Damage::Partial(Vec::new()),
        },
    );
}
