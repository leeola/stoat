//! A baseline-TUI stoatty demo program: draw a framed panel, then hold.
//!
//! Emits pure VT (cursor positioning, SGR styling, and box-drawing glyphs) and
//! no stoatty APC codes, so it renders the same in any terminal. Run as the PTY
//! shell by the `sink` example, it exercises the bytes-to-render path on a
//! richer screen than the `hello` example.

use std::{
    io::{self, Write},
    thread,
    time::Duration,
};
use stoatty_protocol::command;

// Box-drawing glyphs, written as escapes so the source stays ASCII.
const TOP_LEFT: &str = "\u{250c}";
const TOP_RIGHT: &str = "\u{2510}";
const BOTTOM_LEFT: &str = "\u{2514}";
const BOTTOM_RIGHT: &str = "\u{2518}";
const HORIZONTAL: &str = "\u{2500}";
const VERTICAL: &str = "\u{2502}";

/// Visible columns between the panel's side borders.
const INNER: usize = 30;

fn main() {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[2J");

    render_panel(&mut out, 2, 4);
    render_underlines(&mut out, 8, 4);
    render_border(&mut out);

    // Leave the cursor below the demo in the default style.
    cup(&mut out, 10, 1);
    out.extend_from_slice(b"\x1b[0m");

    let mut stdout = io::stdout();
    stdout.write_all(&out).expect("write to stdout");
    stdout.flush().expect("flush stdout");

    move_cursor_forever(&mut stdout);
}

/// Walk the cursor around the panel with plain CUP, pausing between moves so the
/// renderer's easing animation is visible. Never returns, holding the shell open
/// until the window closes and kills this process.
fn move_cursor_forever(stdout: &mut io::Stdout) {
    let path = [(3u16, 6u16), (3, 30), (9, 30), (9, 6)];

    for (row, col) in path.iter().cycle() {
        let mut step = Vec::new();
        cup(&mut step, *row, *col);
        stdout.write_all(&step).expect("write cursor move");
        stdout.flush().expect("flush cursor move");

        thread::sleep(Duration::from_millis(1200));
    }
}

/// Draw a bordered panel of SGR-styled lines with its top-left at (`top`, `left`).
///
/// Each line carries its visible length so the styling escapes, which take no
/// columns, do not push the right border out of alignment.
fn render_panel(out: &mut Vec<u8>, top: u16, left: u16) {
    let lines: [(&[u8], usize); 3] = [
        (b" \x1b[1mstoatty sink demo\x1b[0m", 18),
        (
            b" \x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m",
            22,
        ),
        (
            b" \x1b[31mred\x1b[0m  \x1b[32mgreen\x1b[0m  \x1b[44mon blue\x1b[0m",
            20,
        ),
    ];

    cup(out, top, left);
    border(out, TOP_LEFT, TOP_RIGHT);

    for (row, (content, visible)) in lines.iter().enumerate() {
        cup(out, top + 1 + row as u16, left);
        out.extend_from_slice(VERTICAL.as_bytes());
        out.extend_from_slice(content);
        for _ in 0..INNER.saturating_sub(*visible) {
            out.push(b' ');
        }
        out.extend_from_slice(VERTICAL.as_bytes());
    }

    cup(out, top + 1 + lines.len() as u16, left);
    border(out, BOTTOM_LEFT, BOTTOM_RIGHT);
}

/// Draw one labeled word per underline style at (`top`, `left`).
///
/// All five share a cyan underline color set with SGR 58, then each word selects
/// its style with SGR `4:1`-`4:5` (straight, double, curly, dotted, dashed).
fn render_underlines(out: &mut Vec<u8>, top: u16, left: u16) {
    cup(out, top, left);
    out.extend_from_slice(b"\x1b[58:2::0:200:255m");
    out.extend_from_slice(b"\x1b[4:1mstraight \x1b[4:2mdouble \x1b[4:3mcurly ");
    out.extend_from_slice(b"\x1b[4:4mdotted \x1b[4:5mdashed\x1b[0m");
}

/// Frame a region beside the panel with a renderer-native heavy magenta border
/// via the Gstoatty;border APC frame.
///
/// The region is in absolute 0-based grid coordinates; another terminal
/// consumes the APC string and ignores it.
fn render_border(out: &mut Vec<u8>) {
    out.extend_from_slice(&command::encode_border(&command::BorderCommand {
        top: 1,
        left: 40,
        width: 24,
        height: 6,
        style: command::BorderStyle::Heavy,
        color: [255, 0, 255],
    }));
}

/// Write a horizontal border row spanning [`INNER`] between two corner glyphs.
fn border(out: &mut Vec<u8>, left: &str, right: &str) {
    out.extend_from_slice(left.as_bytes());
    for _ in 0..INNER {
        out.extend_from_slice(HORIZONTAL.as_bytes());
    }
    out.extend_from_slice(right.as_bytes());
}

/// Emit a Cursor Position escape moving to the 1-based (`row`, `col`).
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
}
