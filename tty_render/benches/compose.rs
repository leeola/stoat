//! Divan benchmarks for one headless frame through the render passes.
//!
//! Each case builds a device, a renderer, and a populated grid once outside the
//! timed body, then times the encode, the submit, and the wait for the frame to
//! finish. Divan has no untimed per-iteration teardown, so a body that only
//! submitted would queue frames without bound and report a shrinking share of
//! the real cost. Waiting inside means the number is what a frame costs.
//!
//! Every case skips with a message when no adapter answers, so a machine
//! without a GPU still runs the rest of the suite.

use std::cell::Cell;
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
/// roughly 100 by 33 cells.
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

/// Draw one frame over `damage` and wait for it to finish.
fn draw(bench: &mut Bench, damage: &Damage) {
    render_frame(
        &mut bench.renderer,
        &bench.device,
        &bench.queue,
        &bench.view,
        &bench.grid,
        damage,
    );
    let _ = bench.device.poll(PollType::wait_indefinitely());
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
    let Some(mut bench) = setup() else {
        skipped("full_damage_text");
        return;
    };
    bencher.bench_local(|| draw(&mut bench, &Damage::Full));
}

/// A screenful of prose no run has been shaped for, which is what a fling
/// through scrollback costs.
///
/// The renderer stays warm across iterations and the content does not: every
/// frame draws lines the shape cache has never seen, spelled from letters the
/// glyph atlas already holds. That is a fling exactly. The cases above all
/// redraw text that was already shaped, so this is the only one whose cost
/// includes shaping at all.
///
/// It reports a whole frame, not the shaping inside it. The body waits for the
/// GPU, and at this surface size that wait covers the CPU work, so a change
/// that only moves shaping cost does not move this number. Counting what gets
/// shaped is what shows that; timing a frame is what shows whether it matters.
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
        .bench_local_refs(|grid| {
            render_frame(&mut renderer, &device, &queue, &view, grid, &Damage::Full);
            let _ = device.poll(PollType::wait_indefinitely());
        });
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

/// The same screen with the chrome passes drawing too.
#[divan::bench]
fn full_damage_with_chrome(bencher: divan::Bencher<'_, '_>) {
    let Some(mut bench) = setup() else {
        skipped("full_damage_with_chrome");
        return;
    };
    add_chrome(&mut bench.grid);
    bencher.bench_local(|| draw(&mut bench, &Damage::Full));
}

/// One changed row, which is what a keystroke costs. The gap between this and
/// the full-damage case is what the per-row caches are worth.
#[divan::bench]
fn one_damaged_row(bencher: divan::Bencher<'_, '_>) {
    let Some(mut bench) = setup() else {
        skipped("one_damaged_row");
        return;
    };
    let cols = bench.grid.cols() as u16;
    let damage = Damage::Partial(vec![Some((0, cols.saturating_sub(1)))]);
    bencher.bench_local(|| draw(&mut bench, &damage));
}
