//! Headless GPU check that a hollow cursor fill paints the block's outline only.
//!
//! A window without focus draws its cursor hollow, so the cell's text stays
//! readable while the block still marks where the cursor sits. This draws the
//! cursor over a black grid on cell (0, 1) and reads the pixels back: a hollow
//! block brightens the cell's top-left pixel and leaves its center as the bare
//! grid painted it, and a solid block brightens the center too.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_render::{
    gpu::Renderer,
    render::{background::CursorFill, cell_size},
    test_support::{
        self, offscreen_target, pixel, read_back, require_headless_device, still_frame,
    },
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};

#[test]
fn a_hollow_cursor_paints_its_outline_and_leaves_the_center() {
    let (device, queue) = require_headless_device();

    let font_size = 30;
    let [cell_w, cell_h] = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);
    let (width, height) = (128u32, cell_h * 3);

    let (target, view) = offscreen_target(&device, width, height);
    let mut renderer =
        test_support::renderer(&device, [width, height], font_size, 1.0, Rgb::new(0, 0, 0));

    let (rows, cols) = renderer.grid_size();
    let base = Grid::new(rows, cols);
    let no_decoration = Damage::Partial(Vec::new());
    let resolution = [width as f32, height as f32];
    // The cursor block covers cell (0, 1): corners are its cell extent.
    let corners = [[0.0, 1.0], [1.0, 1.0], [0.0, 2.0], [1.0, 2.0]];

    let render = |renderer: &mut Renderer, cursor: Option<[[f32; 2]; 4]>| {
        let plain = still_frame(&Damage::Full, &no_decoration);
        renderer.render_into(&device, &queue, &view, &base, plain);
        renderer.draw_cursor_over(&device, &queue, &view, resolution, cursor, 0.0, None);
        read_back(&device, &queue, &target, width, height)
    };
    let brightness = |pixels: &[u8], (x, y): (u32, u32)| {
        pixel(pixels, width, x, y)
            .iter()
            .map(|&c| u32::from(c))
            .sum::<u32>()
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
