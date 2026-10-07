//! Headless GPU check that a hollow cursor fill paints the block's outline only.
//!
//! A window without focus draws its cursor hollow, so the cell's text stays
//! readable while the block still marks where the cursor sits. This draws the
//! cursor over a black grid on cell (0, 1) and reads the pixels back: a hollow
//! block brightens the cell's top-left pixel and leaves its center as the bare
//! grid painted it, and a solid block brightens the center too.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_render::{
    gpu::{build_font_system, FontConfig, Frame, Renderer, Scroll},
    render::{background::CursorFill, cell_size},
    test_support::{offscreen_target, read_back, require_headless_device},
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::TextureFormat;

#[test]
fn a_hollow_cursor_paints_its_outline_and_leaves_the_center() {
    let (device, queue) = require_headless_device();

    let font_size = 30;
    let [cell_w, cell_h] = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);
    let (width, height) = (128u32, cell_h * 3);

    let (target, view) = offscreen_target(&device, width, height);
    let mut renderer = Renderer::new(
        &device,
        TextureFormat::Rgba8Unorm,
        [width, height],
        build_font_system(),
        FontConfig {
            size: font_size,
            scale_factor: 1.0,
            family: &["JetBrains Mono".to_owned()],
            ligatures: true,
        },
        Rgb::new(0, 0, 0),
        Rgb::new(255, 255, 255),
    );

    let (rows, cols) = renderer.grid_size();
    let base = Grid::new(rows, cols);
    let no_decoration = Damage::Partial(Vec::new());
    let resolution = [width as f32, height as f32];
    // The cursor block covers cell (0, 1): corners are its cell extent.
    let corners = [[0.0, 1.0], [1.0, 1.0], [0.0, 2.0], [1.0, 2.0]];

    let render = |renderer: &mut Renderer, cursor: Option<[[f32; 2]; 4]>| {
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
            scrolled_rows: 0,
            sketch_reveals: &[],
        };
        renderer.render_into(&device, &queue, &view, &base, plain);
        renderer.draw_cursor_over(&device, &queue, &view, resolution, cursor, 0.0, None);
        read_back(&device, &queue, &target, width, height)
    };
    let brightness = |pixels: &[u8], (x, y): (u32, u32)| {
        let i = ((y * width + x) * 4) as usize;
        pixels[i..i + 3].iter().map(|&c| u32::from(c)).sum::<u32>()
    };
    let center = (cell_w / 2, cell_h + cell_h / 2);
    let edge = (0, cell_h);

    let bare = render(&mut renderer, None);
    renderer.set_cursor_fill(CursorFill::Hollow);
    let hollow = render(&mut renderer, Some(corners));
    renderer.set_cursor_fill(CursorFill::Solid);
    let solid = render(&mut renderer, Some(corners));

    assert_eq!(
        brightness(&hollow, center),
        brightness(&bare, center),
        "a hollow block leaves the cell's center as the grid painted it"
    );
    assert!(
        brightness(&hollow, edge) > brightness(&bare, edge),
        "a hollow block paints its outline over the cell's edge: {} vs {}",
        brightness(&hollow, edge),
        brightness(&bare, edge)
    );
    assert!(
        brightness(&solid, center) > brightness(&bare, center),
        "a solid block tints the cell's center: {} vs {}",
        brightness(&solid, center),
        brightness(&bare, center)
    );
}
