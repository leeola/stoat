//! Headless GPU check that the cursor draws over a pool composite.
//!
//! `render_with_pools` composites each pool over the live grid after the base
//! draw, so a cursor painted in the base would be buried under the pool covering
//! its cell. The cursor now draws on top via [`Renderer::draw_cursor_over`], and
//! a scissor holds it to its pane. This
//! composites a gray pool over the whole grid, then draws the cursor into one
//! cell, and reads the pixels back to assert that cell brightened (the cursor
//! sits above the pool) while its neighbour did not. A second pass scissors the
//! cursor to a band that excludes its cell and asserts it vanishes. Skips when
//! no GPU adapter is present, so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

#[test]
fn cursor_draws_over_pool_and_obeys_its_scissor() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("cursor_pool_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let [cell_w, cell_h] = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);
    let (width, height) = (128u32, cell_h * 3);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);
    let gray = Rgb::new(80, 80, 80);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("cursor pool target"),
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
    assert!(rows >= 2 && cols >= 6, "grid too small: {rows}x{cols}");

    let base = Grid::new(rows, cols);
    let mut pool = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            pool.get_mut(r, c).bg = gray;
        }
    }

    let no_decoration = Damage::Partial(Vec::new());
    let resolution = [width as f32, height as f32];
    let full = [0, 0, width, height];
    // The cursor block covers cell (0, 1): corners are its cell extent.
    let corners = [[0.0, 1.0], [1.0, 1.0], [0.0, 2.0], [1.0, 2.0]];

    let render = |renderer: &mut Renderer, cursor: Option<[[f32; 2]; 4]>, scissor| {
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
        renderer.composite_pool(&device, &queue, &view, &pool, &[], full, 0.0, true, true);
        renderer.draw_cursor_over(&device, &queue, &view, resolution, cursor, 0.0, scissor);
        read_back(&device, &queue, &target, width, height)
    };

    // Sum brightness over the cell at (col, row 1).
    let cell = |pixels: &[u8], col: u32| {
        let mut sum = 0u64;
        for y in cell_h..(cell_h * 2) {
            for x in (col * cell_w)..((col + 1) * cell_w) {
                let i = ((y * width + x) * 4) as usize;
                sum += pixels[i] as u64 + pixels[i + 1] as u64 + pixels[i + 2] as u64;
            }
        }
        sum
    };

    let without = render(&mut renderer, None, None);
    let with = render(&mut renderer, Some(corners), None);

    assert!(
        cell(&with, 0) > cell(&without, 0),
        "the cursor should brighten its cell over the pool: {} vs {}",
        cell(&with, 0),
        cell(&without, 0)
    );
    assert_eq!(
        cell(&with, 4),
        cell(&without, 4),
        "a cell away from the cursor is unchanged"
    );

    // Scissor the cursor to row 2's band, which excludes its cell on row 1, so
    // the block is clipped away entirely.
    let clipped = render(
        &mut renderer,
        Some(corners),
        Some([0, cell_h * 2, width, cell_h]),
    );
    assert_eq!(
        cell(&clipped, 0),
        cell(&without, 0),
        "a scissor excluding the cursor cell clips the block away"
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
        label: Some("cursor pool readback"),
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
