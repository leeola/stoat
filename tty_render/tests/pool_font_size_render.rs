//! Headless GPU check that a pool composited at its own font size lays its
//! cells out at that size, from the screen cell its region starts on, under the
//! boxes of the live grid.
//!
//! A terminal pane draws at a smaller font than the editor's, so its pool's
//! cells are smaller than the live grid's. Each test draws a pool at half the
//! live size over a region inset from the corner of the surface, and reads back
//! the center pixel of every pool cell it names.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_render::{
    gpu::{build_font_system, FontConfig, Frame, PoolComposite, Renderer, Scroll},
    render::cell_size,
    test_support::{offscreen_target, read_back, require_headless_device},
};
use stoatty_term::{
    grid::{Bar, BorderStyle, Grid, Panel, PanelShadow, Polyline, Rgb, TextRun},
    term::Damage,
};
use wgpu::{Device, TextureFormat};

/// The live grid's font size, whose cells are 12 by 24 pixels at scale 1.
const LIVE_FONT: u32 = 20;
/// The pool's font size, whose cells are half the live grid's on each axis.
const POOL_FONT: u32 = 10;
/// The surface. A read-back row spans a multiple of 256 bytes, which at four
/// bytes a pixel is a multiple of 64 pixels.
const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;

#[test]
fn a_pool_at_its_own_font_size_lays_its_cells_out_at_that_size() {
    let (device, queue) = require_headless_device();
    let (target, view) = offscreen_target(&device, WIDTH, HEIGHT);
    let mut renderer = renderer(&device);
    let ([live_w, live_h], [pool_w, pool_h]) = (cells(LIVE_FONT), cells(POOL_FONT));

    // Four by two live cells from live cell (2, 1) hold eight by four pool cells.
    let (left, top) = (2, 1);
    let (cols, rows) = (4 * live_w / pool_w, 2 * live_h / pool_h);
    let color = |row: u32, col: u32| Rgb::new(30 + 25 * col as u8, 40 + 50 * row as u8, 200);
    let mut pool = Grid::new(rows as usize, cols as usize);
    for row in 0..rows {
        for col in 0..cols {
            pool.get_mut(row as usize, col as usize).bg = color(row, col);
        }
    }

    let live = Grid::new((HEIGHT / live_h) as usize, (WIDTH / live_w) as usize);
    let (full, none) = (Damage::Full, Damage::Partial(Vec::new()));
    renderer.render_pools_into(
        &device,
        &queue,
        &view,
        &live,
        still(&full, &none),
        &[PoolComposite {
            id: 0,
            grid: &pool,
            font_size: Some(POOL_FONT),
            origin_cells: [left as f32, top as f32],
            scissor: [left * live_w, top * live_h, 4 * live_w, 2 * live_h],
            shift_rows: 0.0,
            content_changed: true,
            scrolled_rows: None,
            occludable: true,
        }],
        &[],
    );
    let pixels = read_back(&device, &queue, &target, WIDTH, HEIGHT);

    let at = |row: u32, col: u32| {
        let x = left * live_w + col * pool_w + pool_w / 2;
        let y = top * live_h + row * pool_h + pool_h / 2;
        pixel(&pixels, x, y)
    };
    let every = (0..rows).flat_map(|row| (0..cols).map(move |col| (row, col)));
    assert_eq!(
        every
            .clone()
            .map(|(row, col)| at(row, col))
            .collect::<Vec<_>>(),
        every
            .map(|(row, col)| rgb(color(row, col)))
            .collect::<Vec<_>>(),
        "each pool cell paints a square of the pool's size, counted from the region's corner",
    );
}

#[test]
fn the_live_grid_boxes_occlude_a_pool_at_its_own_font_size() {
    let (device, queue) = require_headless_device();
    let (target, view) = offscreen_target(&device, WIDTH, HEIGHT);
    let mut renderer = renderer(&device);
    let ([live_w, live_h], [pool_w, pool_h]) = (cells(LIVE_FONT), cells(POOL_FONT));

    let live_bg = Rgb::new(10, 20, 30);
    let pool_bg = Rgb::new(240, 180, 20);
    let bar_color = Rgb::new(200, 50, 50);
    let run_bg = Rgb::new(50, 200, 50);
    let line_color = Rgb::new(220, 200, 40);

    // An unfilled box over live columns 4 and 5 leaves the live background
    // showing inside it, which is what an occluded pool pixel falls back to.
    let (live_rows, live_cols) = (HEIGHT / live_h, WIDTH / live_w);
    let mut live = Grid::new(live_rows as usize, live_cols as usize);
    for row in 0..live_rows as usize {
        for col in 0..live_cols as usize {
            live.get_mut(row, col).bg = live_bg;
        }
    }
    let panels = vec![Panel {
        top: 0,
        left: 4,
        width: 2,
        height: live_rows as u16,
        style: BorderStyle::Light,
        border: Rgb::new(128, 128, 128),
        corner_radius: 0,
        fill: None,
        shadow: PanelShadow::None_,
        inset_x: 0,
        above_pools: false,
        anchor: None,
        seq: 100,
    }];
    live.set_panels(panels.clone());

    // Six by two live cells from live cell (1, 1) hold twelve by four pool
    // cells. Each of the four composite passes draws one pool row, so each
    // scales the box by the live cell on its own.
    let (left, top) = (1, 1);
    let (cols, rows) = (6 * live_w / pool_w, 2 * live_h / pool_h);
    let mut pool = Grid::new(rows as usize, cols as usize);
    for row in 0..rows as usize {
        for col in 0..cols as usize {
            pool.get_mut(row, col).bg = pool_bg;
        }
    }
    pool.set_polylines(vec![Polyline {
        points: vec![[0, 8], [cols as i16 * 16, 8]],
        width: 16,
        color: line_color,
        seq: 1,
    }]);
    pool.set_bars(vec![Bar {
        x: 0,
        y: 2 * 16,
        width: cols as u16 * 16,
        height: 16,
        color: bar_color,
        seq: 1,
    }]);
    pool.set_text_runs(vec![TextRun {
        col: 0,
        row: 3 * 16,
        scale: 256,
        color: Rgb::new(0, 0, 0),
        bg: Some(run_bg),
        follow: 0,
        anchor: None,
        text: " ".repeat(cols as usize).into(),
        seq: 1,
    }]);

    let (full, none) = (Damage::Full, Damage::Partial(Vec::new()));
    renderer.render_into(&device, &queue, &view, &live, still(&full, &none));
    renderer.composite_pool(
        &device,
        &queue,
        &view,
        &pool,
        Some(POOL_FONT),
        &panels,
        [left * live_w, top * live_h, 6 * live_w, 2 * live_h],
        0.0,
        [left as f32, top as f32],
        true,
        None,
        true,
        0,
        0,
    );
    let pixels = read_back(&device, &queue, &target, WIDTH, HEIGHT);

    let center_x = |col: u32| left * live_w + col * pool_w + pool_w / 2;
    let boxed = |col: u32| (4 * live_w..6 * live_w).contains(&center_x(col));
    for (row, color, what) in [
        (0, line_color, "stroked path"),
        (1, pool_bg, "cell background"),
        (2, bar_color, "bar"),
        (3, run_bg, "run backing"),
    ] {
        let y = top * live_h + row * pool_h + pool_h / 2;
        let expected = (0..cols).map(|col| match boxed(col) {
            true => rgb(live_bg),
            false => rgb(color),
        });
        assert_eq!(
            (0..cols)
                .map(|col| pixel(&pixels, center_x(col), y))
                .collect::<Vec<_>>(),
            expected.collect::<Vec<_>>(),
            "the {what} hides under the box where it sits on the live grid",
        );
    }
}

/// A renderer over the whole surface at the live font size, clearing to black.
fn renderer(device: &Device) -> Renderer {
    Renderer::new(
        device,
        TextureFormat::Rgba8Unorm,
        [WIDTH, HEIGHT],
        build_font_system(),
        FontConfig {
            size: LIVE_FONT,
            scale_factor: 1.0,
            family: &["JetBrains Mono".to_owned()],
            ligatures: true,
        },
        Rgb::new(0, 0, 0),
        Rgb::new(0, 0, 0),
    )
}

/// The whole-pixel cell of `font_size` at scale 1.
fn cells(font_size: u32) -> [u32; 2] {
    cell_size(font_size, 1.0).map(|side| side.round() as u32)
}

/// A frame with no cursor and no scroll.
fn still<'a>(damage: &'a Damage, decoration_damage: &'a Damage) -> Frame<'a> {
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
    }
}

fn pixel(pixels: &[u8], x: u32, y: u32) -> (u8, u8, u8) {
    let i = ((y * WIDTH + x) * 4) as usize;
    (pixels[i], pixels[i + 1], pixels[i + 2])
}

fn rgb(color: Rgb) -> (u8, u8, u8) {
    (color.r, color.g, color.b)
}
