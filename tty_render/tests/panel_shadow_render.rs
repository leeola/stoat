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
    grid::{BorderStyle, Grid, Overlay, Panel, PanelShadow, Rgb},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

/// The surface (clear) color the panel is rendered over, so darkening or fill is
/// measurable against a known non-black background.
const SURFACE: [u8; 3] = [120, 120, 120];

/// Rendered pixels of one panel over the [`SURFACE`] clear, plus the geometry
/// needed to index into them.
struct Rendered {
    pixels: Vec<u8>,
    width: u32,
    cell: [f32; 2],
    rows: usize,
    cols: usize,
}

impl Rendered {
    fn px(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2]]
    }
}

/// Render the panel `build` produces (given the grid size) over the clear and
/// read the pixels back.
fn render_panel(build: impl FnOnce(usize, usize) -> Panel) -> Option<Rendered> {
    render_scene(1.0, |grid, rows, cols| {
        grid.set_panels(vec![build(rows, cols)]);
    })
}

/// Render the chrome `build` puts on the grid over the clear at `scale_factor`,
/// and read the pixels back. `None` when no GPU adapter is present so a GPU-less
/// CI stays green.
fn render_scene(
    scale_factor: f32,
    build: impl FnOnce(&mut Grid, usize, usize),
) -> Option<Rendered> {
    let (device, queue) = headless_device()?;

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell = cell_size(font_size, scale_factor);
    let (width, height) = (256u32, (cell[1] * 8.0).round() as u32);
    let surface = Rgb::new(SURFACE[0], SURFACE[1], SURFACE[2]);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("panel target"),
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
            scale_factor,
            family: &["JetBrains Mono".to_owned()],
            ligatures: true,
        },
        surface,
        Rgb::new(255, 255, 255),
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 5 && cols >= 6, "grid too small: {rows}x{cols}");

    let mut grid = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            grid.get_mut(r, c).bg = surface;
        }
    }
    build(&mut grid, rows, cols);

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
            sketch_reveals: &[],
        },
    );

    let pixels = read_back(&device, &queue, &target, width, height);
    Some(Rendered {
        pixels,
        width,
        cell,
        rows,
        cols,
    })
}

#[test]
fn unfilled_panel_shadow_stays_outside_the_box() {
    // A panel inset one cell from the top-left, leaving a couple cells of margin
    // at the bottom-right for the [5,7]px shadow to fall into.
    let Some(r) = render_panel(|rows, cols| Panel {
        top: 1,
        left: 1,
        width: cols as u16 - 3,
        height: rows as u16 - 3,
        style: BorderStyle::Rounded,
        border: Rgb::new(200, 100, 50),
        corner_radius: 6,
        fill: None,
        shadow: PanelShadow::Drop,
        inset_x: 0,
        above_pools: false,
        anchor: None,
        seq: 0,
    }) else {
        eprintln!("panel_shadow_render: no wgpu adapter available, skipping");
        return;
    };

    let box_left = 1.0 * r.cell[0];
    let box_right = (1.0 + (r.cols as f32 - 3.0)) * r.cell[0];
    let box_bottom = (1.0 + (r.rows as f32 - 3.0)) * r.cell[1];

    let center = r.px((box_right * 0.5) as u32, (box_bottom * 0.5) as u32);
    // A few px past the box's bottom edge, inside the offset shadow rect and
    // clear of the corner, where a blurred rect is at its darkest.
    let exterior = r.px(box_right as u32 - 3, box_bottom as u32 + 3);

    assert!(
        center
            .iter()
            .zip(SURFACE)
            .all(|(&got, want)| got.abs_diff(want) <= 1),
        "panel interior center should keep the clear color, got {center:?}"
    );
    assert!(
        exterior[0] < center[0].saturating_sub(20),
        "a pixel past the box's bottom edge should be shadow-darkened, \
         got exterior {exterior:?} vs center {center:?}"
    );

    // A blurred rect falls off with distance from its edge. A sharp rect faded
    // by its exterior distance alone is flat everywhere inside itself, so these
    // two read the same there.
    let mid_y = (box_bottom * 0.5) as u32;
    let near = r.px(box_right as u32 + 1, mid_y);
    let far = r.px(box_right as u32 + 4, mid_y);
    assert!(
        far[0] > near[0] + 2,
        "the shadow lightens away from the box's right edge, got near {near:?} then far {far:?}"
    );

    // The blur follows the rect's corner, so the corner casts less than the run
    // that leaves it. A rect faded by its exterior distance casts a square
    // plateau that reads the same at both.
    let corner = r.px(box_left as u32 + 3, box_bottom as u32 + 3);
    let along = r.px(box_left as u32 + 12, box_bottom as u32 + 3);
    assert!(
        corner[0] > along[0] + 4,
        "the shadow's corner is lighter than its bottom run, got corner {corner:?} then {along:?}"
    );
}

#[test]
fn a_horizontal_inset_leaves_the_cell_edge_strip_clear() {
    let inset = 6u8;
    let Some(r) = render_panel(|rows, cols| Panel {
        top: 1,
        left: 1,
        width: cols as u16 - 2,
        height: rows as u16 - 3,
        style: BorderStyle::Rounded,
        border: Rgb::new(200, 100, 50),
        corner_radius: 6,
        // Fill the box so the inset frame's interior is visibly not the clear.
        fill: Some(Rgb::new(20, 22, 30)),
        shadow: PanelShadow::None_,
        inset_x: inset,
        above_pools: false,
        anchor: None,
        seq: 0,
    }) else {
        eprintln!("panel_shadow_render: no wgpu adapter available, skipping");
        return;
    };

    let box_left = 1.0 * r.cell[0];
    let box_right = (1.0 + (r.cols as f32 - 2.0)) * r.cell[0];
    let mid_y = ((1.0 + (r.rows as f32 - 3.0) * 0.5) * r.cell[1]) as u32;

    // A couple pixels inside each cell-rect edge, within the inset strip, keeps
    // the clear color because the box is shaved off there.
    let left_strip = r.px(box_left as u32 + 2, mid_y);
    let right_strip = r.px(box_right as u32 - 3, mid_y);
    for (label, got) in [("left", left_strip), ("right", right_strip)] {
        assert!(
            got.iter().zip(SURFACE).all(|(&g, want)| g.abs_diff(want) <= 1),
            "the {label} inset strip at the cell rect edge should keep the clear color, got {got:?}"
        );
    }

    // Past the inset, inside the frame, the fill drew, so the pixel is not clear.
    let interior = r.px(box_left as u32 + inset as u32 + 6, mid_y);
    assert!(
        interior
            .iter()
            .zip(SURFACE)
            .any(|(&g, want)| g.abs_diff(want) > 1),
        "just inside the inset frame the fill should show, got {interior:?}"
    );
}

/// Popover chrome is stated in logical pixels, so a 2x display draws it at
/// twice the size rather than at half the weight of the panel chrome beside it.
///
/// The border is one logical pixel, so it is two opaque rows deep here. The
/// shadow's blur scales with it, so it still reaches a pixel 24 physical pixels
/// past the box.
#[test]
fn popover_chrome_scales_with_the_display() {
    const FILL: [u8; 3] = [20, 22, 30];
    const BORDER: [u8; 3] = [200, 100, 50];

    let Some(r) = render_scene(2.0, |grid, _rows, _cols| {
        grid.set_overlays(vec![Overlay {
            top: 1,
            left: 1,
            width: 4,
            height: 3,
            fill: Rgb::new(FILL[0], FILL[1], FILL[2]),
            border: Rgb::new(BORDER[0], BORDER[1], BORDER[2]),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: String::new(),
        }]);
    }) else {
        eprintln!("panel_shadow_render: no wgpu adapter available, skipping");
        return;
    };

    let box_top = r.cell[1];
    let box_bottom = 4.0 * r.cell[1];
    // The column through the box's horizontal middle, clear of both rounded
    // corners.
    let mid_x = (3.0 * r.cell[0]) as u32;

    let band = [0, 1, 2].map(|row| r.px(mid_x, box_top as u32 + row));
    assert_eq!(
        band,
        [BORDER, BORDER, FILL],
        "the border band is two rows deep at twice the density"
    );

    let below = r.px(mid_x, box_bottom as u32 + 24);
    assert!(
        below[0] + 3 < SURFACE[0],
        "the scaled blur still darkens 24 px past the box, got {below:?}"
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
