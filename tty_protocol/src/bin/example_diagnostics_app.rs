//! A diagnostics stoatty demo: an editor scene where an error span and a warning
//! span carry severity-colored curly underlines, and a rounded tooltip hovers
//! just below the error span -- the VS Code hover-on-error look.
//!
//! The underlines are plain VT (undercurl plus an underline color). The tooltip
//! is a popover with a severity-colored rounded border anchored at a sub-cell
//! pixel offset under the span's start, holding a severity icon composited above
//! its fill and a severity-colored message. The scene is static: drawn once and
//! held, so the screen-anchored tooltip keeps sitting under the span. Run as the
//! PTY shell by the `diagnostics` example.

use std::{
    io::{self, Write},
    thread,
};
use stoatty_protocol::command::{self, IconCommand, IconKind, PopoverCommand};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses. The scene sets them explicitly and clears, so the
/// look is the same regardless of the active theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Error severity color (`#e06c75`): the error squiggle, the tooltip border and
/// icon, and the message text.
const ERROR: [u8; 3] = [224, 108, 117];

/// Warning severity color (`#e5c07b`): the warning squiggle.
const WARNING: [u8; 3] = [229, 192, 123];

/// Tooltip fill (`#21252b`), darker than the editor so the hover reads as
/// floating above the code.
const TOOLTIP_FILL: [u8; 3] = [33, 37, 43];

/// The code buffer, drawn from the top-left.
const BUFFER: [&str; 5] = [
    "fn parse(input: &str) -> Config {",
    "    let raw = input.trim();",
    "    let parsed = raw.to_uppercase();",
    "    Config::from_str(raw).unwrap()",
    "}",
];

/// The warning span: `parsed` is assigned but never used.
const WARNING_ROW: u16 = 2;
const WARNING_COL: u16 = 8;
const WARNING_TEXT: &str = "parsed";

/// The error span: `from_str` is not a method on `Config`.
const ERROR_ROW: u16 = 3;
const ERROR_COL: u16 = 12;
const ERROR_TEXT: &str = "from_str";

fn main() {
    let mut out = Vec::new();
    set_palette(&mut out);
    out.extend_from_slice(b"\x1b[2J");

    draw_buffer(&mut out);
    underline_span(&mut out, WARNING_ROW, WARNING_COL, WARNING_TEXT, WARNING);
    underline_span(&mut out, ERROR_ROW, ERROR_COL, ERROR_TEXT, ERROR);
    emit_tooltip(&mut out);

    // Rest the cursor on the error span so the scene reads as a hover-on-error.
    cup(&mut out, ERROR_ROW, ERROR_COL);

    let mut stdout = io::stdout();
    stdout.write_all(&out).expect("write the scene");
    stdout.flush().expect("flush the scene");

    // Hold so the buffer stays still and the window keeps the process alive;
    // nothing animates.
    loop {
        thread::park();
    }
}

/// Set the scene's foreground and background, so erased cells and body text share
/// the editor colors. The following `\x1b[2J` fills the screen with this
/// background via back-color-erase.
fn set_palette(out: &mut Vec<u8>) {
    out.extend_from_slice(
        format!(
            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
            EDITOR_FG[0], EDITOR_FG[1], EDITOR_FG[2], EDITOR_BG[0], EDITOR_BG[1], EDITOR_BG[2],
        )
        .as_bytes(),
    );
}

/// Write each buffer line from the top-left, once.
fn draw_buffer(out: &mut Vec<u8>) {
    for (row, line) in BUFFER.iter().enumerate() {
        cup(out, row as u16, 0);
        out.extend_from_slice(line.as_bytes());
    }
}

/// Re-stamp a span with a curly underline in `color`, leaving its glyphs and the
/// editor foreground unchanged.
///
/// `\x1b[4:3m` selects the undercurl and `\x1b[58:2::r:g:bm` its color; rewriting
/// the same characters adds the decoration without disturbing the surrounding
/// line. The trailing `\x1b[24;59m` clears the underline and restores the default
/// underline color so it does not bleed onto later writes.
fn underline_span(out: &mut Vec<u8>, row: u16, col: u16, text: &str, color: [u8; 3]) {
    cup(out, row, col);
    out.extend_from_slice(
        format!("\x1b[4:3;58:2::{}:{}:{}m", color[0], color[1], color[2]).as_bytes(),
    );
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[24;59m");
}

/// Emit the error tooltip just below the error span: a rounded popover with a
/// severity-colored border anchored at a sub-cell offset under the span's start,
/// the severity message inside, and an error icon composited over its top-left.
///
/// The content is indented two cells so the icon's cell stays clear of the text;
/// the icon draws after the overlay, so it sits above the fill.
fn emit_tooltip(out: &mut Vec<u8>) {
    let content = [
        "  no method `from_str` found",
        "  help: did you mean `parse`?",
    ]
    .join("\n");

    out.extend_from_slice(&command::encode_popover(&PopoverCommand {
        top: ERROR_ROW + 1,
        left: ERROR_COL,
        width: 34,
        height: 4,
        fill: TOOLTIP_FILL,
        border: ERROR,
        content_fg: ERROR,
        scale: 1,
        offset: [3, 6],
        content,
    }));

    out.extend_from_slice(&command::encode_icon(&IconCommand {
        top: ERROR_ROW + 1,
        left: ERROR_COL,
        kind: IconKind::Error,
        color: ERROR,
        size: 1,
    }));
}

/// Emit a Cursor Position escape to the 0-based grid (`row`, `col`).
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}
