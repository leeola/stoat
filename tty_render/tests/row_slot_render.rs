//! Headless GPU check that the vertex stages recover a row from the slot an
//! instance carries.
//!
//! The text pass stores each instance's buffer slot rather than the display row
//! it paints, and text.wgsl inverts that. The pass's own tests transcribe the
//! shader's arithmetic to check the round trip without a device, which pins the
//! two halves against the transcription but never against the shader itself.
//! This renders instances whose rows are far enough apart to read off the
//! surface and asserts each lands in its own band, so a shader that inverted the
//! rotation the wrong way or wrapped a screen-anchored row is caught. Skips when
//! no GPU adapter is present, so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Grid, Overlay, Rgb, UnderlineStyle},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

const ROWS: u32 = 6;

/// Underlines are grid-row instances, so each one's slot is rotated and the
/// shader has to take it back. Two rows apart in one frame catch a stage that
/// dropped the row term or folded every slot onto one row.
#[test]
fn an_underline_paints_on_the_row_its_slot_names() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("row_slot_render: no wgpu adapter available, skipping");
        return;
    };

    let color = Rgb::new(240, 40, 40);
    let mut grid = Grid::new(ROWS as usize, 8);
    for row in [1usize, 4] {
        let cell = grid.get_mut(row, 0);
        cell.underline = UnderlineStyle::Straight;
        cell.underline_color = color;
    }

    let painted = rows_painting(&device, &queue, &grid, color);

    assert_eq!(
        painted,
        vec![1, 4],
        "each underline must paint in the row its slot names"
    );
}

/// An overlay's content rows are positions its builder chose, not grid rows, so
/// the screen-anchored draws carry no rotation at all. A stage that wrapped them
/// at the grid height would fold a box's later lines back over its first.
#[test]
fn overlay_content_lines_do_not_wrap_at_the_grid_height() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("row_slot_render: no wgpu adapter available, skipping");
        return;
    };

    let content_fg = Rgb::new(20, 240, 60);
    let mut grid = Grid::new(ROWS as usize, 12);
    grid.set_overlays(vec![Overlay {
        top: 0,
        left: 0,
        width: 10,
        height: 5,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg,
        scale: 1,
        offset: [0, 0],
        bold: false,
        // Three lines, laid out from the row below the box's top edge.
        content: "MMM\nMMM\nMMM".to_owned(),
    }]);

    let painted = rows_painting(&device, &queue, &grid, content_fg);

    assert_eq!(
        painted,
        vec![1, 2, 3],
        "each content line must keep its own row"
    );
}

/// The grid rows carrying a pixel within tolerance of `color`, in order.
///
/// Sampled across each row's full band rather than at one point, since a glyph
/// covers only part of a cell and its coverage is blended against what is
/// behind it.
fn rows_painting(device: &Device, queue: &Queue, grid: &Grid, color: Rgb) -> Vec<u32> {
    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let [cell_w, cell_h] = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);
    // Widened to a whole number of readback rows. A texture copy's bytes per row
    // must be 256-aligned, and the surplus columns paint the background.
    let width = (cell_w * grid.cols() as u32).next_multiple_of(64);
    let height = cell_h * ROWS;

    let target = device.create_texture(&TextureDescriptor {
        label: Some("row slot target"),
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
        device,
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

    renderer.render_into(
        device,
        queue,
        &view,
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

    let pixels = read_back(device, queue, &target, width, height);
    let near = |i: usize| {
        let channels = [
            (pixels[i], color.r),
            (pixels[i + 1], color.g),
            (pixels[i + 2], color.b),
        ];
        channels.iter().all(|(got, want)| got.abs_diff(*want) <= 24)
    };

    (0..ROWS)
        .filter(|row| {
            let band = (row * cell_h)..((row + 1) * cell_h).min(height);
            band.flat_map(|y| (0..width).map(move |x| ((y * width + x) * 4) as usize))
                .any(near)
        })
        .collect()
}

fn read_back(
    device: &Device,
    queue: &Queue,
    texture: &Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("row slot readback"),
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
