//! Headless GPU check that patching only damaged rows matches a full rebuild.
//!
//! The text pass caches each row's built glyph instances and, on a damaged
//! frame, rebuilds and re-uploads only the changed rows instead of every glyph
//! on screen. This renders a grid, edits a middle row (changing its glyph count
//! so later rows shift, exercising the from-first-changed-row upload), renders
//! once with partial damage (the incremental path) and once with full damage,
//! and asserts the two frames are pixel-identical. Skips when no GPU adapter is
//! present, so a GPU-less CI stays green.

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
fn patched_rows_match_a_full_rebuild() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("damaged_rows_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    let (width, height) = (256u32, cell_h * 4);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("damaged rows target"),
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
    assert!(rows >= 3 && cols >= 12, "grid too small: {rows}x{cols}");
    let mut grid = Grid::new(rows, cols);
    fill_row(&mut grid, 0, "first row", white, black);
    fill_row(&mut grid, 1, "middle", white, black);
    fill_row(&mut grid, 2, "last row", white, black);

    let render = |renderer: &mut Renderer, grid: &Grid, damage: &Damage| {
        renderer.render_into(
            &device,
            &queue,
            &view,
            grid,
            Frame {
                cursor: None,
                scroll: Scroll {
                    grid: 0.0,
                    region: 0.0,
                    popovers: &[],
                },
                damage,
            },
        );
        read_back(&device, &queue, &target, width, height)
    };

    // Build the original grid, then edit a middle row so the rows after it shift.
    let original = render(&mut renderer, &grid, &Damage::Full);
    fill_row(&mut grid, 1, "much longer middle row", white, black);

    // Incremental: only row 1 is damaged, so rows 0 and 2 come from the cache and
    // the upload starts at row 1's offset.
    let incremental = render(
        &mut renderer,
        &grid,
        &Damage::Partial({
            let mut dirty = vec![false; rows];
            dirty[1] = true;
            dirty
        }),
    );

    // Full rebuild of the same edited grid.
    let full = render(&mut renderer, &grid, &Damage::Full);

    assert!(
        incremental != original,
        "editing the middle row should change the frame"
    );
    assert_eq!(
        incremental, full,
        "patching only the damaged row must match a full rebuild"
    );
}

fn fill_row(grid: &mut Grid, row: usize, text: &str, fg: Rgb, bg: Rgb) {
    for (col, ch) in text.chars().enumerate() {
        if col >= grid.cols() {
            break;
        }
        let cell = grid.get_mut(row, col);
        cell.ch = ch;
        cell.fg = fg;
        cell.bg = bg;
    }
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
        label: Some("damaged rows readback"),
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
