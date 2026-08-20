//! Headless pixel check that a placed image reaches the framebuffer.
//!
//! The terminal has held decoded pixels and placements for several steps
//! without anything drawing one, so the question this answers is whether the
//! pass turns a placement into pixels at all, and whether its z-index puts it on
//! the side of the text the client asked for. Skips when no GPU adapter is
//! present so a GPU-less CI stays green.

use std::sync::Arc;
use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Grid, ImageCrop, PlacedImage, Rgb},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

/// A solid image of `color`, as the store would hand one over.
fn solid(width: u32, height: u32, color: [u8; 4]) -> Arc<[u8]> {
    color
        .iter()
        .copied()
        .cycle()
        .take((width * height * 4) as usize)
        .collect::<Vec<u8>>()
        .into()
}

fn placement(rgba: Arc<[u8]>, width: u32, height: u32, z: i32) -> PlacedImage {
    PlacedImage {
        image: 1,
        placement: 0,
        generation: 1,
        rgba,
        width,
        height,
        row: 1,
        col: 1,
        cols: 3,
        rows: 2,
        crop: ImageCrop::default(),
        offset_x: 0,
        offset_y: 0,
        z,
    }
}

#[test]
fn a_placement_paints_its_rect_and_its_z_orders_it_against_the_glyphs() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("image_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 8.0).round() as u32);

    let surface = Rgb::new(0, 0, 0);
    let target = device.create_texture(&TextureDescriptor {
        label: Some("image target"),
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
        surface,
        Rgb::new(255, 255, 255),
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 4 && cols >= 6, "grid too small: {rows}x{cols}");

    // A white block glyph in the cell the placement covers, so the two compete
    // for the same pixels and the z-index decides which wins.
    let mut render = |z: i32| {
        let mut grid = Grid::new(rows, cols);
        {
            let cell = grid.get_mut(1, 1);
            cell.ch = '\u{2588}';
            cell.fg = Rgb::new(255, 255, 255);
            cell.bg = surface;
        }
        grid.set_images(vec![placement(solid(4, 4, [255, 0, 0, 255]), 4, 4, z)]);

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
                scrolled_rows: 0,
            },
        );
        read_back(&device, &queue, &target, width, height)
    };

    // A cell the placement covers but no glyph does, so the image is the only
    // thing that could have painted it.
    let clear_x = (2.5 * cell_w) as u32;
    let clear_y = (1.5 * cell_h) as u32;
    // The glyph's own cell, where the two overlap.
    let glyph_x = (1.5 * cell_w) as u32;
    let glyph_y = (1.5 * cell_h) as u32;

    let at = |pixels: &[u8], x: u32, y: u32| -> [u8; 3] {
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };

    let over = render(0);
    assert_eq!(
        at(&over, clear_x, clear_y),
        [255, 0, 0],
        "the placement paints the rect its cell box covers",
    );
    assert_eq!(
        at(&over, glyph_x, glyph_y),
        [255, 0, 0],
        "and at z 0 it draws over the glyph sharing those pixels",
    );

    let under = render(-1);
    assert_eq!(
        at(&under, clear_x, clear_y),
        [255, 0, 0],
        "a negative z still paints where nothing covers it",
    );
    assert_eq!(
        at(&under, glyph_x, glyph_y),
        [255, 255, 255],
        "but the glyph wins where they overlap",
    );
}

fn read_back(
    device: &Device,
    queue: &Queue,
    texture: &Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("image readback"),
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
    let data = buffer.slice(..).get_mapped_range().to_vec();
    buffer.unmap();
    data
}
