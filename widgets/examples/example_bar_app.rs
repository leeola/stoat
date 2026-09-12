//! A bar stoatty demo. Sub-cell rectangles are laid at every width, at every
//! sub-row offset, overhanging their area, and in motion.
//!
//! [`Bar`] appears inside the editor only in the gutter and the status bar,
//! each at one size. Its design problem is where a sub-cell edge lands against
//! a fractional cell height, which the renderer's snap decides, so every
//! display here puts an edge against a cell edge rather than against another
//! bar.
//!
//! The staircase steps the width one sixteenth at a time over a checkerboard,
//! so a width that snaps to the same pixels as its neighbor shows as a repeat.
//! The hairlines put a one-sixteenth bar at each of the sixteen offsets inside
//! a cell, where a snap either keeps it or loses it. The overhang draws from a
//! negative anchor. The sweep advances one sixteenth per frame, which is the
//! smallest motion the unit expresses.
//!
//! The bar writes no cell fallback, so a session no stoatty answers shows the
//! checkerboard and the labels alone.
//!
//! Run as the PTY shell by the `bar` example.

use ratatui::{
    backend::CrosstermBackend,
    crossterm::event::{self, Event, KeyCode, KeyModifiers},
    layout::Rect,
    style::{Color, Style},
    Frame, Terminal,
};
use std::{io, time::Duration};
use stoat_widgets::{bar::Bar, ApcScene, ApcSession, SessionOptions};

/// The window the `bar` example opens, in cells.
const COLS: u16 = 72;

/// Sixteenths in one cell, which is the unit every coordinate is in.
const CELL: i16 = 16;

/// Steps the staircase and the hairlines run through, one per sixteenth.
const STEPS: i16 = 16;

/// Column the displays start at, leaving room for their labels.
const LEFT: u16 = 8;

/// The two backgrounds the staircase's checkerboard alternates between.
const CHECKER: [[u8; 3]; 2] = [[40, 44, 52], [55, 60, 70]];

/// Bar and label color.
const INK: [u8; 3] = [97, 175, 239];
const LABEL: [u8; 3] = [171, 178, 191];

/// Cells the sweep travels before it wraps.
const SWEEP_CELLS: i16 = 40;

fn main() {
    // The session holds raw mode and the hidden cursor for as long as it lives,
    // and gives both back on the way out of main -- including the way out an
    // `expect` below takes, which its panic hook covers.
    let session = ApcSession::new(SessionOptions {
        raw_mode: true,
        hide_cursor: true,
        ..SessionOptions::default()
    });

    run(session);
}

/// Draw the four displays until the user quits, returning so [`main`]'s session
/// restores the terminal.
fn run(mut session: ApcSession) {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    terminal.clear().expect("clear the screen");

    let mut sweep = 0_i16;

    loop {
        session.scene().clear();
        terminal
            .draw(|frame| render_frame(frame, session.scene(), sweep))
            .expect("draw a frame");
        session.flush().expect("write the frame");

        sweep = (sweep + 1) % (SWEEP_CELLS * CELL);

        // Polled rather than blocking, so the sweep advances between events.
        if !event::poll(Duration::from_millis(16)).expect("poll for an event") {
            continue;
        }
        let Event::Key(key) = event::read().expect("read a terminal event") else {
            continue;
        };
        if key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return;
        }
    }
}

/// Draw the staircase, the hairlines, the overhang, and the sweep.
fn render_frame(frame: &mut Frame<'_>, scene: &mut ApcScene, sweep: i16) {
    let mut y = 1;
    y = draw_staircase(frame, scene, y) + 1;
    y = draw_hairlines(frame, scene, y) + 1;
    y = draw_overhang(frame, scene, y) + 1;
    y = draw_sweep(frame, scene, y, sweep) + 1;

    label(frame, 2, y, "q: quit");
}

/// Draw sixteen bars whose widths step one sixteenth at a time, over a
/// checkerboard, returning the row under them.
///
/// Each bar sits one whole cell plus one sixteenth past the last, so its left
/// edge walks across the cell while its width grows. A width that snaps to the
/// same pixels as its neighbor therefore shows as a repeat rather than hiding
/// behind an aligned start.
fn draw_staircase(frame: &mut Frame<'_>, scene: &mut ApcScene, top: u16) -> u16 {
    label(frame, 2, top, "width");

    let width = COLS - LEFT;
    for column in 0..width {
        let square = CHECKER[(column % 2) as usize];
        frame.buffer_mut()[(LEFT + column, top)]
            .set_bg(Color::Rgb(square[0], square[1], square[2]));
    }

    let area = Rect {
        x: LEFT,
        y: top,
        width,
        height: 1,
    };
    for step in 0..STEPS {
        frame.render_stateful_widget(
            Bar {
                x: step * CELL + step,
                y: 0,
                width: step as u16 + 1,
                height: CELL as u16,
                color: INK,
            },
            area,
            scene,
        );
    }

    top
}

/// Draw a one-sixteenth bar at each of the sixteen offsets inside a cell,
/// returning the row under them.
///
/// A hairline is where the snap shows plainest. At some offsets it lands on a
/// whole pixel and at others it falls between two, so the row reads as sixteen
/// lines or as fewer.
fn draw_hairlines(frame: &mut Frame<'_>, scene: &mut ApcScene, top: u16) -> u16 {
    const ROWS: u16 = 4;
    label(frame, 2, top, "hair");

    let area = Rect {
        x: LEFT,
        y: top,
        width: 20,
        height: ROWS,
    };
    for step in 0..STEPS {
        frame.render_stateful_widget(
            Bar {
                x: 0,
                y: step,
                width: 20 * CELL as u16,
                height: 1,
                color: INK,
            },
            area,
            scene,
        );
    }

    top + ROWS - 1
}

/// Draw one bar anchored above and left of its own area, returning its row.
///
/// A negative anchor is what a gutter separator uses to sit in the margin
/// beside the text rather than inside it, so the demo shows the bar leaving
/// the area it was rendered into.
fn draw_overhang(frame: &mut Frame<'_>, scene: &mut ApcScene, top: u16) -> u16 {
    label(frame, 2, top + 1, "over");

    frame.render_stateful_widget(
        Bar {
            x: -CELL / 2,
            y: -CELL / 2,
            width: 3 * CELL as u16,
            height: CELL as u16,
            color: INK,
        },
        Rect {
            x: LEFT,
            y: top + 1,
            width: 6,
            height: 2,
        },
        scene,
    );

    top + 2
}

/// Draw one bar whose left edge advances a sixteenth per frame, returning its
/// row.
///
/// One sixteenth is the smallest step the unit expresses, so a sweep at that
/// rate is what separates smooth sub-cell motion from a bar that jumps a whole
/// cell at a time.
fn draw_sweep(frame: &mut Frame<'_>, scene: &mut ApcScene, top: u16, sweep: i16) -> u16 {
    label(frame, 2, top, "sweep");

    frame.render_stateful_widget(
        Bar {
            x: sweep,
            y: 0,
            width: 4,
            height: CELL as u16,
            color: INK,
        },
        Rect {
            x: LEFT,
            y: top,
            width: SWEEP_CELLS as u16,
            height: 1,
        },
        scene,
    );

    top
}

/// Write one cell-grid label naming a display.
fn label(frame: &mut Frame<'_>, x: u16, y: u16, text: &str) {
    let style = Style::default().fg(Color::Rgb(LABEL[0], LABEL[1], LABEL[2]));
    frame
        .buffer_mut()
        .set_stringn(x, y, text, COLS as usize, style);
}
