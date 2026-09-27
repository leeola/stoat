//! Headless GPU check that patching only damaged rows matches a full rebuild.
//!
//! The text pass caches each row's built glyph instances and, on a damaged
//! frame, rebuilds and re-uploads only the changed rows instead of every glyph
//! on screen. This renders a grid, edits a middle row (changing its glyph count
//! so later rows shift, exercising the from-first-changed-row upload), renders
//! once with partial damage (the incremental path) and once with full damage,
//! and asserts the two frames are pixel-identical. The edit also recolours one
//! border row, marked via the separate decoration-damage signal, so the same
//! comparison covers the border pass's per-row rebuild.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_render::{
    gpu::{build_font_system, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
    test_support::{offscreen_target, read_back, require_headless_device},
};
use stoatty_term::{
    grid::{whole_row, Border, BorderEdge, BorderStyle, Grid, Rgb, UnderlineStyle},
    term::Damage,
};
use wgpu::TextureFormat;

#[test]
fn patched_rows_match_a_full_rebuild() {
    let (device, queue) = require_headless_device();

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    let (width, height) = (256u32, cell_h * 4);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);

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
                    scrolled_rows: 0,
                    sketch_reveals: &[],
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
            let mut dirty = vec![None; rows];
            dirty[1] = whole_row(cols);
            dirty
        }),
        &Damage::Partial({
            let mut dirty = vec![None; rows];
            dirty[2] = whole_row(cols);
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
    for edge in [
        BorderEdge::Top,
        BorderEdge::Right,
        BorderEdge::Bottom,
        BorderEdge::Left,
    ] {
        grid.set_border_edge(row, col..col + 1, edge, border);
    }
}
