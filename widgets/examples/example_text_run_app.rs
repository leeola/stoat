//! A text-run stoatty demo. One string is drawn across a scale ramp, with and
//! without a backing box, and at sub-cell nudges.
//!
//! [`TextRun`] appears inside the editor only as a panel title and as sketch
//! labels, each at one scale. How a fractional glyph size sits on its row, how
//! a box-less run blends over what is behind it, and where a sub-cell nudge
//! lands against a cell edge are therefore invisible without a display built
//! for them.
//!
//! Three displays cover it. The ramp puts nine scales under each other, each
//! labeled in grid-size cells so the scaled run is measured against ordinary
//! text. The box row draws the same string twice over a checkerboard, opaque
//! beside blended. The nudge grid steps a run through sixteenths of a cell over
//! drawn cell edges.
//!
//! The run writes no cell fallback, so a session no stoatty answers shows the
//! labels and the grid alone.
//!
//! Run as the PTY shell by the `text_run` example.

use ratatui::{
    backend::CrosstermBackend,
    crossterm::event::{self, Event, KeyCode, KeyModifiers},
    layout::Rect,
    style::{Color, Style},
    Frame, Terminal,
};
use std::io;
use stoat_widgets::{text_run::TextRun, ApcScene, ApcSession, SessionOptions};

/// The window the `text_run` example opens, in cells.
const COLS: u16 = 80;

/// Sixteenths in one cell, which is the unit an anchor is in.
const CELL: i16 = 16;

/// The string every display draws, mixing letters and digits so a scale's
/// effect on both reads at once.
const SAMPLE: &str = "Stoatty 0123";

/// The scales the ramp steps through, in 256ths of the cell size.
const RAMP: [u16; 9] = [96, 128, 160, 192, 224, 256, 320, 384, 512];

/// The scale the box row and the nudge grid draw at.
const FIXED_SCALE: u16 = 160;

/// Column the ramp's runs start at, leaving room for their cell labels.
const RAMP_X: u16 = 8;

/// The two backgrounds the box row's checkerboard alternates between.
const CHECKER: [[u8; 3]; 2] = [[40, 44, 52], [55, 60, 70]];

/// The opaque box the boxed run carries.
const BOX_BG: [u8; 3] = [40, 44, 52];

/// Run and label color.
const INK: [u8; 3] = [171, 178, 191];

/// The live run's scale bounds and the step the keys move it by.
const LIVE_MIN: u16 = 64;
const LIVE_MAX: u16 = 512;
const LIVE_STEP: u16 = 8;

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

/// Draw the three displays until the user quits, returning so [`main`]'s
/// session restores the terminal.
fn run(mut session: ApcSession) {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    terminal.clear().expect("clear the screen");

    let mut live = FIXED_SCALE;

    loop {
        session.scene().clear();
        terminal
            .draw(|frame| render_frame(frame, session.scene(), live))
            .expect("draw a frame");
        session.flush().expect("write the frame");

        let Event::Key(key) = event::read().expect("read a terminal event") else {
            continue;
        };
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match key.code {
            KeyCode::Char('q') => return,
            KeyCode::Char('+') | KeyCode::Char('=') => live = (live + LIVE_STEP).min(LIVE_MAX),
            KeyCode::Char('-') => live = live.saturating_sub(LIVE_STEP).max(LIVE_MIN),
            _ => {},
        }
    }
}

/// Draw the ramp, the box row, the nudge grid, and the live run.
fn render_frame(frame: &mut Frame<'_>, scene: &mut ApcScene, live: u16) {
    let mut y = draw_ramp(frame, scene);
    y = draw_box_row(frame, scene, y + 1);
    y = draw_nudge_grid(frame, scene, y + 1);
    draw_live(frame, scene, y + 1, live);
}

/// Draw one run of [`SAMPLE`] per scale, top to bottom, each labeled.
///
/// A group is as many rows tall as its run needs, so a scale past one cell
/// overlaps neither the label beside it nor the group below.
fn draw_ramp(frame: &mut Frame<'_>, scene: &mut ApcScene) -> u16 {
    let mut y = 0;

    for scale in RAMP {
        let rows = scale.div_ceil(256) + 1;
        let area = Rect {
            x: RAMP_X,
            y,
            width: COLS - RAMP_X,
            height: rows,
        };

        label(frame, 2, y, &format!("{scale:>3}"));
        frame.render_stateful_widget(
            TextRun {
                col: 0,
                row: 0,
                scale,
                color: INK,
                bg: None,
                text: SAMPLE,
                follow: 0,
                anchor: None,
            },
            area,
            scene,
        );

        y += rows;
    }

    y
}

/// Draw the same run boxed and box-less over a checkerboard, returning the row
/// under it.
///
/// The checkerboard is what separates the two. An opaque box carries its own
/// background across the squares, and a box-less run lets them through.
fn draw_box_row(frame: &mut Frame<'_>, scene: &mut ApcScene, top: u16) -> u16 {
    let height = 2;
    label(frame, 2, top, "box");

    for column in 0..(COLS - RAMP_X) {
        for row in 0..height {
            let square = CHECKER[((column / 4 + row) % 2) as usize];
            frame.buffer_mut()[(RAMP_X + column, top + row)]
                .set_bg(Color::Rgb(square[0], square[1], square[2]));
        }
    }

    let area = Rect {
        x: RAMP_X,
        y: top,
        width: COLS - RAMP_X,
        height,
    };
    for (index, bg) in [Some(BOX_BG), None].into_iter().enumerate() {
        frame.render_stateful_widget(
            TextRun {
                col: index as i16 * 20 * CELL,
                row: 0,
                scale: FIXED_SCALE,
                color: INK,
                bg,
                text: SAMPLE,
                follow: 0,
                anchor: None,
            },
            area,
            scene,
        );
    }

    top + height
}

/// Draw runs at sub-cell offsets over drawn cell edges, returning the row under
/// them.
///
/// The crosses mark every cell corner, so an anchor a quarter of a cell in is
/// read against the grid rather than against the run beside it.
fn draw_nudge_grid(frame: &mut Frame<'_>, scene: &mut ApcScene, top: u16) -> u16 {
    const COL_STEPS: [i16; 5] = [0, 4, 8, 12, 16];
    const ROW_STEPS: [i16; 3] = [0, 4, 8];
    let height = ROW_STEPS.len() as u16 * 2;

    label(frame, 2, top, "nudge");
    for column in 0..(COLS - RAMP_X) {
        for row in 0..height {
            let mark = match (column % 4, row % 2) {
                (0, 0) => '+',
                (0, _) => '|',
                (_, 0) => '-',
                _ => ' ',
            };
            frame.buffer_mut()[(RAMP_X + column, top + row)]
                .set_char(mark)
                .set_fg(Color::Rgb(70, 76, 88));
        }
    }

    for (row_index, row_step) in ROW_STEPS.into_iter().enumerate() {
        let area = Rect {
            x: RAMP_X,
            y: top + row_index as u16 * 2,
            width: COLS - RAMP_X,
            height: 2,
        };
        for (col_index, col_step) in COL_STEPS.into_iter().enumerate() {
            frame.render_stateful_widget(
                TextRun {
                    col: col_index as i16 * 13 * CELL + col_step,
                    row: row_step,
                    scale: FIXED_SCALE,
                    color: INK,
                    bg: None,
                    text: "Ag0",
                    follow: 0,
                    anchor: None,
                },
                area,
                scene,
            );
        }
    }

    top + height
}

/// Draw the run whose scale the keys step, with its scale and the keys named
/// beside it.
fn draw_live(frame: &mut Frame<'_>, scene: &mut ApcScene, top: u16, live: u16) {
    label(frame, 2, top, &format!("{live:>3}"));
    label(frame, 2, top + 2, "+/-: scale   q: quit");

    frame.render_stateful_widget(
        TextRun {
            col: 0,
            row: 0,
            scale: live,
            color: INK,
            bg: None,
            text: SAMPLE,
            follow: 0,
            anchor: None,
        },
        Rect {
            x: RAMP_X,
            y: top,
            width: COLS - RAMP_X,
            height: 2,
        },
        scene,
    );
}

/// Write one grid-size cell label, which is what the scaled runs are measured
/// against.
fn label(frame: &mut Frame<'_>, x: u16, y: u16, text: &str) {
    let style = Style::default().fg(Color::Rgb(INK[0], INK[1], INK[2]));
    frame
        .buffer_mut()
        .set_stringn(x, y, text, COLS as usize, style);
}
