//! Headless pixel check that a panel's drop shadow stays outside its box.
//!
//! An unfilled panel (`fill: None`, what stoat's modals emit) must not wash its
//! own interior with the drop shadow. This renders one such panel over a
//! non-black clear off-screen and reads the pixels back, asserting the panel's
//! interior center keeps the clear color while a pixel just past the box's
//! bottom-right edge is darkened by the shadow. Skips when no GPU adapter is
//! present so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{BorderStyle, Grid, Panel, Rgb},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

#[test]
fn unfilled_panel_shadow_stays_outside_the_box() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("panel_shadow_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 8.0).round() as u32);

    // A non-black cell background so the black shadow's darkening is measurable.
    let surface = Rgb::new(120, 120, 120);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("panel shadow target"),
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
    assert!(rows >= 5 && cols >= 6, "grid too small: {rows}x{cols}");

    // A panel inset one cell from the top-left, leaving a couple cells of margin
    // at the bottom-right for the [5,7]px shadow to fall into.
    let panel = Panel {
        top: 1,
        left: 1,
        width: cols as u16 - 3,
        height: rows as u16 - 3,
        style: BorderStyle::Rounded,
        border: Rgb::new(200, 100, 50),
        corner_radius: 6,
        fill: None,
        shadow: true,
        title_gap: None,
    };
    let mut grid = Grid::new(rows, cols);
    // Paint every cell the surface color so the panel's interior and its
    // exterior-shadow region sit on the same known background.
    for r in 0..rows {
        for c in 0..cols {
            grid.get_mut(r, c).bg = surface;
        }
    }
    grid.set_panels(vec![panel]);

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
        },
    );

    let pixels = read_back(&device, &queue, &target, width, height);
    let px = |x: u32, y: u32| -> [u8; 3] {
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };

    // The box's right and bottom edges in pixels, one cell past the left/top
    // inset plus the panel's cell width and height.
    let box_right = (1.0 + (cols as f32 - 3.0)) * cell_w;
    let box_bottom = (1.0 + (rows as f32 - 3.0)) * cell_h;

    let center = px((box_right * 0.5) as u32, (box_bottom * 0.5) as u32);
    // A few px past the box's bottom-right corner, inside the offset shadow rect.
    let exterior = px(box_right as u32 + 3, box_bottom as u32 + 3);

    assert!(
        center
            .iter()
            .zip([120, 120, 120])
            .all(|(&got, want)| got.abs_diff(want) <= 1),
        "panel interior center should keep the clear color, got {center:?}"
    );
    assert!(
        exterior[0] < center[0].saturating_sub(20),
        "a pixel past the box's bottom-right edge should be shadow-darkened, \
         got exterior {exterior:?} vs center {center:?}"
    );
}

#[test]
fn title_gap_notches_the_top_stroke() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("panel_shadow_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 8.0).round() as u32);

    let surface = Rgb::new(120, 120, 120);
    let border = Rgb::new(0, 200, 0);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("panel title-gap target"),
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
    assert!(rows >= 5 && cols >= 12, "grid too small: {rows}x{cols}");

    // A title gap three cells in from the panel's left edge, four cells wide, so
    // the notch clears the corner radius. Both probe columns sit on the top
    // edge, one inside the gap and one to its left over solid stroke.
    let panel = Panel {
        top: 2,
        left: 2,
        width: cols as u16 - 4,
        height: rows as u16 - 4,
        style: BorderStyle::Light,
        border,
        corner_radius: 6,
        fill: None,
        shadow: false,
        title_gap: Some((48, 64)),
    };
    let mut grid = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            grid.get_mut(r, c).bg = surface;
        }
    }
    grid.set_panels(vec![panel]);

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
        },
    );

    let pixels = read_back(&device, &queue, &target, width, height);
    let px = |x: u32, y: u32| -> [u8; 3] {
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };

    let top_y = (2.0 * cell_h).round() as u32;
    // Middle of the gap span (panel left + 5 cells) versus one cell in from the
    // left edge, left of the gap and past the corner.
    let in_gap = px(((2.0 + 5.0) * cell_w) as u32, top_y);
    let on_stroke = px(((2.0 + 1.0) * cell_w) as u32, top_y);

    assert!(
        in_gap
            .iter()
            .zip([120, 120, 120])
            .all(|(&got, want)| got.abs_diff(want) <= 4),
        "a top-edge pixel inside the title gap should show no stroke, got {in_gap:?}"
    );
    // The green border suppresses red and blue where it draws, so a stroked
    // pixel reads green-dominant while the neutral gap pixel does not.
    assert!(
        on_stroke[1] > on_stroke[0] + 40 && on_stroke[1] > on_stroke[2] + 40,
        "a top-edge pixel outside the title gap should show the green border stroke, \
         got {on_stroke:?} vs gap {in_gap:?}"
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
        label: Some("panel shadow readback"),
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
