//! Headless GPU check that a focused cursor draws as an opaque block with the
//! glyph it covers in its cell's background color.
//!
//! Each test fills the grid with blue cells, puts a red full block glyph in one
//! column of every row, and draws the white cursor over them. It then reads
//! back pixels inside and around the block. A blank cell under the block shows
//! the cursor color, and the glyph shows in its cell's blue only where the
//! block covers it.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_render::{
    gpu::{PoolComposite, Renderer},
    render::{background::CursorFill, cell_size},
    test_support::{
        self, offscreen_target, pixel, read_back, require_headless_device, still_frame,
    },
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::{Device, Queue, Texture, TextureView};

/// The font size, whose cells are 12 by 24 pixels at scale 1.
const FONT: u32 = 20;
/// A read-back row spans a multiple of 256 bytes, which at four bytes a pixel
/// is a multiple of 64 pixels.
const WIDTH: u32 = 128;
/// Three whole rows and half of a fourth, so the last row hangs from the bottom
/// edge, 12 pixels above where the uniform pitch puts it.
const HEIGHT: u32 = 84;
const CELL: Rgb = Rgb::new(20, 30, 90);
const INK: Rgb = Rgb::new(200, 40, 40);
/// The cursor color [`test_support::renderer`] draws with.
const CURSOR: [u8; 3] = [255, 255, 255];

/// A renderer over an offscreen target, and the grid size it lays out.
struct Scene {
    device: Device,
    queue: Queue,
    target: Texture,
    view: TextureView,
    renderer: Renderer,
    rows: usize,
    cols: usize,
}

impl Scene {
    fn new() -> Scene {
        let (device, queue) = require_headless_device();
        let (target, view) = offscreen_target(&device, WIDTH, HEIGHT);
        let renderer =
            test_support::renderer(&device, [WIDTH, HEIGHT], FONT, 1.0, Rgb::new(0, 0, 0));
        let (rows, cols) = renderer.grid_size();
        Scene {
            device,
            queue,
            target,
            view,
            renderer,
            rows,
            cols,
        }
    }

    /// Draws `grid` in a plain frame, with its cursor cell at `cursor` and the
    /// block at `corners`, and reads the frame back.
    fn inline(
        &mut self,
        grid: &Grid,
        cursor: [f32; 2],
        corners: Option<[[f32; 2]; 4]>,
        grid_scroll: f32,
    ) -> Vec<u8> {
        let (full, none) = (Damage::Full, Damage::Partial(Vec::new()));
        let mut frame = still_frame(&full, &none);
        frame.cursor = Some(cursor);
        frame.cursor_corners = corners;
        frame.scroll.grid = grid_scroll;
        self.renderer
            .render_into(&self.device, &self.queue, &self.view, grid, frame);
        read_back(&self.device, &self.queue, &self.target, WIDTH, HEIGHT)
    }

    /// Draws `pools` over a blank live grid, with its cursor cell at `cursor`,
    /// then the cursor block at `corners` over them, and reads the frame back.
    fn pooled(
        &mut self,
        cursor: [f32; 2],
        corners: Option<[[f32; 2]; 4]>,
        pools: &[PoolComposite<'_>],
    ) -> Vec<u8> {
        let (full, none) = (Damage::Full, Damage::Partial(Vec::new()));
        let mut frame = still_frame(&full, &none);
        frame.cursor = Some(cursor);
        let live = Grid::new(self.rows, self.cols);
        self.renderer.render_pools_into(
            &self.device,
            &self.queue,
            &self.view,
            &live,
            frame,
            pools,
            &[],
        );
        self.renderer.draw_cursor_over(
            &self.device,
            &self.queue,
            &self.view,
            [WIDTH as f32, HEIGHT as f32],
            corners,
            0.0,
            None,
        );
        read_back(&self.device, &self.queue, &self.target, WIDTH, HEIGHT)
    }

    fn cells(&self) -> Grid {
        cells(self.rows, self.cols)
    }

    fn glyphs(&self) -> Grid {
        glyphs(self.rows, self.cols)
    }
}

/// A settled block paints a blank cell in the cursor color, opaque, and the
/// glyph of a cell holding one in that cell's background color.
#[test]
fn a_solid_cursor_is_opaque_and_draws_its_glyph_in_the_cell_background() {
    let mut scene = Scene::new();
    let grid = scene.glyphs();
    let on_glyph = scene.inline(&grid, [1.0, 1.0], Some(block([1.0, 1.0])), 0.0);
    let on_blank = scene.inline(&grid, [3.0, 1.0], Some(block([3.0, 1.0])), 0.0);

    assert_eq!(
        [at(&on_glyph, 1.5, 1.5), at(&on_blank, 3.5, 1.5)],
        [rgb(CELL), CURSOR],
        "the glyph's cell, then a blank cell, under a settled block",
    );
}

/// A block easing from a blank cell onto the glyph draws the glyph only where
/// it covers it. A warped block draws it only inside its own quad, though the
/// quad's bounding box reaches further.
#[test]
fn a_moving_block_draws_the_glyph_only_inside_itself() {
    let mut scene = Scene::new();
    let grid = scene.glyphs();
    let sliding = scene.inline(&grid, [0.6, 1.0], Some(block([0.6, 1.0])), 0.0);
    // The right side leans from the top-right corner to half a cell in.
    let leaning = [[1.0, 1.0], [2.0, 1.0], [1.0, 2.0], [1.5, 2.0]];
    let warped = scene.inline(&grid, [1.0, 1.0], Some(leaning), 0.0);

    assert_eq!(
        [
            at(&sliding, 0.8, 1.5),
            at(&sliding, 1.3, 1.5),
            at(&sliding, 1.8, 1.5),
            at(&warped, 1.25, 1.5),
            at(&warped, 1.9, 1.9),
        ],
        [CURSOR, rgb(CELL), rgb(INK), rgb(CELL), rgb(INK)],
        "a sliding block over the blank cell, then over the glyph, then the glyph \
         past it, and a warped block over the glyph, then the glyph past its slant",
    );
}

/// An unfocused window's hollow outline, and a frame whose block is hidden,
/// both leave the glyph in its ink.
#[test]
fn a_hollow_or_hidden_block_leaves_the_glyph_in_its_ink() {
    let mut scene = Scene::new();
    let grid = scene.glyphs();
    scene.inline(&grid, [1.0, 1.0], Some(block([1.0, 1.0])), 0.0);
    let hidden = scene.inline(&grid, [1.0, 1.0], None, 0.0);
    scene.renderer.set_cursor_fill(CursorFill::Hollow);
    let hollow = scene.inline(&grid, [1.0, 1.0], Some(block([1.0, 1.0])), 0.0);

    assert_eq!(
        [at(&hidden, 1.5, 1.5), at(&hollow, 1.5, 1.5)],
        [rgb(INK), rgb(INK)],
        "the glyph after a solid block hides, then in the middle of a hollow one",
    );
}

/// The glyph keeps to the block on a scrolled grid, and on the last row, which
/// hangs from the bottom edge off the uniform pitch.
#[test]
fn the_glyph_follows_the_block_on_a_scrolled_grid_and_the_anchored_last_row() {
    let mut scene = Scene::new();
    let grid = scene.glyphs();
    let scrolled = scene.inline(&grid, [1.0, 1.0], Some(block([1.0, 1.0])), 0.5);
    let last = scene.inline(&grid, [1.0, 3.0], Some(block([1.0, 3.0])), 0.0);

    let [width, height] = cell_size(FONT, 1.0);
    let last_row_top = HEIGHT - height as u32;
    assert_eq!(
        [
            at(&scrolled, 1.5, 2.25),
            pixel(&last, WIDTH, (1.5 * width) as u32, last_row_top + 4),
        ],
        [rgb(CELL), rgb(CELL)],
        "the lower part of a block half a row down, and the top of the last row's",
    );
}

/// A pool holding the cursor draws the glyph its own cell holds over the block.
/// It draws none once the block hides, and a plain frame after it draws none of
/// that pool's.
#[test]
fn a_pool_under_the_cursor_draws_its_glyph_over_the_block() {
    let mut scene = Scene::new();
    let (pool, blank) = (scene.glyphs(), scene.cells());
    let pools = [whole_pool(0, &pool, true)];
    let shown = scene.pooled([1.0, 1.0], Some(block([1.0, 1.0])), &pools);
    let hidden = scene.pooled([1.0, 1.0], None, &pools);
    let plain = scene.inline(&blank, [1.0, 1.0], Some(block([1.0, 1.0])), 0.0);

    assert_eq!(
        [
            at(&shown, 1.5, 1.5),
            at(&hidden, 1.5, 1.5),
            at(&plain, 1.5, 1.5)
        ],
        [rgb(CELL), rgb(INK), CURSOR],
        "the pool's glyph under a settled block, then with the block hidden, then a \
         plain frame's blank cell",
    );
}

/// Of two pools under the cursor, the glyph comes from the one drawn on top. A
/// non-occludable pool draws over an occludable one whatever slots they hold.
#[test]
fn the_pool_on_top_of_the_cursor_gives_its_glyph() {
    let mut scene = Scene::new();
    let (over, under) = (scene.cells(), scene.glyphs());
    let drawn = scene.pooled(
        [1.0, 1.0],
        Some(block([1.0, 1.0])),
        &[whole_pool(1, &over, false), whole_pool(0, &under, true)],
    );

    assert_eq!(
        at(&drawn, 1.5, 1.5),
        CURSOR,
        "a settled block over the top pool's blank cell",
    );
}

/// A grid of [`CELL`] cells holding nothing.
fn cells(rows: usize, cols: usize) -> Grid {
    let mut grid = Grid::new(rows, cols);
    for row in 0..rows {
        for col in 0..cols {
            grid.get_mut(row, col).bg = CELL;
        }
    }
    grid
}

/// A grid of [`CELL`] cells with a full block in [`INK`] in column 1 of every
/// row.
fn glyphs(rows: usize, cols: usize) -> Grid {
    let mut grid = cells(rows, cols);
    for row in 0..rows {
        let cell = grid.get_mut(row, 1);
        cell.ch = '\u{2588}';
        cell.fg = INK;
    }
    grid
}

/// The corners of a one-cell block whose top-left sits at `(col, row)`.
fn block([col, row]: [f32; 2]) -> [[f32; 2]; 4] {
    [
        [col, row],
        [col + 1.0, row],
        [col, row + 1.0],
        [col + 1.0, row + 1.0],
    ]
}

/// A pool over the whole surface at the live font size, which takes the cursor.
fn whole_pool(id: u32, grid: &Grid, occludable: bool) -> PoolComposite<'_> {
    PoolComposite {
        id,
        grid,
        font_size: None,
        origin_cells: [0.0; 2],
        scissor: [0, 0, WIDTH, HEIGHT],
        shift_rows: 0.0,
        content_changed: true,
        scrolled_rows: None,
        occludable,
        skip_clear: false,
        changed_rows: None,
        dim: 0.0,
    }
}

/// The pixel at `(x, y)` in cells, on a row of the uniform pitch.
fn at(pixels: &[u8], x: f32, y: f32) -> [u8; 3] {
    let [width, height] = cell_size(FONT, 1.0);
    pixel(pixels, WIDTH, (x * width) as u32, (y * height) as u32)
}

fn rgb(color: Rgb) -> [u8; 3] {
    [color.r, color.g, color.b]
}
