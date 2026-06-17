//! A split-pane stoatty demo: a fixed sidebar beside a tall buffer whose
//! viewport chases a cursor via the per-region scroll command.
//!
//! The left pane is static; the right pane shows a window onto a longer buffer.
//! A fixed sequence of cursor jumps moves the window -- big upward jumps shoot it
//! up, small downward steps nudge it back -- and each jump reports the window's
//! absolute scroll position via a `Gstoatty;scroll_region` frame, so the
//! renderer eases the change while the left pane stays put. Run as the PTY shell
//! by the `split_scroll` example.

use std::{
    io::{self, Write},
    thread,
    time::Duration,
};
use stoatty_protocol::command::{self, BorderCommand, BorderStyle, ScrollRegionCommand};

/// Left (fixed) pane border, in 0-based grid coordinates.
const LEFT: Rect = Rect {
    top: 1,
    left: 2,
    width: 20,
    height: 14,
};

/// Right (scrolling) pane border, in 0-based grid coordinates.
const RIGHT: Rect = Rect {
    top: 1,
    left: 26,
    width: 32,
    height: 14,
};

/// Rows of buffer the right pane shows at once: its border's interior height.
const VIEWPORT_H: usize = RIGHT.height as usize - 2;

/// Lines in the scrolled buffer.
const BUFFER_LINES: usize = 40;

/// Greatest first-visible line, so the last window ends at the buffer's end.
const MAX_SCROLL: usize = BUFFER_LINES - VIEWPORT_H;

/// Rows the cursor sits below the window top, so the window keeps it in view.
const CURSOR_MARGIN: usize = 6;

/// `(cursor line, settle-ms)` per jump, looped forever. Fixed so the demo
/// repeats identically without an RNG; big upward jumps and small downward steps
/// drive large and small eased scrolls in turn.
const CURSOR_JUMPS: [(usize, u64); 10] = [
    (6, 450),
    (8, 350),
    (34, 650),
    (30, 350),
    (26, 350),
    (6, 650),
    (10, 350),
    (14, 350),
    (18, 350),
    (38, 650),
];

/// Fixed left-pane content: a static file-tree sidebar.
const SIDEBAR: [&str; 7] = [
    "src/",
    "  main.rs",
    "  app.rs",
    "  render.rs",
    "tests/",
    "  smoke.rs",
    "Cargo.toml",
];

fn main() {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[2J");
    draw_borders(&mut out);
    draw_sidebar(&mut out);

    let mut stdout = io::stdout();
    stdout.write_all(&out).expect("write initial frame");
    stdout.flush().expect("flush initial frame");

    loop {
        for (cursor_line, settle_ms) in CURSOR_JUMPS {
            let mut frame = Vec::new();
            draw_viewport(&mut frame, cursor_line);
            stdout.write_all(&frame).expect("write viewport frame");
            stdout.flush().expect("flush viewport frame");

            thread::sleep(Duration::from_millis(settle_ms));
        }
    }
}

/// A grid region in 0-based cell coordinates.
#[derive(Clone, Copy)]
struct Rect {
    top: u16,
    left: u16,
    width: u16,
    height: u16,
}

/// Frame both panes with renderer-native borders via `Gstoatty;border` frames.
fn draw_borders(out: &mut Vec<u8>) {
    for rect in [LEFT, RIGHT] {
        out.extend_from_slice(&command::encode_border(&BorderCommand {
            top: rect.top,
            left: rect.left,
            width: rect.width,
            height: rect.height,
            style: BorderStyle::Light,
            color: [120, 130, 150],
        }));
    }
}

/// Write the fixed sidebar lines inside the left pane, once.
fn draw_sidebar(out: &mut Vec<u8>) {
    for (row, line) in SIDEBAR.iter().enumerate() {
        cup(out, LEFT.top + 1 + row as u16, LEFT.left + 1);
        out.extend_from_slice(line.as_bytes());
    }
}

/// Redraw the right pane's window for `cursor_line`, place the cursor on it, and
/// report the window's scroll position.
///
/// The window keeps the cursor [`CURSOR_MARGIN`] rows from its top where it can,
/// so a cursor jump moves the window with it. Every interior cell is rewritten
/// (padded to the interior width) so the prior window is fully overwritten; the
/// renderer eases the reported scroll change and clips the glide to the pane.
fn draw_viewport(out: &mut Vec<u8>, cursor_line: usize) {
    let scroll = cursor_line.saturating_sub(CURSOR_MARGIN).min(MAX_SCROLL);
    let inner_top = RIGHT.top + 1;
    let inner_left = RIGHT.left + 1;
    let inner_width = RIGHT.width as usize - 2;

    for row in 0..VIEWPORT_H {
        let line: String = buffer_line(scroll + row)
            .chars()
            .chain(std::iter::repeat(' '))
            .take(inner_width)
            .collect();
        cup(out, inner_top + row as u16, inner_left);
        out.extend_from_slice(line.as_bytes());
    }

    out.extend_from_slice(&command::encode_scroll_region(&ScrollRegionCommand {
        top: inner_top,
        left: inner_left,
        width: inner_width as u16,
        height: VIEWPORT_H as u16,
        offset: scroll as u16,
    }));

    cup(out, inner_top + (cursor_line - scroll) as u16, inner_left);
}

/// The text of buffer line `index`: a line number and a cycling code fragment,
/// so the scroll is legible as the numbers and content move.
fn buffer_line(index: usize) -> String {
    const SNIPPETS: [&str; 8] = [
        "fn render(&self) {",
        "    let mut out = Vec::new();",
        "    for cell in self.cells() {",
        "        out.push(cell.glyph);",
        "    }",
        "    out",
        "}",
        "",
    ];

    format!("{:>3}  {}", index + 1, SNIPPETS[index % SNIPPETS.len()])
}

/// Emit a Cursor Position escape to the 0-based grid (`row`, `col`).
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}
