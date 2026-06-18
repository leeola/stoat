//! A hover-doc stoatty demo: a code buffer with a documentation tooltip anchored
//! under the word beneath the cursor.
//!
//! The tooltip is a rounded popover anchored sub-cell to the word, its content
//! drawn at a larger-than-grid font and overflowing the box so the renderer
//! auto-scrolls it -- the Zed/VS Code hover-on-symbol look. The scene is static:
//! the buffer is drawn once and held, so the screen-anchored tooltip keeps
//! tracking the word. Run as the PTY shell by the `doc_tooltip` example.

use std::{
    io::{self, Write},
    thread,
};
use stoatty_protocol::command::{self, PopoverCommand};

/// The static code buffer, drawn from the top-left.
const BUFFER: [&str; 5] = [
    "fn main() {",
    "    let grid = Grid::new(80, 24);",
    "    let frame = render(&grid);",
    "    present(frame);",
    "}",
];

/// 0-based grid row and column of the `render` call the tooltip documents, and
/// the cursor's resting cell.
const WORD_ROW: u16 = 2;
const WORD_COL: u16 = 16;

fn main() {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[2J");
    draw_buffer(&mut out);
    emit_tooltip(&mut out);

    // Rest the cursor on the documented word so the scene reads as a hover.
    cup(&mut out, WORD_ROW, WORD_COL);

    let mut stdout = io::stdout();
    stdout.write_all(&out).expect("write the scene");
    stdout.flush().expect("flush the scene");

    // Hold so the buffer stays still and the window keeps the process alive; the
    // app auto-scrolls the overflowing tooltip content on its own.
    loop {
        thread::park();
    }
}

/// Write each buffer line from the top-left, once.
fn draw_buffer(out: &mut Vec<u8>) {
    for (row, line) in BUFFER.iter().enumerate() {
        cup(out, row as u16, 0);
        out.extend_from_slice(line.as_bytes());
    }
}

/// Emit the documentation popover anchored just under the `render` word.
///
/// The box is anchored a row below the word and nudged by a sub-cell pixel
/// offset so it sits snug under the call. `scale` draws the content larger than
/// the grid, and the content runs longer than the box so the renderer
/// auto-scrolls it.
fn emit_tooltip(out: &mut Vec<u8>) {
    let content = [
        "render(grid: &Grid) -> Frame",
        "",
        "Draw the grid into a new",
        "frame, compositing each",
        "cell's glyph over its",
        "background in linear light.",
        "",
        "Applies cursor and",
        "selection styles, easing",
        "scroll for smooth cursor",
        "motion.",
        "",
        "Returns a Frame ready to",
        "present to the surface.",
    ]
    .join("\n");

    out.extend_from_slice(&command::encode_popover(&PopoverCommand {
        top: WORD_ROW + 1,
        left: WORD_COL - 2,
        width: 56,
        height: 12,
        fill: [22, 24, 34],
        border: [120, 170, 255],
        content_fg: [228, 232, 240],
        scale: 2,
        offset: [6, 4],
        content,
    }));
}

/// Emit a Cursor Position escape to the 0-based grid (`row`, `col`).
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}
