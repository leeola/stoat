//! Headless GPU check that patching only damaged rows matches a full rebuild.
//!
//! The text pass caches each row's built glyph instances and, on a damaged
//! frame, rebuilds and re-uploads only the changed rows instead of every glyph
//! on screen. This renders a grid, edits a middle row (changing its glyph count
//! so later rows shift, exercising the from-first-changed-row upload), renders
//! once with partial damage (the incremental path) and once with full damage,
//! and asserts the two frames are pixel-identical. The edit also recolours one
//! border row, marked via the separate decoration-damage signal, so the same
//! comparison covers the border pass's per-row rebuild. Skips when no GPU adapter
//! is present, so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Border, BorderStyle, Grid, Rgb, UnderlineStyle},
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
    // Underline a cell in row 0, which stays unchanged across the edit below, so
    // the comparison also checks the cached underline row is preserved.
    grid.get_mut(0, 0).underline = UnderlineStyle::Straight;
    grid.get_mut(0, 0).underline_color = Rgb::new(0, 200, 255);
    // Border a cell in the stable row 0 and one in row 2; row 2's border colour
    // changes in the edit below while row 0's stays, so the comparison checks the
    // border pass rebuilds the decoration-damaged row and keeps the cached one.
    set_border(&mut grid, 0, 0, Rgb::new(0, 200, 255));
    set_border(&mut grid, 2, 0, Rgb::new(0, 200, 255));

    let render =
        |renderer: &mut Renderer, grid: &Grid, damage: &Damage, decoration_damage: &Damage| {
            renderer.render_into(
                &device,
                &queue,
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
                    damage,
                    decoration_damage,
                },
            );
            read_back(&device, &queue, &target, width, height)
        };

    // Build the original grid, then edit a middle row so the rows after it shift.
    // The edit also changes the row's background colour, so the comparison covers
    // both the glyph and the background per-row patching at a non-zero offset.
    let original = render(&mut renderer, &grid, &Damage::Full, &Damage::Full);
    fill_row(
        &mut grid,
        1,
        "much longer middle row",
        white,
        Rgb::new(30, 40, 120),
    );
    // Recolour row 2's border: a decoration change with no VT-cell change, so the
    // incremental frame damages row 2 only through decoration_damage.
    set_border(&mut grid, 2, 0, Rgb::new(220, 50, 47));

    // Incremental: VT damage marks row 1 (glyph/bg) and decoration damage marks
    // row 2 (border), so rows 0 and 2 come from their caches except row 2's border.
    let incremental = render(
        &mut renderer,
        &grid,
        &Damage::Partial({
            let mut dirty = vec![false; rows];
            dirty[1] = true;
            dirty
        }),
        &Damage::Partial({
            let mut dirty = vec![false; rows];
            dirty[2] = true;
            dirty
        }),
    );

    // Full rebuild of the same edited grid.
    let full = render(&mut renderer, &grid, &Damage::Full, &Damage::Full);

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

fn set_border(grid: &mut Grid, row: usize, col: usize, color: Rgb) {
    let border = Border {
        style: BorderStyle::Light,
        color,
    };
    let cell = grid.get_mut(row, col);
    cell.borders.top = Some(border);
    cell.borders.right = Some(border);
    cell.borders.bottom = Some(border);
    cell.borders.left = Some(border);
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
