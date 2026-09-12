//! An icon stoatty demo. Every kind is drawn at every size, and one kind is
//! drawn at pixel offsets from its anchor cell.
//!
//! [`Icon`] draws three signed-distance kinds, and the diagnostics demo shows
//! one of them at one size. How a silhouette holds its shape as the size grows,
//! and where an offset puts it relative to the cell it was anchored at, are
//! therefore invisible without a grid built for them.
//!
//! The widget's own render lays the degraded severity letter down beside the
//! silhouette, which a rich terminal shows through. This demo composites its
//! own chrome, so it calls `draw_components` when a stoatty answers and
//! `draw_fallback` when none does, and never both.
//!
//! Run as the PTY shell by the `icon` example.

use ratatui::{
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Style},
    Frame, Terminal,
};
use std::{io, thread};
use stoat_widgets::{icon::Icon, ApcScene, ApcSession, SessionOptions};
use stoatty_protocol::command::IconKind;

/// The window the `icon` example opens, in cells.
const COLS: u16 = 60;

/// The kinds the grid rows step through, with the severity color and the label
/// each takes.
const KINDS: [(IconKind, [u8; 3], &str); 3] = [
    (IconKind::Error, [224, 108, 117], "error"),
    (IconKind::Warning, [229, 192, 123], "warn"),
    (IconKind::Info, [97, 175, 239], "info"),
];

/// The sizes the grid columns step through.
const SIZES: [u8; 4] = [1, 2, 3, 4];

/// Column the grid starts at, leaving room for the row labels.
const GRID_X: u16 = 8;

/// Cells between grid columns, wide enough that a size-4 icon clears the next.
const COL_STRIDE: u16 = 6;

/// Cells between grid rows, tall enough for the same reason.
const ROW_STRIDE: u16 = 5;

/// The offsets the last row draws the same icon at, in pixels from the anchor.
const OFFSETS: [[i16; 2]; 3] = [[0, 0], [6, 0], [0, 6]];

/// Label color.
const LABEL: [u8; 3] = [171, 178, 191];

/// The color the offset row's cell grid is drawn in.
const GRID_INK: [u8; 3] = [70, 76, 88];

fn main() {
    // The session holds the hidden cursor for as long as it lives, and gives it
    // back on the way out of main -- including the way out an `expect` below
    // takes, which its panic hook covers.
    let mut session = ApcSession::new(SessionOptions {
        hide_cursor: true,
        ..SessionOptions::default()
    });
    let live = session.live();

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");

    terminal.clear().expect("clear the screen");
    terminal
        .draw(|frame| draw_scene(frame, session.scene(), live))
        .expect("draw the scene");
    session.flush().expect("write the decoration");

    // Hold so the scene stays still and the window keeps the process alive;
    // nothing animates. Closing the window ends the process outright, so the
    // session's own restore never runs and its panic hook is what covers a
    // crash.
    loop {
        thread::park();
    }
}

/// Draw the size grid, the offset row, and the caption.
fn draw_scene(frame: &mut Frame<'_>, scene: &mut ApcScene, live: bool) {
    let offsets_top = draw_size_grid(frame, scene, live) + 1;
    let caption_top = draw_offset_row(frame, scene, live, offsets_top) + 1;

    label(
        frame,
        2,
        caption_top,
        "each kind across sizes 1 to 4, then one kind at pixel offsets",
    );
}

/// Draw one row per kind and one column per size, returning the row under the
/// grid.
///
/// A column carries its size above it and a row its kind beside it, so a
/// silhouette that loses its shape at one size is read against the same kind at
/// the others.
fn draw_size_grid(frame: &mut Frame<'_>, scene: &mut ApcScene, live: bool) -> u16 {
    let top = 1;

    for (column, size) in SIZES.into_iter().enumerate() {
        label(
            frame,
            GRID_X + column as u16 * COL_STRIDE,
            top,
            &format!("{size}x"),
        );
    }

    for (row, (kind, color, name)) in KINDS.into_iter().enumerate() {
        let y = top + 1 + row as u16 * ROW_STRIDE;
        label(frame, 2, y, name);

        for (column, size) in SIZES.into_iter().enumerate() {
            draw_icon(
                frame,
                scene,
                live,
                Icon {
                    kind,
                    color,
                    size,
                    offset: [0, 0],
                },
                GRID_X + column as u16 * COL_STRIDE,
                y,
            );
        }
    }

    top + KINDS.len() as u16 * ROW_STRIDE
}

/// Draw one kind three times at different pixel offsets over drawn cell edges,
/// returning the row under them.
///
/// An offset moves the icon off the cell it was anchored at, which is how a
/// popover lines an icon up with inset content. The crosses mark the cell
/// corners, so the shift is measured against the grid rather than against the
/// icon beside it.
fn draw_offset_row(frame: &mut Frame<'_>, scene: &mut ApcScene, live: bool, top: u16) -> u16 {
    const HEIGHT: u16 = 3;
    label(frame, 2, top + 1, "offset");

    for column in 0..(COLS - GRID_X) {
        for row in 0..HEIGHT {
            let mark = match (column % 3, row % 2) {
                (0, 0) => '+',
                (0, _) => '|',
                (_, 0) => '-',
                _ => ' ',
            };
            frame.buffer_mut()[(GRID_X + column, top + row)]
                .set_char(mark)
                .set_fg(Color::Rgb(GRID_INK[0], GRID_INK[1], GRID_INK[2]));
        }
    }

    let (kind, color, _) = KINDS[1];
    for (index, offset) in OFFSETS.into_iter().enumerate() {
        draw_icon(
            frame,
            scene,
            live,
            Icon {
                kind,
                color,
                size: 2,
                offset,
            },
            GRID_X + index as u16 * COL_STRIDE * 2,
            top + 1,
        );
    }

    top + HEIGHT
}

/// Draw one icon at a cell, as a silhouette on a stoatty and as its severity
/// letter anywhere else.
///
/// The two never draw together. The widget's own render writes the letter as
/// well, which shows through the silhouette on a terminal that draws one.
fn draw_icon(frame: &mut Frame<'_>, scene: &mut ApcScene, live: bool, icon: Icon, x: u16, y: u16) {
    let area = Rect {
        x,
        y,
        width: 1,
        height: 1,
    };
    match live {
        true => icon.draw_components(area, scene),
        false => icon.draw_fallback(area, frame.buffer_mut()),
    }
}

/// Write one cell-grid label.
fn label(frame: &mut Frame<'_>, x: u16, y: u16, text: &str) {
    let style = Style::default().fg(Color::Rgb(LABEL[0], LABEL[1], LABEL[2]));
    frame
        .buffer_mut()
        .set_stringn(x, y, text, COLS as usize, style);
}
