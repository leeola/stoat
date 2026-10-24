//! Headless pixel check that an APC cell border survives the glyph it frames.
//!
//! Z-order across passes comes from the record chain, so a decoration pass that
//! records before the text pass loses its lines wherever glyph ink reaches a
//! cell edge. A full block is the adversarial case: it scales to the exact cell
//! box, so it covers every pixel a border would occupy and leaves nothing of a
//! border drawn beneath it.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_render::{
    render::cell_size,
    test_support::{
        self, offscreen_target, pixel, read_back, require_headless_device, still_frame,
    },
};
use stoatty_term::{
    grid::{Border, BorderEdge, BorderStyle, Grid, Panel, PanelShadow, Rgb},
    term::Damage,
};

#[test]
fn a_cell_border_draws_over_the_block_glyph_filling_its_cell() {
    let (device, queue) = require_headless_device();

    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 10.0).round() as u32);

    let surface = Rgb::new(0, 0, 0);
    let border_color = Rgb::new(255, 0, 0);

    let (target, view) = offscreen_target(&device, width, height);

    let mut renderer = test_support::renderer(&device, [width, height], font_size, 1.0, surface);

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
        still_frame(&Damage::Full, &Damage::Partial(Vec::new())),
    );

    let pixels = read_back(&device, &queue, &target, width, height);
    let px = |x: u32, y: u32| -> [u8; 3] { pixel(&pixels, width, x, y) };

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
    let (device, queue) = require_headless_device();

    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 8.0).round() as u32);

    let surface = Rgb::new(0, 0, 0);
    let (target, view) = offscreen_target(&device, width, height);

    let mut renderer = test_support::renderer(&device, [width, height], font_size, 1.0, surface);

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
        still_frame(&Damage::Full, &Damage::Partial(Vec::new())),
    );

    let pixels = read_back(&device, &queue, &target, width, height);
    let at = |x: u32, y: u32| -> [u8; 3] { pixel(&pixels, width, x, y) };

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

/// A rounded corner carries the weight of the runs it joins, so the arc holds
/// as much ink as the straight edge four pixels along from it.
///
/// An arc drawn as a line centered on the outer radius lays ink on both sides
/// of it, which reads as a corner heavier than the sides that meet it and as a
/// bleed past the cell's own corner.
#[test]
fn a_rounded_corner_carries_the_weight_of_the_runs_it_joins() {
    let (device, queue) = require_headless_device();

    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 6.0).round() as u32);

    let (target, view) = offscreen_target(&device, width, height);

    let mut renderer =
        test_support::renderer(&device, [width, height], font_size, 1.0, Rgb::new(0, 0, 0));

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 4 && cols >= 4, "grid too small: {rows}x{cols}");

    let (brow, bcol) = (2, 2);
    let mut grid = Grid::new(rows, cols);
    for edge in [BorderEdge::Top, BorderEdge::Left] {
        grid.set_border_edge(
            brow,
            bcol..bcol + 1,
            edge,
            Border {
                style: BorderStyle::Rounded,
                color: Rgb::new(255, 0, 0),
            },
        );
    }

    renderer.render_into(
        &device,
        &queue,
        &view,
        &grid,
        still_frame(&Damage::Full, &Damage::Partial(Vec::new())),
    );

    let pixels = read_back(&device, &queue, &target, width, height);
    let (x0, y0) = (bcol as f32 * cell_w, brow as f32 * cell_h);

    // The arc spans the quadrant a half-cell wide, so the column one pixel
    // short of it crosses the arc where it meets the straight top run.
    let radius = cell_w.min(cell_h) * 0.5;
    let join_x = (x0 + radius) as u32 - 1;
    let ink = |x: u32| -> u32 {
        (y0 as u32..(y0 + cell_h) as u32)
            .map(|y| u32::from(pixel(&pixels, width, x, y)[0]))
            .sum()
    };

    let join = ink(join_x);
    let run = ink(join_x + 4);
    assert!(
        join > 200 && join.abs_diff(run) <= 20,
        "the arc holds the straight run's ink: {join} at the join against {run} along the run"
    );
}

/// A border and a panel in the style that draws no line paint no pixel, so a
/// panel declared only to cover its rect leaves the frame as a blank grid
/// draws it. Each in a light line paints, which shows the fixture reaches the
/// screen.
#[test]
fn the_style_with_no_line_paints_nothing() {
    let (device, queue) = require_headless_device();

    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (width, height) = (256u32, (cell[1] * 6.0).round() as u32);

    let (target, view) = offscreen_target(&device, width, height);

    let mut renderer =
        test_support::renderer(&device, [width, height], font_size, 1.0, Rgb::new(0, 0, 0));

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 4 && cols >= 8, "grid too small: {rows}x{cols}");

    let bordered = |style| {
        let mut grid = Grid::new(rows, cols);
        grid.set_border_edge(
            2,
            1..3,
            BorderEdge::Bottom,
            Border {
                style,
                color: Rgb::new(255, 0, 0),
            },
        );
        grid
    };
    let paneled = |style| {
        let mut grid = Grid::new(rows, cols);
        grid.set_panels(vec![Panel {
            top: 1,
            left: 4,
            width: 4,
            height: 3,
            style,
            border: Rgb::new(255, 0, 0),
            corner_radius: 6,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
            anchor: None,
            seq: 0,
        }]);
        grid
    };
    // One renderer draws every grid in turn, and a frame keeps the decorations
    // it does not name, so each frame names them all.
    let mut render = |grid: &Grid| {
        renderer.render_into(
            &device,
            &queue,
            &view,
            grid,
            still_frame(&Damage::Full, &Damage::Full),
        );
        read_back(&device, &queue, &target, width, height)
    };

    let blank = render(&Grid::new(rows, cols));
    let changed = |pixels: Vec<u8>| blank.iter().zip(&pixels).filter(|(a, b)| a != b).count();
    let painted = [
        bordered(BorderStyle::Light),
        paneled(BorderStyle::Light),
        bordered(BorderStyle::None_),
        paneled(BorderStyle::None_),
    ]
    .map(|grid| changed(render(&grid)) > 0);
    assert_eq!(
        painted,
        [true, true, false, false],
        "a light border and panel paint, and with no line each leaves the blank frame"
    );
}
