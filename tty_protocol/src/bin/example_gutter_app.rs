//! A gutter stoatty demo: an editor scene whose gutter packs a smaller-than-grid
//! line number, thin git and diagnostic color bars, and a hairline separator
//! into a few columns -- all off the cell grid, while the code stays on the
//! uniform grid.
//!
//! The line numbers are fractional, vertically-centered text runs; the bars are
//! sub-cell fills; and one line carries an integer-cell inline expansion (an
//! inline diagnostic) that pushes the lines below it down. The gutter components
//! anchor at their lines' logical rows and bind to the surface's logical-line
//! layout, so the numbers and bars below the expansion track it, and they
//! re-derive from the live cell metrics so they stay centered through font zoom.
//!
//! The scene is static: drawn once and held, since the renderer accumulates
//! text-run and bar declarations rather than replacing them. Run as the PTY shell
//! by the `gutter` example.

use std::{
    io::{self, Write},
    thread,
};
use stoatty_protocol::command::{self, BarCommand, LineLayoutCommand, TextRunCommand};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses. The scene sets them explicitly and clears, so erased
/// cells share a known background the gutter components -- which composite over
/// the cells -- can pass as their own `bg` and blend against under any theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Color of the inline-expansion (diagnostic) text drawn beneath a line
/// (`#828997`), dimmer than the code so it reads as secondary.
const EXPANSION_FG: [u8; 3] = [130, 137, 151];

/// 0-based column the code text starts at. Columns left of it are the gutter the
/// off-grid components draw into, which the editor leaves blank.
const BODY_COL: u16 = 3;

/// Line-number glyph size in 256ths of a cell (160 = 0.625x), so the number is
/// smaller than the body text.
const NUMBER_SCALE: u16 = 160;

/// Line-number color (`#636d83`).
const NUMBER_FG: [u8; 3] = [99, 109, 131];

/// Right edge the line numbers align to, in sixteenths of a cell, so one- and
/// two-digit numbers share a right margin.
const NUMBER_RIGHT_EDGE: i16 = 30;

/// X of the diagnostic-severity bar (the gutter's left edge), in sixteenths.
const DIAG_BAR_X: i16 = 0;

/// X of the git-status bar (just right of the numbers), in sixteenths.
const GIT_BAR_X: i16 = 31;

/// X of the hairline separator (the gutter's right edge), in sixteenths.
const SEPARATOR_X: i16 = 37;

/// Separator color (`#3c424d`), a hair lighter than the background.
const SEPARATOR_COLOR: [u8; 3] = [60, 66, 77];

/// Git status flagged by a gutter bar.
#[derive(Clone, Copy)]
enum Git {
    Added,
    Modified,
}

/// Diagnostic severity flagged by a gutter bar.
#[derive(Clone, Copy)]
enum Diag {
    Error,
    Warning,
}

/// One logical line of the scene: its code, the gutter bars it carries, and any
/// inline-expansion rows drawn beneath it.
struct Line {
    code: &'static str,
    git: Option<Git>,
    diag: Option<Diag>,
    expand: &'static [&'static str],
}

/// The inline diagnostic the flagged line expands into: two extra rows pushed
/// beneath the code, so the line is three rows tall in the layout.
const ERROR_EXPANSION: [&str; 2] = [
    "  ^^^^^^^^^ no method `composite` found for `Frame`",
    "  help: a method with a similar name exists: `compose`",
];

/// The code buffer, one entry per logical line, top to bottom.
const BUFFER: [Line; 14] = [
    Line {
        code: "pub fn render(&self, grid: &Grid) -> Frame {",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "    let mut frame = Frame::new(grid.size());",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "    for row in grid.rows() {",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "        for cell in row.cells() {",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "            frame.paint(cell);",
        git: Some(Git::Modified),
        diag: None,
        expand: &[],
    },
    Line {
        code: "        }",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "    }",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "    frame.composite()",
        git: None,
        diag: Some(Diag::Error),
        expand: &ERROR_EXPANSION,
    },
    Line {
        code: "}",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "fn compose(parts: &[Frame]) -> Frame {",
        git: Some(Git::Added),
        diag: None,
        expand: &[],
    },
    Line {
        code: "    parts.iter().copied().collect()",
        git: Some(Git::Added),
        diag: Some(Diag::Warning),
        expand: &[],
    },
    Line {
        code: "}",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "// TODO: cache composed frames",
        git: None,
        diag: None,
        expand: &[],
    },
];

fn main() {
    let mut out = Vec::new();
    set_palette(&mut out);
    out.extend_from_slice(b"\x1b[2J");

    draw_buffer(&mut out);
    emit_line_layout(&mut out);
    emit_line_numbers(&mut out);
    emit_bars(&mut out);

    // Rest the cursor on the flagged line so the scene reads as mid-edit.
    cup(&mut out, physical_row(7), BODY_COL);

    let mut stdout = io::stdout();
    stdout.write_all(&out).expect("write the scene");
    stdout.flush().expect("flush the scene");

    // Hold so the buffer and gutter stay still and the window keeps the process
    // alive; nothing animates.
    loop {
        thread::park();
    }
}

/// Set the scene's foreground and background, so erased cells and body text share
/// the editor colors the gutter components blend against. The following `\x1b[2J`
/// fills the screen with this background via back-color-erase.
fn set_palette(out: &mut Vec<u8>) {
    out.extend_from_slice(
        format!(
            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
            EDITOR_FG[0], EDITOR_FG[1], EDITOR_FG[2], EDITOR_BG[0], EDITOR_BG[1], EDITOR_BG[2],
        )
        .as_bytes(),
    );
}

/// Write each logical line's code at its physical row, then its inline-expansion
/// rows just beneath it in [`EXPANSION_FG`], all from [`BODY_COL`].
fn draw_buffer(out: &mut Vec<u8>) {
    for (index, line) in BUFFER.iter().enumerate() {
        let row = physical_row(index);
        cup(out, row, BODY_COL);
        out.extend_from_slice(line.code.as_bytes());

        if line.expand.is_empty() {
            continue;
        }

        set_fg(out, EXPANSION_FG);
        for (offset, text) in line.expand.iter().enumerate() {
            cup(out, row + 1 + offset as u16, BODY_COL);
            out.extend_from_slice(text.as_bytes());
        }
        set_fg(out, EDITOR_FG);
    }
}

/// Declare the surface's logical-line layout the gutter components bind to: each
/// line's height in rows, the flagged line taller by its inline rows. The
/// renderer shifts every later line's bound components down by the prefix sum.
fn emit_line_layout(out: &mut Vec<u8>) {
    let heights = BUFFER.iter().map(line_height).collect();
    out.extend_from_slice(&command::encode_line_layout(&LineLayoutCommand { heights }));
}

/// Emit one line-number text run per logical line: a smaller-than-grid,
/// right-aligned run anchored at the line's logical row, so the renderer centers
/// it on the row and shifts it through expansions above.
fn emit_line_numbers(out: &mut Vec<u8>) {
    for index in 0..BUFFER.len() {
        let number = index + 1;
        out.extend_from_slice(&command::encode_text_run(&TextRunCommand {
            col: number_col(number),
            row: logical_row(index),
            scale: NUMBER_SCALE,
            color: NUMBER_FG,
            bg: EDITOR_BG,
            text: number.to_string(),
        }));
    }
}

/// Emit the gutter's sub-cell bars: a diagnostic-severity bar and a git-status
/// bar per flagged line, both anchored at the line's logical row so they track
/// expansions, plus a hairline separator spanning the gutter's full height.
fn emit_bars(out: &mut Vec<u8>) {
    for (index, line) in BUFFER.iter().enumerate() {
        if let Some(diag) = line.diag {
            out.extend_from_slice(&command::encode_bar(&BarCommand {
                x: DIAG_BAR_X,
                y: logical_row(index),
                width: diag_bar_width(diag),
                height: line_height(line) * 16,
                color: diag_color(diag),
            }));
        }

        if let Some(git) = line.git {
            out.extend_from_slice(&command::encode_bar(&BarCommand {
                x: GIT_BAR_X,
                y: logical_row(index),
                width: 3,
                height: 16,
                color: git_color(git),
            }));
        }
    }

    out.extend_from_slice(&command::encode_bar(&BarCommand {
        x: SEPARATOR_X,
        y: 0,
        width: 1,
        height: total_rows() * 16,
        color: SEPARATOR_COLOR,
    }));
}

/// The physical row a logical line starts on: the sum of the heights of the lines
/// above it. Mirrors the renderer's own prefix sum, so the body text the emitter
/// prints lines up with the gutter components the renderer places.
fn physical_row(line: usize) -> u16 {
    BUFFER[..line].iter().map(line_height).sum()
}

/// A logical line's anchor row for a bound component, in sixteenths of a cell.
/// The renderer resolves this through the line layout, so a component anchored
/// here tracks expansions above it.
fn logical_row(line: usize) -> i16 {
    line as i16 * 16
}

/// A line's height in rows: one, plus its inline-expansion rows.
fn line_height(line: &Line) -> u16 {
    1 + line.expand.len() as u16
}

/// Total physical rows the buffer occupies: the sum of every line's height.
fn total_rows() -> u16 {
    BUFFER.iter().map(line_height).sum()
}

/// The right-aligned start column of `number`, in sixteenths of a cell, so every
/// line number shares [`NUMBER_RIGHT_EDGE`] as its right margin. The run advances
/// one scaled cell width per digit, so the start backs off by the digit count.
fn number_col(number: usize) -> i16 {
    let digits = number.to_string().len() as f32;
    let advance = digits * NUMBER_SCALE as f32 / 256.0;
    let right_edge = NUMBER_RIGHT_EDGE as f32 / 16.0;
    ((right_edge - advance) * 16.0).round() as i16
}

/// The severity bar's color: red for an error, yellow for a warning.
fn diag_color(diag: Diag) -> [u8; 3] {
    match diag {
        Diag::Error => [224, 108, 117],
        Diag::Warning => [229, 192, 123],
    }
}

/// The severity bar's width in sixteenths: an error reads wider than a warning.
fn diag_bar_width(diag: Diag) -> u16 {
    match diag {
        Diag::Error => 4,
        Diag::Warning => 3,
    }
}

/// The git bar's color: green for an added line, blue for a modified one.
fn git_color(git: Git) -> [u8; 3] {
    match git {
        Git::Added => [152, 195, 121],
        Git::Modified => [97, 175, 239],
    }
}

/// Set the foreground color for the text that follows.
fn set_fg(out: &mut Vec<u8>, color: [u8; 3]) {
    out.extend_from_slice(format!("\x1b[38;2;{};{};{}m", color[0], color[1], color[2]).as_bytes());
}

/// Emit a Cursor Position escape to the 0-based grid (`row`, `col`).
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}
