//! Divan benchmarks for one headless frame through the render passes.
//!
//! Each case builds a device, a renderer, and a populated grid once outside the
//! timed body, then times the encode and the submit. That is the CPU a frame
//! spends, which is what the passes here are written to move.
//!
//! The wait for the device falls outside the sample. A body returns a [`Wait`]
//! guard, and divan drops a body's return value after it ends the sample, so
//! the frame is still waited for before the next iteration starts. That bound
//! is what keeps the bodies from queueing frames without limit.
//!
//! Every case skips with a message when no adapter answers, so a machine
//! without a GPU still runs the rest of the suite.

use std::{cell::Cell, fmt::Write};
use stoatty_render::gpu::{
    build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll,
};
use stoatty_term::{
    grid::{Bar, Border, BorderEdge, BorderStyle, Grid, Icon, IconKind, Panel, PanelShadow, Rgb},
    term::Damage,
};
use wgpu::{
    Device, Extent3d, PollType, Queue, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureView, TextureViewDescriptor,
};

/// Offscreen target size in physical pixels, which at font size 15 is a grid of
/// 133 by 40 cells.
const WIDTH: u32 = 1200;
const HEIGHT: u32 = 720;

fn main() {
    divan::main();
}

/// Everything one case needs to draw, built untimed.
struct Bench {
    device: Device,
    queue: Queue,
    view: TextureView,
    renderer: Renderer,
    grid: Grid,
}

/// Build a device, a renderer, and a grid of text, or `None` when no adapter
/// answers.
fn setup() -> Option<Bench> {
    let (device, queue) = headless_device()?;
    let format = TextureFormat::Rgba8Unorm;

    let target = device.create_texture(&TextureDescriptor {
        label: Some("compose bench target"),
        size: Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&TextureViewDescriptor::default());

    let renderer = build_renderer(&device);

    let (rows, cols) = renderer.grid_size();
    let mut grid = Grid::new(rows, cols);
    fill_text(&mut grid, rows, cols);

    Some(Bench {
        device,
        queue,
        view,
        renderer,
        grid,
    })
}

/// A renderer over the standard bench target, with its caches empty.
fn build_renderer(device: &Device) -> Renderer {
    Renderer::new(
        device,
        TextureFormat::Rgba8Unorm,
        [WIDTH, HEIGHT],
        build_font_system(),
        FontConfig {
            size: 15,
            scale_factor: 1.0,
            family: &["JetBrains Mono".to_owned()],
            ligatures: true,
        },
        Rgb::new(0, 0, 0),
        Rgb::new(217, 217, 217),
    )
}

/// Fill every cell with printable text in a repeating color cycle, so the text
/// pass shapes a full screen rather than a sparse one.
fn fill_text(grid: &mut Grid, rows: usize, cols: usize) {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789 ";

    for row in 0..rows {
        for col in 0..cols {
            let cell = grid.get_mut(row, col);
            cell.ch = ALPHABET[(row * cols + col) % ALPHABET.len()] as char;
            cell.fg = Rgb::new(200, 200 - (col % 60) as u8, 180);
        }
    }
}

/// Chrome over the text: a border, a filled panel with a shadow, three icons,
/// and two sub-cell bars, so the passes a plain text screen never reaches draw
/// too.
fn add_chrome(grid: &mut Grid) {
    grid.set_border_edge(
        0,
        0..1,
        BorderEdge::Top,
        Border {
            style: BorderStyle::Rounded,
            color: Rgb::new(255, 0, 0),
        },
    );
    grid.set_panels(vec![Panel {
        top: 4,
        left: 2,
        width: 40,
        height: 12,
        style: BorderStyle::Rounded,
        border: Rgb::new(180, 180, 220),
        corner_radius: 6,
        fill: Some(Rgb::new(30, 30, 50)),
        shadow: PanelShadow::Drop,
        inset_x: 0,
        above_pools: false,
        anchor: None,
        seq: 0,
    }]);
    grid.set_icons(vec![
        Icon {
            top: 5,
            left: 0,
            kind: IconKind::Error,
            color: Rgb::new(220, 50, 47),
            size: 1,
            offset: [0, 0],
            seq: 0,
        },
        Icon {
            top: 5,
            left: 2,
            kind: IconKind::Warning,
            color: Rgb::new(255, 200, 0),
            size: 1,
            offset: [0, 0],
            seq: 0,
        },
        Icon {
            top: 5,
            left: 4,
            kind: IconKind::Info,
            color: Rgb::new(38, 139, 210),
            size: 1,
            offset: [0, 0],
            seq: 0,
        },
    ]);
    grid.set_bars(vec![
        Bar {
            x: 0,
            y: 80,
            width: 3,
            height: 16,
            color: Rgb::new(220, 50, 47),
            seq: 0,
        },
        Bar {
            x: 30,
            y: 0,
            width: 1,
            height: 96,
            color: Rgb::new(88, 88, 88),
            seq: 0,
        },
    ]);
}

/// Waits for the submitted frame to finish, on drop.
///
/// Returned from a timed body so the wait lands after divan ends the sample.
/// The wait still happens before the next iteration, which is what stops the
/// bodies from running ahead of the device.
struct Wait<'a>(&'a Device);

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        let _ = self.0.poll(PollType::wait_indefinitely());
    }
}

/// Draw one frame over `damage`, leaving the wait to the guard it returns.
///
/// Takes the parts rather than the whole [`Bench`], since a guard borrowing
/// the device outlives the call, and a mutable borrow of the struct that holds
/// the device rules that out.
fn draw<'a>(
    renderer: &mut Renderer,
    device: &'a Device,
    queue: &Queue,
    view: &TextureView,
    grid: &Grid,
    damage: &Damage,
) -> Wait<'a> {
    render_frame(renderer, device, queue, view, grid, damage);
    Wait(device)
}

/// Encode and submit one frame, leaving the wait to the caller.
fn render_frame(
    renderer: &mut Renderer,
    device: &Device,
    queue: &Queue,
    view: &TextureView,
    grid: &Grid,
    damage: &Damage,
) {
    renderer.render_into(
        device,
        queue,
        view,
        grid,
        Frame {
            cursor: Some([0.0, 0.0]),
            cursor_corners: Some([[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]),
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers: &[],
            },
            damage,
            decoration_damage: &Damage::Partial(Vec::new()),
            scrolled_rows: 0,
            sketch_reveals: &[],
        },
    );
}

/// Announce the skip once per case, so an adapter-less run says why it reported
/// nothing rather than looking like a bench that vanished.
fn skipped(case: &str) {
    eprintln!("compose: no wgpu adapter available, skipping {case}");
}

/// A full screen of text rebuilt from scratch, which is what the first frame
/// after a resize or a theme change costs.
#[divan::bench]
fn full_damage_text(bencher: divan::Bencher<'_, '_>) {
    let Some(bench) = setup() else {
        skipped("full_damage_text");
        return;
    };
    let Bench {
        device,
        queue,
        view,
        mut renderer,
        grid,
    } = bench;

    bencher.bench_local(|| draw(&mut renderer, &device, &queue, &view, &grid, &Damage::Full));
}

/// A screenful of prose no run has been shaped for, which is what a fling
/// through scrollback costs.
///
/// The renderer stays warm across iterations and the content does not: every
/// frame draws lines the shape cache has never seen, spelled from letters the
/// glyph atlas already holds. That is a fling exactly. The cases above all
/// redraw text that was already shaped, so shaping costs them nothing.
///
/// It reports a whole frame's CPU, not the shaping alone, but shaping is part
/// of that CPU here, so a change that moves shaping cost moves this number.
/// Counting what gets shaped says how much work a change removed. This says
/// what the removal is worth against everything else a frame does.
///
/// See also:
/// - [`novel_words`], which misses on every run rather than on one per row.
#[divan::bench]
fn fresh_rows(bencher: divan::Bencher<'_, '_>) {
    let Some(bench) = setup() else {
        skipped("fresh_rows");
        return;
    };
    let Bench {
        device,
        queue,
        view,
        mut renderer,
        ..
    } = bench;
    let (rows, cols) = renderer.grid_size();

    // One frame of the same shape fills the atlas, so the timed frames rasterize
    // no glyph for the first time.
    render_frame(
        &mut renderer,
        &device,
        &queue,
        &view,
        &prose_grid(rows, cols, 0),
        &Damage::Full,
    );
    let _ = device.poll(PollType::wait_indefinitely());

    let frame = Cell::new(1usize);
    bencher
        .with_inputs(|| {
            let at = frame.get();
            frame.set(at + 1);
            prose_grid(rows, cols, at)
        })
        .bench_local_refs(|grid| draw(&mut renderer, &device, &queue, &view, grid, &Damage::Full));
}

/// A screen of numbered source-like prose, as though `frame` screenfuls had
/// already scrolled past.
///
/// The rows share every word but the number that leads them, which is what
/// scrollback is: lines novel as wholes and almost entirely familiar as words.
fn prose_grid(rows: usize, cols: usize, frame: usize) -> Grid {
    const TAIL: &str = " fn handle_event(ev) -> Result<()> { self.dispatch(ev)?; }";

    let mut grid = Grid::new(rows, cols);
    for row in 0..rows {
        let line = format!("line {:07}{TAIL}", frame * rows + row);
        let mut chars = line.chars();
        for col in 0..cols {
            let cell = grid.get_mut(row, col);
            cell.ch = chars.next().unwrap_or(' ');
            cell.fg = Rgb::new(200, 200, 180);
        }
    }
    grid
}

/// A screenful of words no run has been shaped for, which is what a fling
/// through a hexdump, a log, or a column of hashes costs.
///
/// [`fresh_rows`] is novel by the line and familiar by the word: its rows share
/// every word but the number that leads them, so a frame takes a few dozen
/// run-cache misses against a few hundred runs. Here no word repeats, so every
/// run misses.
///
/// A miss is where the renderer decides anything about shaping at all, since
/// the reshape gate runs only inside the cache's miss closure. This is
/// therefore the case that weighs that gate through the renderer, rather than
/// beside it the way the shape bench does.
#[divan::bench]
fn novel_words(bencher: divan::Bencher<'_, '_>) {
    let Some(bench) = setup() else {
        skipped("novel_words");
        return;
    };
    let Bench {
        device,
        queue,
        view,
        mut renderer,
        ..
    } = bench;
    let (rows, cols) = renderer.grid_size();

    // One frame of the same shape fills the atlas, so the timed frames rasterize
    // no glyph for the first time.
    render_frame(
        &mut renderer,
        &device,
        &queue,
        &view,
        &novel_grid(rows, cols, 0),
        &Damage::Full,
    );
    let _ = device.poll(PollType::wait_indefinitely());

    let frame = Cell::new(1usize);
    bencher
        .with_inputs(|| {
            let at = frame.get();
            frame.set(at + 1);
            novel_grid(rows, cols, at)
        })
        .bench_local_refs(|grid| draw(&mut renderer, &device, &queue, &view, grid, &Damage::Full));
}

/// A screen of hexdump-like tokens, as though `frame` screenfuls of them had
/// already scrolled past.
///
/// Every token differs from every other token on the screen, and from every
/// token on every other frame, so no run the text pass builds is one the shape
/// cache holds.
///
/// The tokens are hex because sixteen digits fit the glyph atlas after one
/// frame, which leaves rasterization out of what the case measures. Groups of
/// two digits were the other candidate and name only 256 tokens, which the
/// cache holds in full within two frames, so that grid would measure the hit
/// path instead.
///
/// A screen of 40 rows by 133 columns holds 560 tokens, so eight frames fill
/// the run cache's 4,096 entries. Its steady state therefore pays the eviction
/// as well as the miss, which is what a real fling through unique words pays.
fn novel_grid(rows: usize, cols: usize, frame: usize) -> Grid {
    /// Columns one token takes: eight hex digits and the space after it.
    const TOKEN_COLUMNS: usize = 9;

    // Whole tokens only, so a row ends on a token boundary and pads with
    // spaces. A truncated token is novel too, but the count per frame stops
    // being arithmetic anyone can repeat.
    let per_row = (cols + 1) / TOKEN_COLUMNS;

    let mut grid = Grid::new(rows, cols);
    let mut line = String::new();

    for row in 0..rows {
        line.clear();
        let first = (frame * rows + row) * per_row;
        for index in 0..per_row {
            if index > 0 {
                line.push(' ');
            }
            write!(line, "{:08x}", first + index).expect("writing to a String is infallible");
        }

        let mut chars = line.chars();
        for col in 0..cols {
            let cell = grid.get_mut(row, col);
            cell.ch = chars.next().unwrap_or(' ');
            // One color for the whole screen, so a run breaks at a space the
            // way a hexdump's does rather than at every column.
            cell.fg = Rgb::new(200, 200, 180);
        }
    }
    grid
}

/// The same screen with the chrome passes drawing too.
#[divan::bench]
fn full_damage_with_chrome(bencher: divan::Bencher<'_, '_>) {
    let Some(mut bench) = setup() else {
        skipped("full_damage_with_chrome");
        return;
    };
    add_chrome(&mut bench.grid);
    let Bench {
        device,
        queue,
        view,
        mut renderer,
        grid,
    } = bench;

    bencher.bench_local(|| draw(&mut renderer, &device, &queue, &view, &grid, &Damage::Full));
}

/// One changed row, which is what a keystroke costs. The gap between this and
/// the full-damage case is what the per-row caches are worth.
///
/// That gap is in CPU. The device wait is the same either way and far larger
/// than both, so while it sat inside the sample it hid this case entirely, and
/// one row read slower than a whole novel screen.
#[divan::bench]
fn one_damaged_row(bencher: divan::Bencher<'_, '_>) {
    let Some(bench) = setup() else {
        skipped("one_damaged_row");
        return;
    };
    let cols = bench.grid.cols() as u16;
    let damage = Damage::Partial(vec![Some((0, cols.saturating_sub(1)))]);
    let Bench {
        device,
        queue,
        view,
        mut renderer,
        grid,
    } = bench;

    bencher.bench_local(|| draw(&mut renderer, &device, &queue, &view, &grid, &damage));
}
