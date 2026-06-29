//! Headless GPU check that grid scroll rides the globals uniform.
//!
//! Scroll used to be baked into each glyph instance, so a scroll-only frame
//! rebuilt and re-uploaded every glyph. It now travels in the text globals and
//! is applied in the vertex shader, so a frame that only scrolls reuses the
//! cached instances. This renders a glyph at row 0, then renders again at grid
//! scroll 1 with no damage -- so the instances are not rebuilt -- and reads the
//! pixels back to assert the glyph moved down exactly one row. Skips when no GPU
//! adapter is present, so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Grid, Rgb, UnderlineStyle},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

#[test]
fn grid_scroll_moves_glyph_down_without_rebuild() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("scroll_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    // 4 * width must be a multiple of 256 for the texture-to-buffer copy; three
    // rows give a clear row-0 and row-1 band.
    let (width, height) = (128u32, cell_h * 3);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("scroll target"),
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
    assert!(rows >= 2 && cols >= 1, "grid too small: {rows}x{cols}");
    let mut grid = Grid::new(rows, cols);
    grid.get_mut(0, 0).ch = 'M';
    grid.get_mut(0, 0).fg = white;
    grid.get_mut(0, 0).bg = black;
    // Underline the cell too: it must scroll with the glyph, so the row-0-cleared
    // assertion below also catches a stale (non-scrolling) underline.
    grid.get_mut(0, 0).underline = UnderlineStyle::Straight;
    grid.get_mut(0, 0).underline_color = white;

    let no_decoration = Damage::Partial(Vec::new());
    let frame = |grid_scroll, damage| Frame {
        cursor: None,
        cursor_corners: None,
        scroll: Scroll {
            grid: grid_scroll,
            document: 0.0,
            scrollback: 0.0,
            region: 0.0,
            popovers: &[],
        },
        damage,
        decoration_damage: &no_decoration,
    };

    // First frame builds the instances at row 0.
    renderer.render_into(&device, &queue, &view, &grid, frame(0.0, &Damage::Full));
    let unscrolled = read_back(&device, &queue, &target, width, height);

    // Second frame only scrolls one row, with no damage, so the instances are
    // reused and the scroll comes entirely from the uniform.
    let idle = Damage::Partial(vec![false; rows]);
    renderer.render_into(&device, &queue, &view, &grid, frame(1.0, &idle));
    let scrolled = read_back(&device, &queue, &target, width, height);

    let lit = |pixels: &[u8], band: u32| {
        let mut count = 0usize;
        for y in (band * cell_h)..((band + 1) * cell_h) {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let (r, g, b) = (pixels[i] as u32, pixels[i + 1] as u32, pixels[i + 2] as u32);
                if r + g + b > 120 {
                    count += 1;
                }
            }
        }
        count
    };

    let base_row0 = lit(&unscrolled, 0);
    assert!(base_row0 > 0, "glyph should render in row 0 unscrolled");
    assert_eq!(lit(&unscrolled, 1), 0, "row 1 should be empty unscrolled");

    assert!(
        lit(&scrolled, 1) > 0,
        "scroll should move the glyph into row 1"
    );
    assert!(
        lit(&scrolled, 0) * 4 < base_row0,
        "row 0 should be nearly cleared after scrolling: {} vs base {base_row0}",
        lit(&scrolled, 0)
    );
}

#[test]
fn document_scroll_shifts_the_grid_like_grid_scroll() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("scroll_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    let (width, height) = (128u32, cell_h * 3);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("document scroll target"),
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
    assert!(rows >= 2 && cols >= 1, "grid too small: {rows}x{cols}");
    let mut grid = Grid::new(rows, cols);
    grid.get_mut(0, 0).ch = 'M';
    grid.get_mut(0, 0).fg = white;
    grid.get_mut(0, 0).bg = black;

    let no_decoration = Damage::Partial(Vec::new());
    let frame = |document, damage| Frame {
        cursor: None,
        cursor_corners: None,
        scroll: Scroll {
            grid: 0.0,
            document,
            scrollback: 0.0,
            region: 0.0,
            popovers: &[],
        },
        damage,
        decoration_damage: &no_decoration,
    };

    // The document offset rides the same globals as the grid offset, so a
    // document-only shift moves the cached glyph with no rebuild.
    renderer.render_into(&device, &queue, &view, &grid, frame(0.0, &Damage::Full));
    let unscrolled = read_back(&device, &queue, &target, width, height);

    let idle = Damage::Partial(vec![false; rows]);
    renderer.render_into(&device, &queue, &view, &grid, frame(1.0, &idle));
    let scrolled = read_back(&device, &queue, &target, width, height);

    let lit = |pixels: &[u8], band: u32| {
        let mut count = 0usize;
        for y in (band * cell_h)..((band + 1) * cell_h) {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let (r, g, b) = (pixels[i] as u32, pixels[i + 1] as u32, pixels[i + 2] as u32);
                if r + g + b > 120 {
                    count += 1;
                }
            }
        }
        count
    };

    let base_row0 = lit(&unscrolled, 0);
    assert!(base_row0 > 0, "glyph should render in row 0 unscrolled");
    assert_eq!(lit(&unscrolled, 1), 0, "row 1 should be empty unscrolled");

    assert!(
        lit(&scrolled, 1) > 0,
        "a document scroll of one row should move the glyph into row 1"
    );
    assert!(
        lit(&scrolled, 0) * 4 < base_row0,
        "row 0 should be nearly cleared after a document scroll: {} vs base {base_row0}",
        lit(&scrolled, 0)
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
        label: Some("scroll readback"),
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
