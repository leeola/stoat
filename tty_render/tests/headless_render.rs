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
    grid::{
        Bar, Border, BorderEdge, BorderStyle, Grid, Icon, IconKind, Overlay, Panel, PanelShadow,
        Rgb, ScrollRegion, TextRun,
    },
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
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
    grid.set_border_edge(
        0,
        0..1,
        BorderEdge::Top,
        Border {
            style: BorderStyle::Rounded,
            color: Rgb::new(255, 0, 0),
        },
    );
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
            bold: true,
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
            bold: false,
            content: "aa\nbb\ncc\ndd".to_owned(),
        },
    ]);

    // A modal-chrome panel with a fill and shadow, so the panel pass draws its
    // rounded frame under the grid text against the real device.
    grid.set_panels(vec![Panel {
        top: 4,
        left: 2,
        width: 10,
        height: 4,
        style: BorderStyle::Rounded,
        border: Rgb::new(180, 180, 220),
        corner_radius: 6,
        fill: Some(Rgb::new(30, 30, 50)),
        shadow: PanelShadow::Drop,
        inset_x: 0,
        above_pools: false,
        anchor: None,
        seq: 0,
    }]);

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
            offset: [0, 0],
            seq: 0,
        },
        Icon {
            top: 5,
            left: 2,
            kind: IconKind::Warning,
            color: Rgb::new(255, 200, 0),
            size: 1,
            offset: [0, 0],
            seq: 0,
        },
        Icon {
            top: 5,
            left: 4,
            kind: IconKind::Info,
            color: Rgb::new(38, 139, 210),
            size: 2,
            offset: [3, 6],
            seq: 0,
        },
    ]);

    // A fractional, vertically-centered text run, so the text-run glyph stream
    // shapes at a sub-cell scale and draws against the real device.
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 48,
        scale: 192,
        color: Rgb::new(150, 160, 170),
        bg: Some(Rgb::new(0, 0, 0)),
        text: "127".into(),
        seq: 0,
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
            seq: 0,
        },
        Bar {
            x: 30,
            y: 0,
            width: 1,
            height: 96,
            color: Rgb::new(88, 88, 88),
            seq: 0,
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
            cursor_corners: Some([[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]),
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 1.5,
                popovers: &[0.0, 1.0],
            },
            damage: &Damage::Full,
            decoration_damage: &Damage::Partial(Vec::new()),
            scrolled_rows: 0,
        },
    );
}

/// Two renderers on one device draw their own grids.
///
/// A second window builds its context on the handles the first already holds,
/// so both renderers create their pipelines, atlases, and buffers against the
/// same device. Nothing in a renderer may reach state another one owns: each
/// clears to its own background and paints its own cells, and neither may show
/// the other's.
#[test]
fn two_renderers_on_one_device_draw_their_own_grids() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("headless_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let (width, height) = (128, 128);
    let build = |background: Rgb| {
        let target = device.create_texture(&TextureDescriptor {
            label: Some("shared device target"),
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
        let renderer = Renderer::new(
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
            background,
            Rgb::new(0, 0, 0),
        );
        (target, view, renderer)
    };

    let mine_bg = Rgb::new(200, 40, 40);
    let theirs_bg = Rgb::new(40, 200, 40);
    let (mine_target, mine_view, mut mine) = build(mine_bg);
    let (theirs_target, theirs_view, mut theirs) = build(theirs_bg);

    // Prepared one after the other and drawn one after the other, which is the
    // order two windows redrawing on one device reach the device in.
    let (rows, cols) = mine.grid_size();
    let mut mine_grid = Grid::new(rows, cols);
    mine_grid.get_mut(0, 0).bg = mine_bg;
    let mut theirs_grid = Grid::new(rows, cols);
    theirs_grid.get_mut(0, 0).bg = theirs_bg;

    render_blank(&device, &queue, &mine_view, &mut mine, &mine_grid);
    render_blank(&device, &queue, &theirs_view, &mut theirs, &theirs_grid);

    let corner = |target: &wgpu::Texture| {
        let pixels = read_back(&device, &queue, target, width, height);
        (pixels[0], pixels[1], pixels[2])
    };
    let rgb = |c: Rgb| (c.r, c.g, c.b);

    assert_eq!(
        (corner(&mine_target), corner(&theirs_target)),
        (rgb(mine_bg), rgb(theirs_bg)),
        "each renderer's target holds its own grid, not the one drawn after it",
    );
}

/// Draw `grid` into `view` with nothing eased and everything damaged.
fn render_blank(
    device: &Device,
    queue: &Queue,
    view: &wgpu::TextureView,
    renderer: &mut Renderer,
    grid: &Grid,
) {
    renderer.render_into(
        device,
        queue,
        view,
        grid,
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
            scrolled_rows: 0,
        },
    );
}

/// Copy `texture` into a mappable buffer and return its RGBA bytes, row-major
/// with no padding (the caller sizes the texture so `4 * width` is 256-aligned).
fn read_back(
    device: &Device,
    queue: &Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("shared device readback"),
        size: u64::from(width * height * 4),
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        },
        TexelCopyBufferInfo {
            buffer: &buffer,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: None,
            },
        },
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    buffer.slice(..).map_async(MapMode::Read, |_| {});
    device
        .poll(PollType::wait_indefinitely())
        .expect("poll readback");
    buffer.slice(..).get_mapped_range().to_vec()
}
