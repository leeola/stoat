//! Headless GPU check that a geometric powerline separator fills its cell.
//!
//! The procedural separators replace a vertical-only bitmap scale that left the
//! arrow short of the cell's right edge, the dark seam the fish statusline
//! showed. This renders a U+E0B0 separator off-screen and reads the pixels back,
//! asserting the filled triangle spans the whole cell: the full-height left edge
//! is the arrow colour, the apex reaches the right edge, and the empty top-right
//! corner stays the background.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_render::{
    gpu::{build_font_system, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
    test_support::{offscreen_target, read_back, require_headless_device},
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::TextureFormat;

#[test]
fn powerline_separator_fills_the_cell() {
    let (device, queue) = require_headless_device();

    let format = TextureFormat::Rgba8Unorm;
    // 4 * width must be a multiple of 256 for the texture-to-buffer copy.
    let (width, height) = (128u32, 64u32);
    let font_size = 30;

    let arrow = Rgb::new(220, 40, 40);
    let fill = Rgb::new(30, 40, 200);

    let (target, view) = offscreen_target(&device, width, height);

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
        fill,
        Rgb::new(255, 255, 255),
    );

    let (rows, cols) = renderer.grid_size();
    let mut grid = Grid::new(rows, cols);
    grid.get_mut(0, 0).ch = '\u{E0B0}';
    grid.get_mut(0, 0).fg = arrow;
    grid.get_mut(0, 0).bg = fill;

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
    let texel = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        (pixels[i], pixels[i + 1], pixels[i + 2])
    };
    let is_arrow = |(r, _, b): (u8, u8, u8)| r > 160 && b < 100;
    let is_fill = |(r, _, b): (u8, u8, u8)| b > 160 && r < 100;

    let [cell_w, cell_h] = cell_size(font_size, 1.0);
    let cell_w = cell_w.round() as u32;
    let cell_h = cell_h.round() as u32;
    let mid = cell_h / 2;

    // The full-height left edge is the arrow's base: the arrow colour top to
    // bottom of the cell.
    assert!(
        is_arrow(texel(1, cell_h / 4)),
        "left edge upper: {:?}",
        texel(1, cell_h / 4)
    );
    assert!(
        is_arrow(texel(1, mid)),
        "left edge middle: {:?}",
        texel(1, mid)
    );
    assert!(
        is_arrow(texel(1, cell_h * 3 / 4)),
        "left edge lower: {:?}",
        texel(1, cell_h * 3 / 4)
    );

    // The apex reaches the cell's right edge at mid-height. The old vertical-only
    // scaling left this as background, the seam this change removes.
    assert!(
        is_arrow(texel(cell_w - 2, mid)),
        "apex reaches right edge: {:?}",
        texel(cell_w - 2, mid)
    );

    // The top-right corner sits outside the triangle, so it stays the fill.
    assert!(
        is_fill(texel(cell_w - 2, 2)),
        "top-right corner: {:?}",
        texel(cell_w - 2, 2)
    );
}
