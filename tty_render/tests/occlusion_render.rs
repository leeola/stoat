//! Headless GPU check that a box occludes the bars, text-run backgrounds, and
//! icons declared beneath it.
//!
//! A modal panel is drawn before the off-grid bar, text-run, and icon passes, so
//! without occlusion those primitives paint over the box body. This builds a
//! grid with an unfilled panel (higher `seq`, the upper box) over a full-width
//! bar, a run-background rect, and an icon (lower `seq`, the chrome beneath),
//! renders one frame, and reads the pixels back to assert each primitive is
//! discarded inside the box rect while still painting outside it. Skips when no
//! GPU adapter is present, so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Bar, BorderStyle, Grid, Icon, IconKind, Panel, Rgb, TextRun},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

#[test]
fn a_box_occludes_the_bars_runs_and_icons_beneath_it() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("occlusion_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let [cell_w, cell_h] = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);
    let (width, height) = (128u32, cell_h * 6);

    let modal_bg = Rgb::new(10, 20, 30);
    let bar_color = Rgb::new(200, 50, 50);
    let run_bg = Rgb::new(50, 200, 50);
    let icon_color = Rgb::new(80, 80, 220);
    let border = Rgb::new(128, 128, 128);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("occlusion target"),
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
        Rgb::new(0, 0, 0),
        Rgb::new(0, 0, 0),
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 5 && cols >= 7, "grid too small: {rows}x{cols}");

    // Every cell carries the modal background, so an occluded primitive falls
    // back to it. An unfilled panel over cols 2..6 stands in for the upper box;
    // the chrome beneath it -- a full-width bar, a run-background rect, and an
    // icon -- all carry a lower seq than the panel.
    let mut grid = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            grid.get_mut(r, c).bg = modal_bg;
        }
    }
    grid.set_panels(vec![Panel {
        top: 0,
        left: 2,
        width: 4,
        height: rows as u16,
        style: BorderStyle::Light,
        border,
        corner_radius: 0,
        fill: None,
        shadow: false,
        seq: 100,
    }]);
    grid.set_bars(vec![Bar {
        x: 0,
        y: 16,
        width: cols as u16 * 16,
        height: 16,
        color: bar_color,
        seq: 1,
    }]);
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 32,
        scale: 256,
        color: Rgb::new(0, 0, 0),
        bg: Some(run_bg),
        text: " ".repeat(cols),
        seq: 2,
    }]);
    grid.set_icons(vec![
        Icon {
            top: 3,
            left: 3,
            kind: IconKind::Info,
            color: icon_color,
            size: 1,
            offset: [0, 0],
            seq: 3,
        },
        Icon {
            top: 3,
            left: 0,
            kind: IconKind::Info,
            color: icon_color,
            size: 1,
            offset: [0, 0],
            seq: 3,
        },
    ]);

    let pixels = {
        let frame = Frame {
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
        };
        renderer.render_into(&device, &queue, &view, &grid, frame);
        read_back(&device, &queue, &target, width, height)
    };

    // The color at the center of cell (row, col).
    let cell = |row: u32, col: u32| -> (u8, u8, u8) {
        let x = col * cell_w + cell_w / 2;
        let y = row * cell_h + cell_h / 2;
        let i = ((y * width + x) * 4) as usize;
        (pixels[i], pixels[i + 1], pixels[i + 2])
    };
    let rgb = |c: Rgb| (c.r, c.g, c.b);

    // Col 3 sits inside the box, col 0 outside it. Each primitive is discarded
    // inside the box (falling back to the modal background) and painted outside.
    assert_eq!(cell(1, 0), rgb(bar_color), "bar paints outside the box");
    assert_eq!(cell(1, 3), rgb(modal_bg), "bar is occluded inside the box");

    assert_eq!(cell(2, 0), rgb(run_bg), "run bg paints outside the box");
    assert_eq!(
        cell(2, 3),
        rgb(modal_bg),
        "run bg is occluded inside the box"
    );

    assert_eq!(cell(3, 0), rgb(icon_color), "icon paints outside the box");
    assert_eq!(cell(3, 3), rgb(modal_bg), "icon is occluded inside the box");
}

/// A pane pool composited beneath a box is occluded by it, while a non-pane
/// pool (box content itself) bleeds through.
///
/// Renders a live grid carrying a box, then composites a solid-colored pool over
/// the whole surface. With `occludable` true the pool's cells are discarded
/// inside the box rect, so the box shows there while the pool paints outside it.
/// With `occludable` false the pool paints everywhere, the accepted glide-frame
/// bleed for a pool that is a box's own content. Skips without a GPU adapter.
#[test]
fn a_box_occludes_the_pool_composite_beneath_it() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("occlusion_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let [cell_w, cell_h] = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);
    let (width, height) = (128u32, cell_h * 6);

    let live_bg = Rgb::new(10, 20, 30);
    let pool_bg = Rgb::new(240, 180, 20);
    let border = Rgb::new(128, 128, 128);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("pool occlusion target"),
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
        Rgb::new(0, 0, 0),
        Rgb::new(0, 0, 0),
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 5 && cols >= 7, "grid too small: {rows}x{cols}");

    let panels = vec![Panel {
        top: 0,
        left: 2,
        width: 4,
        height: rows as u16,
        style: BorderStyle::Light,
        border,
        corner_radius: 0,
        fill: None,
        shadow: false,
        seq: 100,
    }];

    // The live grid carries the box over cols 2..6. Its unfilled interior keeps
    // the live background, so an occluded pool cell falls back to it.
    let mut live = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            live.get_mut(r, c).bg = live_bg;
        }
    }
    live.set_panels(panels.clone());

    // The pool is a full viewport of the pool background, standing in for an
    // editor pane's eased page rows.
    let mut pool = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            pool.get_mut(r, c).bg = pool_bg;
        }
    }

    let full = [0, 0, width, height];
    let render_live = |renderer: &mut Renderer| {
        renderer.render_into(
            &device,
            &queue,
            &view,
            &live,
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
    };

    let cell = |pixels: &[u8], row: u32, col: u32| -> (u8, u8, u8) {
        let x = col * cell_w + cell_w / 2;
        let y = row * cell_h + cell_h / 2;
        let i = ((y * width + x) * 4) as usize;
        (pixels[i], pixels[i + 1], pixels[i + 2])
    };
    let rgb = |c: Rgb| (c.r, c.g, c.b);

    render_live(&mut renderer);
    renderer.composite_pool(
        &device, &queue, &view, &pool, &panels, full, 0.0, true, true,
    );
    let occluded = read_back(&device, &queue, &target, width, height);
    assert_eq!(
        cell(&occluded, 2, 0),
        rgb(pool_bg),
        "the pool paints outside the box"
    );
    assert_eq!(
        cell(&occluded, 2, 3),
        rgb(live_bg),
        "the pane pool is occluded inside the box"
    );

    // A non-pane pool is box content, so it is never occluded: the same pool
    // composited with occludable=false paints through the box.
    render_live(&mut renderer);
    renderer.composite_pool(
        &device, &queue, &view, &pool, &panels, full, 0.0, true, false,
    );
    let bled = read_back(&device, &queue, &target, width, height);
    assert_eq!(
        cell(&bled, 2, 3),
        rgb(pool_bg),
        "a non-pane pool bleeds through the box"
    );
}

/// Copy `texture` into a mappable buffer and return its RGBA bytes, row-major
/// with no padding (the caller sizes the texture so `4 * width` is 256-aligned).
fn read_back(
    device: &Device,
    queue: &Queue,
    texture: &Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("occlusion readback"),
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
