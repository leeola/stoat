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

/// Lines the slow crawl emits each cycle before the darting bursts run.
const SLOW_LINES: u64 = 8;

/// Milliseconds between slow-crawl lines, the gentle steady cadence.
const SLOW_INTERVAL_MS: u64 = 400;

/// `(lines, pause-after-ms)` for each darting burst. Uneven sizes and gaps so
/// the eased scroll jumps erratically instead of gliding, and fixed so the demo
/// repeats identically without an RNG.
const BURSTS: [(u64, u64); 8] = [
    (7, 70),
    (3, 220),
    (12, 45),
    (2, 260),
    (9, 55),
    (4, 180),
    (11, 40),
    (5, 130),
];

fn main() {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[2J");

    render_panel(&mut out, 2, 4);
    render_underlines(&mut out, 8, 4);
    render_border(&mut out);
    render_rounded_border(&mut out);
    render_scaled_heading(&mut out);
    render_mixed_size_text(&mut out);
    render_popover(&mut out);

    // Leave the cursor below the demo in the default style.
    cup(&mut out, 10, 1);
    out.extend_from_slice(b"\x1b[0m");

    let mut stdout = io::stdout();
    stdout.write_all(&out).expect("write to stdout");
    stdout.flush().expect("flush stdout");

    scroll_text_forever(&mut stdout);
}

/// Drive two contrasting scroll patterns forever so the renderer's eased grid
/// scroll is shown both ways: a slow steady crawl that glides, then fast uneven
/// bursts that dart. Never returns, holding the shell open until the window
/// closes and kills this process.
fn scroll_text_forever(stdout: &mut io::Stdout) {
    let mut line = 0u64;
    loop {
        slow_crawl(stdout, &mut line);
        darting_bursts(stdout, &mut line);
    }
}

/// Emit [`SLOW_LINES`] single lines at [`SLOW_INTERVAL_MS`], each flushed on its
/// own so the renderer eases every one-row scroll into a gentle glide.
fn slow_crawl(stdout: &mut io::Stdout, line: &mut u64) {
    for _ in 0..SLOW_LINES {
        *line += 1;

        let mut step = Vec::new();
        push_line(&mut step, &format!("scrolling line {line}"));
        stdout.write_all(&step).expect("write scroll line");
        stdout.flush().expect("flush scroll line");

        thread::sleep(Duration::from_millis(SLOW_INTERVAL_MS));
    }
}

/// Emit each [`BURSTS`] entry as one multi-line write, so the whole burst lands
/// in a single frame and the renderer seeds a large scroll offset that darts up.
///
/// The uneven burst sizes and pauses make the motion read as erratic, in
/// contrast to [`slow_crawl`]'s steady glide.
fn darting_bursts(stdout: &mut io::Stdout, line: &mut u64) {
    for (count, pause_ms) in BURSTS {
        let mut burst = Vec::new();
        for _ in 0..count {
            *line += 1;
            push_line(&mut burst, &format!("darting line {line}"));
        }
        stdout.write_all(&burst).expect("write darting burst");
        stdout.flush().expect("flush darting burst");

        thread::sleep(Duration::from_millis(pause_ms));
    }
}

/// Append `text` followed by CRLF to `out`.
fn push_line(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\r\n");
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

/// Frame a second region below the heavy one with a Rounded border, so the
/// renderer draws arced corners next to the square ones.
fn render_rounded_border(out: &mut Vec<u8>) {
    out.extend_from_slice(&command::encode_border(&command::BorderCommand {
        top: 8,
        left: 40,
        width: 24,
        height: 4,
        style: command::BorderStyle::Rounded,
        color: [0, 255, 255],
    }));
}

/// Write a short word and scale each letter to 2x with Gstoatty;scale frames, so
/// the renderer draws a double-size heading beside the normal-size text.
///
/// The letters sit two grid columns apart because each 2x glyph owns a 2x2
/// block; the columns between are left blank for the blocks to cover.
fn render_scaled_heading(out: &mut Vec<u8>) {
    let row = 13u16;
    let letters = [(b'B', 4u16), (b'I', 6), (b'G', 8)];

    for (glyph, col) in letters {
        cup(out, row + 1, col + 1);
        out.push(glyph);
    }

    for (_, col) in letters {
        out.extend_from_slice(&command::encode_scale(&command::ScaleCommand {
            top: row,
            left: col,
            scale: 2,
        }));
    }
}

/// Write a line of normal-size text wrapping a 2x inner run via Gstoatty;scale
/// frames, so one region shows normal text around a larger phrase.
///
/// The inner letters sit two columns apart because each 2x glyph owns a 2x2
/// block; the surrounding words share the block's top row.
fn render_mixed_size_text(out: &mut Vec<u8>) {
    let row = 16u16;
    let prefix = b"a ";
    let inner = [b'H', b'U', b'G', b'E'];
    let suffix = b" word";

    let inner_left = prefix.len() as u16;
    let suffix_left = inner_left + inner.len() as u16 * 2;

    cup(out, row + 1, 1);
    out.extend_from_slice(prefix);

    for (i, glyph) in inner.iter().enumerate() {
        let col = inner_left + i as u16 * 2;
        cup(out, row + 1, col + 1);
        out.push(*glyph);
    }

    for i in 0..inner.len() as u16 {
        out.extend_from_slice(&command::encode_scale(&command::ScaleCommand {
            top: row,
            left: inner_left + i * 2,
            scale: 2,
        }));
    }

    cup(out, row + 1, suffix_left + 1);
    out.extend_from_slice(suffix);
}

/// Reveal a floating popover over the panel via a Gstoatty;popover frame,
/// simulating a hover. It composites above the cells with its own z-order,
/// occluding whatever it covers.
fn render_popover(out: &mut Vec<u8>) {
    out.extend_from_slice(&command::encode_popover(&command::PopoverCommand {
        top: 3,
        left: 12,
        width: 16,
        height: 4,
        fill: [20, 22, 34],
        border: [120, 170, 255],
        content_fg: [236, 239, 245],
        scale: 1,
        content: [
            "render_overlay",
            "render_border",
            "render_shadow",
            "render_text",
            "render_grid",
            "render_scale",
        ]
        .join("\n"),
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
