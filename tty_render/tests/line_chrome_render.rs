//! Headless pixel check that an APC cell border survives the glyph it frames.
//!
//! Z-order across passes comes from the record chain, so a decoration pass that
//! records before the text pass loses its lines wherever glyph ink reaches a
//! cell edge. A full block is the adversarial case: it scales to the exact cell
//! box, so it covers every pixel a border would occupy and leaves nothing of a
//! border drawn beneath it. Skips when no GPU adapter is present so a GPU-less
//! CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Border, BorderEdge, BorderStyle, Grid, Panel, PanelShadow, Rgb},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

#[test]
fn a_cell_border_draws_over_the_block_glyph_filling_its_cell() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("line_chrome_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 10.0).round() as u32);

    let surface = Rgb::new(0, 0, 0);
    let border_color = Rgb::new(255, 0, 0);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("line chrome target"),
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
    assert!(rows >= 6 && cols >= 6, "grid too small: {rows}x{cols}");

    // A full block in white, so the glyph's own ink is the one color a surviving
    // red border cannot be confused with.
    let (grow, gcol) = (rows / 2, cols / 2);
    let mut grid = Grid::new(rows, cols);
    {
        let cell = grid.get_mut(grow, gcol);
        cell.ch = '\u{2588}';
        cell.fg = Rgb::new(255, 255, 255);
        cell.bg = surface;
    }
    grid.set_border_edge(
        grow,
        gcol..gcol + 1,
        BorderEdge::Bottom,
        Border {
            style: BorderStyle::Heavy,
            color: border_color,
        },
    );

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

    let pixels = read_back(&device, &queue, &target, width, height);
    let px = |x: u32, y: u32| -> [u8; 3] {
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };

    let x0 = (gcol as f32 * cell_w).floor() as u32;
    let x1 = ((gcol as f32 + 1.0) * cell_w).ceil() as u32;
    let y1 = ((grow as f32 + 1.0) * cell_h).ceil() as u32;

    // The border runs along the cell's bottom edge, so only the last rows of the
    // cell can carry it. The block glyph alone paints every one of them white.
    let mut red = 0;
    for y in y1.saturating_sub(3)..y1.min(height) {
        for x in x0..x1.min(width) {
            let [r, g, b] = px(x, y);
            if r >= 200 && g <= 80 && b <= 80 {
                red += 1;
            }
        }
    }

    assert!(
        red > 0,
        "the bottom border should paint over the block glyph filling the cell, \
         but every pixel along that edge is the glyph's own ink"
    );
}

/// A panel's frame surrounds the cells it frames, so glyph ink reaching a cell
/// edge must not break the line around it. The block glyph is the adversarial
/// case again: it fills its cell box exactly, so it covers every pixel of an
/// edge it sits beside.
#[test]
fn a_panel_frame_draws_over_the_glyphs_it_surrounds() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("line_chrome_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 8.0).round() as u32);

    let surface = Rgb::new(0, 0, 0);
    let target = device.create_texture(&TextureDescriptor {
        label: Some("panel stroke target"),
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
    assert!(rows >= 6 && cols >= 8, "grid too small: {rows}x{cols}");

    let mut grid = Grid::new(rows, cols);
    // Block glyphs on both sides of the panel's left edge, each filling its cell
    // exactly, so the frame has ink to survive on either side of it.
    for col in [2, 3] {
        let cell = grid.get_mut(2, col);
        cell.ch = '\u{2588}';
        cell.fg = Rgb::new(255, 255, 255);
        cell.bg = surface;
    }
    grid.set_panels(vec![Panel {
        top: 1,
        left: 3,
        width: 4,
        height: 3,
        style: BorderStyle::Heavy,
        border: Rgb::new(255, 0, 0),
        corner_radius: 0,
        fill: Some(Rgb::new(0, 0, 60)),
        shadow: PanelShadow::None_,
        inset_x: 0,
        above_pools: false,
        anchor: None,
        seq: 0,
    }]);

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

    let pixels = read_back(&device, &queue, &target, width, height);
    let at = |x: u32, y: u32| -> [u8; 3] {
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };

    // The panel's left edge, at the boundary between the two block glyphs.
    let edge_x = (3.0 * cell_w).round() as u32;
    let mid_y = (2.5 * cell_h) as u32;

    let reddest = (edge_x.saturating_sub(1)..=edge_x + 1)
        .map(|x| at(x, mid_y))
        .max_by_key(|[r, g, _]| i32::from(*r) - i32::from(*g))
        .expect("pixels across the edge");

    assert!(
        reddest[0] > 200 && reddest[1] < 100,
        "the frame survives the glyphs flanking it, got {reddest:?}",
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
        label: Some("line chrome readback"),
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
