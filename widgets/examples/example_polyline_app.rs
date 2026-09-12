//! A polyline stoatty demo. A commit-graph figure sits beside a canvas the
//! pointer draws on.
//!
//! [`Polyline`] is the kit's only non-axis-aligned primitive, and inside the
//! editor its only consumer is the commit graph. The left half reproduces that
//! shape, so a lane, a dot, and a merge edge read here without opening a
//! repository. The right half strokes whatever the pointer puts down, at a
//! width the keys step, so a weight reads against a real path rather than
//! against a fixture.
//!
//! The renderer blends a path of up to twelve points as one instance and chunks
//! a longer one at a shared joint. The animated sine runs to forty points, so
//! the chunk boundary moves across the path while it draws.
//!
//! Run as the PTY shell by the `polyline` example.

use ratatui::{
    backend::CrosstermBackend,
    crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind},
    layout::Rect,
    style::Style,
    Frame, Terminal,
};
use std::{f32::consts::TAU, io, time::Duration};
use stoat_widgets::{polyline::Polyline, ApcScene, ApcSession, SessionOptions};

/// The window the `polyline` example opens, in cells.
const COLS: u16 = 90;
const ROWS: u16 = 28;

/// Sixteenths in one cell, which is the unit every coordinate here is in.
const CELL: i16 = 16;

/// The commit-graph figure's area, on the left.
const GRAPH: Rect = Rect {
    x: 2,
    y: 1,
    width: 24,
    height: ROWS - 4,
};

/// The canvas the pointer draws on, right of the figure.
const CANVAS: Rect = Rect {
    x: 30,
    y: 1,
    width: COLS - 32,
    height: ROWS - 4,
};

/// The row the caption is written on, under both halves.
const CAPTION_Y: u16 = ROWS - 2;

/// Lane columns in the figure, in cells from its left edge.
const LANES: [i16; 3] = [4, 8, 12];

/// One color per lane, so a dot and its lane read as one strand.
const LANE_COLORS: [[u8; 3]; 3] = [[97, 175, 239], [152, 195, 121], [224, 108, 117]];

/// The merge edge's color, distinct from every lane it joins.
const EDGE_COLOR: [u8; 3] = [198, 120, 221];

/// The drawn path's color.
const INK: [u8; 3] = [229, 192, 123];

/// Lane stroke width in sixteenths of a cell.
const LANE_WIDTH: u16 = 3;

/// Commit dot width in sixteenths of a cell.
const DOT_WIDTH: u16 = 8;

/// Rows between commit dots on a lane.
const DOT_EVERY: u16 = 3;

/// Steps the merge edge interpolates between its two lanes.
const CURVE_STEPS: i16 = 8;

/// How far the edge runs straight before it bends, in sixteenths.
const CURVE_STRAIGHT: i16 = 6;

/// Points in the animated sine, past the twelve one instance carries.
const SINE_POINTS: usize = 40;

/// Phase the sine advances per frame, in radians.
const SINE_STEP: f32 = 0.08;

fn main() {
    // The session holds raw mode, mouse reporting, and the hidden cursor for as
    // long as it lives, and gives all three back on the way out of main --
    // including the way out an `expect` below takes, which its panic hook
    // covers.
    let session = ApcSession::new(SessionOptions {
        raw_mode: true,
        mouse_capture: true,
        hide_cursor: true,
        ..SessionOptions::default()
    });

    run(session);
}

/// Draw the figure and the canvas until the user quits, returning so [`main`]'s
/// session restores the terminal.
fn run(mut session: ApcSession) {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    terminal.clear().expect("clear the screen");

    let mut canvas = CanvasState {
        paths: vec![Vec::new()],
        width: 4,
        sine: false,
        phase: 0.0,
    };

    loop {
        session.scene().clear();
        terminal
            .draw(|frame| render_frame(frame, session.scene(), &canvas))
            .expect("draw a frame");
        session.flush().expect("write the frame");

        if canvas.sine {
            canvas.phase = (canvas.phase + SINE_STEP) % TAU;
        }

        // Polled rather than blocking, so the sine advances between events.
        if !event::poll(Duration::from_millis(16)).expect("poll for an event") {
            continue;
        }
        match event::read().expect("read a terminal event") {
            Event::Mouse(mouse) => canvas.press(mouse.kind, mouse.column, mouse.row),
            Event::Key(key) => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return;
                }
                match key.code {
                    KeyCode::Char('q') => return,
                    KeyCode::Char('n') => canvas.paths.push(Vec::new()),
                    KeyCode::Char('c') => canvas.paths = vec![Vec::new()],
                    KeyCode::Char('w') => canvas.width = (canvas.width + 1).min(16),
                    KeyCode::Char('W') => canvas.width = canvas.width.saturating_sub(1).max(1),
                    KeyCode::Char('a') => canvas.sine = !canvas.sine,
                    _ => {},
                }
            },
            _ => {},
        }
    }
}

/// What the pointer and the keys have put on the canvas.
struct CanvasState {
    /// Each drawn path's vertices, in sixteenths from the canvas's top-left.
    /// The last is the one a press extends.
    paths: Vec<Vec<[i16; 2]>>,
    /// Stroke width every drawn path takes, in sixteenths of a cell.
    width: u16,
    /// Whether the animated sine draws.
    sine: bool,
    /// The sine's phase, advanced each frame while it draws.
    phase: f32,
}

impl CanvasState {
    /// Put a vertex where a left press landed, if it landed on the canvas.
    ///
    /// The pty reports the pointer in whole cells, so the vertex takes the
    /// cell's center rather than its corner. A path laid on the corners hugs
    /// the grid, which is what a sub-cell primitive exists to avoid.
    fn press(&mut self, kind: MouseEventKind, column: u16, row: u16) {
        if kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let inside = (CANVAS.x..CANVAS.x + CANVAS.width).contains(&column)
            && (CANVAS.y..CANVAS.y + CANVAS.height).contains(&row);
        if !inside {
            return;
        }

        let point = [
            (column - CANVAS.x) as i16 * CELL + CELL / 2,
            (row - CANVAS.y) as i16 * CELL + CELL / 2,
        ];
        self.paths
            .last_mut()
            .expect("the canvas always holds a path")
            .push(point);
    }
}

/// Draw the commit-graph figure, the canvas paths, and the caption.
fn render_frame(frame: &mut Frame<'_>, scene: &mut ApcScene, canvas: &CanvasState) {
    draw_graph(frame, scene);
    draw_canvas(frame, scene, canvas);

    let caption = format!(
        "click: vertex  n: new path  w/W: width {:>2}  a: sine {}  c: clear  q: quit",
        canvas.width,
        match canvas.sine {
            true => "on",
            false => "off",
        },
    );
    frame
        .buffer_mut()
        .set_stringn(2, CAPTION_Y, &caption, COLS as usize - 4, Style::default());
}

/// Draw three lanes, their commit dots, and one merge edge between lanes.
///
/// The figure is the shape the commit graph draws, so a lane's weight against
/// its dot and the bend of a merge read here rather than in the editor.
fn draw_graph(frame: &mut Frame<'_>, scene: &mut ApcScene) {
    let bottom = (GRAPH.height as i16 - 1) * CELL + CELL / 2;

    for (lane, color) in LANES.iter().zip(LANE_COLORS) {
        let x = lane * CELL + CELL / 2;
        stroke(
            frame,
            scene,
            GRAPH,
            vec![[x, CELL / 2], [x, bottom]],
            LANE_WIDTH,
            color,
        );

        for row in (0..GRAPH.height).step_by(DOT_EVERY as usize) {
            let y = row as i16 * CELL + CELL / 2;
            stroke(frame, scene, GRAPH, vec![[x, y]], DOT_WIDTH, color);
        }
    }

    stroke(
        frame,
        scene,
        GRAPH,
        merge_edge(0, 1, 4),
        LANE_WIDTH,
        EDGE_COLOR,
    );
}

/// A merge edge from lane `from` to lane `to`, leaving row `y` and arriving at
/// the row below it.
///
/// A stub, the smoothstepped steps across, and a stub, which is how the commit
/// graph shapes one. The straight ends are what keep the bend clear of the dots
/// it passes.
fn merge_edge(from: usize, to: usize, y: u16) -> Vec<[i16; 2]> {
    let (x0, x1) = (LANES[from] * CELL + CELL / 2, LANES[to] * CELL + CELL / 2);
    let (y0, y1) = (y as i16 * CELL + CELL / 2, (y as i16 + 1) * CELL + CELL / 2);
    let (bend_top, bend_bottom) = (y0 + CURVE_STRAIGHT, y1 - CURVE_STRAIGHT);

    let mut points = vec![[x0, y0]];
    points.extend((0..=CURVE_STEPS).map(|step| {
        let t = f32::from(step) / f32::from(CURVE_STEPS);
        let eased = t * t * (3.0 - 2.0 * t);
        [
            x0 + (f32::from(x1 - x0) * eased).round() as i16,
            bend_top + (f32::from(bend_bottom - bend_top) * t).round() as i16,
        ]
    }));
    points.push([x1, y1]);

    points
}

/// Draw every path the pointer laid down, and the sine when it is on.
fn draw_canvas(frame: &mut Frame<'_>, scene: &mut ApcScene, canvas: &CanvasState) {
    for path in &canvas.paths {
        if path.is_empty() {
            continue;
        }
        stroke(frame, scene, CANVAS, path.clone(), canvas.width, INK);
    }

    if canvas.sine {
        stroke(
            frame,
            scene,
            CANVAS,
            sine_points(canvas.phase),
            canvas.width,
            EDGE_COLOR,
        );
    }
}

/// A sine along the canvas bottom at `phase`, long enough that the renderer
/// chunks it at a shared joint.
fn sine_points(phase: f32) -> Vec<[i16; 2]> {
    let span = (CANVAS.width as i16 - 1) * CELL;
    let mid = (CANVAS.height as i16 - 3) * CELL;
    let swing = CELL * 2;

    (0..SINE_POINTS)
        .map(|step| {
            let t = step as f32 / (SINE_POINTS - 1) as f32;
            [
                (f32::from(span) * t).round() as i16,
                mid + (swing as f32 * (t * TAU * 2.0 + phase).sin()).round() as i16,
            ]
        })
        .collect()
}

/// Render one path into `area`, which is where every coordinate above is
/// measured from.
fn stroke(
    frame: &mut Frame<'_>,
    scene: &mut ApcScene,
    area: Rect,
    points: Vec<[i16; 2]>,
    width: u16,
    color: [u8; 3],
) {
    frame.render_stateful_widget(
        Polyline {
            points,
            width,
            color,
        },
        area,
        scene,
    );
}
