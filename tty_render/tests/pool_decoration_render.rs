//! Headless GPU check that a pool grid's bars and text-run backgrounds
//! composite over the page and glide with the eased sub-cell scroll.
//!
//! `composite_pool` draws a pool's slot-bound gutter chrome after the page
//! glyphs. This builds a pool grid with a full-width bar on one row and a
//! run-background rect on another, composites it at no shift and at a one-cell
//! upward shift, and reads the pixels back to assert each decoration paints its
//! color and moves up a row with the shift. Skips when no GPU adapter is
//! present, so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Bar, Grid, Rgb, TextRun},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

#[test]
fn pool_decorations_composite_and_glide_with_the_shift() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("pool_decoration_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let [cell_w, cell_h] = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);
    let (width, height) = (128u32, cell_h * 4);

    let black = Rgb::new(0, 0, 0);
    let page_bg = Rgb::new(10, 20, 30);
    let bar_color = Rgb::new(200, 50, 50);
    let run_bg = Rgb::new(50, 200, 50);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("pool decoration target"),
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
        black,
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 4 && cols >= 6, "grid too small: {rows}x{cols}");

    // The base fills the target with the page background under the composite
    // (which loads rather than clears). The pool carries the same background, a
    // full-width bar on row 1, and a run-background rect on row 2.
    let mut base = Grid::new(rows, cols);
    let mut pool = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            base.get_mut(r, c).bg = page_bg;
            pool.get_mut(r, c).bg = page_bg;
        }
    }
    pool.set_bars(vec![Bar {
        x: 0,
        y: 16,
        width: cols as u16 * 16,
        height: 16,
        color: bar_color,
        seq: 0,
    }]);
    pool.set_text_runs(vec![TextRun {
        col: 0,
        row: 32,
        scale: 256,
        color: black,
        bg: run_bg,
        text: "   ".to_owned(),
        seq: 0,
    }]);

    let no_decoration = Damage::Partial(Vec::new());
    let full = [0, 0, width, height];

    let render = |renderer: &mut Renderer, shift: f32| {
        let plain = Frame {
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
        };
        renderer.render_into(&device, &queue, &view, &base, plain);
        renderer.composite_pool(&device, &queue, &view, &pool, full, shift, true);
        read_back(&device, &queue, &target, width, height)
    };

    // The color at the center of cell (row, col).
    let cell = |pixels: &[u8], row: u32, col: u32| -> (u8, u8, u8) {
        let x = col * cell_w + cell_w / 2;
        let y = row * cell_h + cell_h / 2;
        let i = ((y * width + x) * 4) as usize;
        (pixels[i], pixels[i + 1], pixels[i + 2])
    };
    let rgb = |c: Rgb| (c.r, c.g, c.b);

    let unshifted = render(&mut renderer, 0.0);
    assert_eq!(cell(&unshifted, 1, 5), rgb(bar_color), "bar paints row 1");
    assert_eq!(cell(&unshifted, 2, 1), rgb(run_bg), "run bg paints row 2");
    assert_eq!(
        cell(&unshifted, 0, 5),
        rgb(page_bg),
        "row 0 is page background"
    );

    let shifted = render(&mut renderer, -1.0);
    assert_eq!(
        cell(&shifted, 0, 5),
        rgb(bar_color),
        "bar glides up to row 0"
    );
    assert_eq!(
        cell(&shifted, 1, 1),
        rgb(run_bg),
        "run bg glides up to row 1"
    );
    assert_eq!(
        cell(&shifted, 2, 5),
        rgb(page_bg),
        "row 2 vacated by the shift"
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
        label: Some("pool decoration readback"),
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
