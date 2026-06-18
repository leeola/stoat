//! A gutter stoatty demo: three editor panes tiled side by side, each drawing
//! the same code with its own gutter at a different size -- wide color bars,
//! compressed, then padded -- so the gutter geometry reads as just sub-cell
//! coordinates the emitter places.
//!
//! Each pane is framed by a border; its gutter packs smaller-than-grid line
//! numbers (fractional text runs), thin git and diagnostic color bars (sub-cell
//! fills), and a hairline separator into a few columns, all off the cell grid
//! while the code stays on the uniform grid. One line carries an integer-cell
//! inline expansion (an inline diagnostic) that pushes the lines below it down.
//!
//! A single per-surface line layout cannot bind independent side-by-side panes
//! (that is deferred multi-surface work), so the emitter positions every gutter
//! component at an absolute physical row offset to its pane origin and folds the
//! expansion shift in itself rather than declaring a line layout for the
//! renderer to resolve against. The components still ride in sixteenths of a
//! cell, so they track live font zoom. The scene is static: drawn once and held.
//! Run as the PTY shell by the `gutter` example.

use std::{
    io::{self, Write},
    thread,
};
use stoatty_protocol::command::{self, BarCommand, BorderCommand, BorderStyle, TextRunCommand};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so erased cells share a known
/// background the gutter components composite over.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Pane border color (`#4e5666`).
const BORDER_COLOR: [u8; 3] = [78, 86, 102];

/// Line-number glyph size in 256ths of a cell (160 = 0.625x), so the number is
/// smaller than the body text.
const NUMBER_SCALE: u16 = 160;

/// Inline-expansion glyph size in 256ths of a cell (200 = 0.78x), so the inline
/// diagnostic reads smaller than the full-cell body code.
const EXPANSION_SCALE: u16 = 200;

/// Line-number color (`#636d83`).
const NUMBER_FG: [u8; 3] = [99, 109, 131];

/// Sixteenths of a cell a two-digit line number occupies at [`NUMBER_SCALE`], so
/// the gutter reserves room for the widest number the buffer reaches.
const NUMBER_AREA: i16 = 20;

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

/// The inline diagnostic the flagged line expands into: one extra row pushed
/// beneath the code, so the line is two rows tall in the layout.
const EXPANSION: [&str; 1] = ["  ^^ no method"];

/// The shared code buffer, one entry per logical line, drawn in every pane.
const BUFFER: [Line; 10] = [
    Line {
        code: "fn render(g) {",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "  let f =",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "    Frame::new();",
        git: Some(Git::Modified),
        diag: None,
        expand: &[],
    },
    Line {
        code: "  for r in g {",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "    f.draw(r);",
        git: Some(Git::Added),
        diag: None,
        expand: &[],
    },
    Line {
        code: "  }",
        git: None,
        diag: None,
        expand: &[],
    },
    Line {
        code: "  f.compose()",
        git: None,
        diag: Some(Diag::Error),
        expand: &EXPANSION,
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
        code: "// cache it",
        git: None,
        diag: Some(Diag::Warning),
        expand: &[],
    },
];

/// A gutter's sub-cell geometry, in sixteenths of a cell, so each pane can draw
/// its gutter at a different size.
///
/// `bar_width` and `pad` are the knobs; the rest of the layout (the git bar and
/// number positions, the separator, and the body offset) derives from them, so
/// the gutter stays tight as the knobs change.
#[derive(Clone, Copy)]
struct GutterConfig {
    /// Color-bar width.
    bar_width: u16,
    /// Inter-element padding.
    pad: u16,
}

impl GutterConfig {
    /// The git bar's left edge: past the diagnostic bar (at the gutter's left
    /// edge) and one padding.
    fn git_x(self) -> i16 {
        (self.bar_width + self.pad) as i16
    }

    /// The line numbers' right edge: past both bars and a padding, with room for
    /// the widest number.
    fn number_right_edge(self) -> i16 {
        2 * self.bar_width as i16 + 2 * self.pad as i16 + NUMBER_AREA
    }

    /// The hairline separator's x: a padding right of the numbers.
    fn separator_x(self) -> i16 {
        self.number_right_edge() + self.pad as i16
    }

    /// The gutter's width in whole cells, where the body starts: the separator
    /// rounded up to the next cell.
    fn body_col(self) -> u16 {
        (self.separator_x() as u16 + 1).div_ceil(16)
    }
}

/// One tiled pane: a bordered rectangle drawing [`BUFFER`] with `gutter`'s sizing.
struct Pane {
    top: u16,
    left: u16,
    width: u16,
    height: u16,
    gutter: GutterConfig,
}

/// The three panes, left to right: wide color bars, compressed, then padded.
const PANES: [Pane; 3] = [
    Pane {
        top: 1,
        left: 1,
        width: 22,
        height: 14,
        gutter: GutterConfig {
            bar_width: 5,
            pad: 2,
        },
    },
    Pane {
        top: 1,
        left: 24,
        width: 22,
        height: 14,
        gutter: GutterConfig {
            bar_width: 2,
            pad: 1,
        },
    },
    Pane {
        top: 1,
        left: 47,
        width: 22,
        height: 14,
        gutter: GutterConfig {
            bar_width: 3,
            pad: 5,
        },
    },
];

/// The index of the flagged line that carries the inline expansion.
const FLAGGED_LINE: usize = 6;

fn main() {
    let mut out = Vec::new();
    set_palette(&mut out);
    out.extend_from_slice(b"\x1b[2J");

    for pane in &PANES {
        draw_pane(&mut out, pane);
    }

    // Rest the cursor on the first pane's flagged line so the scene reads as
    // mid-edit.
    let first = &PANES[0];
    cup(
        &mut out,
        body_row(first, FLAGGED_LINE),
        first.left + 1 + first.gutter.body_col(),
    );

    let mut stdout = io::stdout();
    stdout.write_all(&out).expect("write the scene");
    stdout.flush().expect("flush the scene");

    // Hold so the panes stay still and the window keeps the process alive.
    loop {
        thread::park();
    }
}

/// Draw a pane: its border, its code body, then its gutter.
fn draw_pane(out: &mut Vec<u8>, pane: &Pane) {
    out.extend_from_slice(&command::encode_border(&BorderCommand {
        top: pane.top,
        left: pane.left,
        width: pane.width,
        height: pane.height,
        style: BorderStyle::Rounded,
        color: BORDER_COLOR,
    }));

    draw_body(out, pane);
    draw_gutter(out, pane);
}

/// Write each line's code at its physical row inside the pane, then any
/// inline-expansion rows just beneath it as smaller, error-colored text runs, so
/// the inline diagnostic reads at a different size and color from the code.
fn draw_body(out: &mut Vec<u8>, pane: &Pane) {
    let body_col = pane.left + 1 + pane.gutter.body_col();

    for (index, line) in BUFFER.iter().enumerate() {
        let row = body_row(pane, index);
        cup(out, row, body_col);
        out.extend_from_slice(line.code.as_bytes());

        for (offset, run) in line.expand.iter().enumerate() {
            out.extend_from_slice(&command::encode_text_run(&TextRunCommand {
                col: body_col as i16 * 16,
                row: (row + 1 + offset as u16) as i16 * 16,
                scale: EXPANSION_SCALE,
                color: diag_color(Diag::Error),
                bg: EDITOR_BG,
                text: run.to_string(),
            }));
        }
    }
}

/// Emit the pane's gutter: a line number per line and a git/diag bar per flagged
/// line, all at absolute sixteenths offset to the pane origin, plus a hairline
/// separator down the gutter's right edge.
fn draw_gutter(out: &mut Vec<u8>, pane: &Pane) {
    let config = pane.gutter;
    let gutter_left = i16::try_from(pane.left + 1).unwrap_or(0) * 16;

    for (index, line) in BUFFER.iter().enumerate() {
        let number = index + 1;
        let y = body_row(pane, index) as i16 * 16;

        out.extend_from_slice(&command::encode_text_run(&TextRunCommand {
            col: gutter_left + number_col(number, config.number_right_edge()),
            row: y,
            scale: NUMBER_SCALE,
            color: NUMBER_FG,
            bg: EDITOR_BG,
            text: number.to_string(),
        }));

        if let Some(diag) = line.diag {
            out.extend_from_slice(&command::encode_bar(&BarCommand {
                x: gutter_left,
                y,
                width: config.bar_width,
                height: line_height(line) * 16,
                color: diag_color(diag),
            }));
        }

        if let Some(git) = line.git {
            out.extend_from_slice(&command::encode_bar(&BarCommand {
                x: gutter_left + config.git_x(),
                y,
                width: config.bar_width,
                height: 16,
                color: git_color(git),
            }));
        }
    }

    out.extend_from_slice(&command::encode_bar(&BarCommand {
        x: gutter_left + config.separator_x(),
        y: (pane.top + 1) as i16 * 16,
        width: 1,
        height: total_rows() * 16,
        color: SEPARATOR_COLOR,
    }));
}

/// The absolute grid row a line's code sits on in `pane`: the pane's first
/// interior row plus the line's physical row, so an expansion above shifts it.
fn body_row(pane: &Pane, line: usize) -> u16 {
    pane.top + 1 + physical_row(line)
}

/// The physical row a line starts on within the buffer: the sum of the heights
/// of the lines above it, folding in any inline expansion.
fn physical_row(line: usize) -> u16 {
    BUFFER[..line].iter().map(line_height).sum()
}

/// A line's height in rows: one, plus its inline-expansion rows.
fn line_height(line: &Line) -> u16 {
    1 + line.expand.len() as u16
}

/// Total physical rows the buffer occupies.
fn total_rows() -> u16 {
    BUFFER.iter().map(line_height).sum()
}

/// The right-aligned start column of `number`, in sixteenths, so every number
/// shares `right_edge`. The run advances one scaled cell per digit, so the start
/// backs off by the digit count.
fn number_col(number: usize, right_edge: i16) -> i16 {
    let digits = number.to_string().len() as f32;
    let advance = digits * NUMBER_SCALE as f32 / 256.0;
    let right = right_edge as f32 / 16.0;
    ((right - advance) * 16.0).round() as i16
}

/// The severity bar's color: red for an error, yellow for a warning.
fn diag_color(diag: Diag) -> [u8; 3] {
    match diag {
        Diag::Error => [224, 108, 117],
        Diag::Warning => [229, 192, 123],
    }
}

/// The git bar's color: green for an added line, blue for a modified one.
fn git_color(git: Git) -> [u8; 3] {
    match git {
        Git::Added => [152, 195, 121],
        Git::Modified => [97, 175, 239],
    }
}

/// Set the scene's foreground and background, so erased cells and body text
/// share the editor colors the gutter components blend against.
fn set_palette(out: &mut Vec<u8>) {
    out.extend_from_slice(
        format!(
            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
            EDITOR_FG[0], EDITOR_FG[1], EDITOR_FG[2], EDITOR_BG[0], EDITOR_BG[1], EDITOR_BG[2],
        )
        .as_bytes(),
    );
}

/// Emit a Cursor Position escape to the 0-based grid (`row`, `col`).
fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}
