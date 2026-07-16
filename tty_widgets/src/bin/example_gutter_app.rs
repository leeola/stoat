//! A gutter stoatty demo: three editor panes tiled side by side, each drawing
//! the same code with its own gutter at a different size -- wide color bars,
//! compressed, then padded -- so the gutter geometry reads as just sub-cell
//! coordinates the emitter places.
//!
//! Each pane is framed by a [`Border`] widget; its [`Gutter`] packs
//! smaller-than-grid line numbers, thin git and diagnostic color bars, and a
//! hairline separator into a few columns off the cell grid, while the code stays
//! on the uniform grid. One line carries an integer-cell inline expansion (an
//! inline diagnostic) that pushes the lines below it down.
//!
//! A single per-surface line layout cannot bind independent side-by-side panes
//! (that is deferred multi-surface work), so each [`Gutter`] positions its
//! components relative to its pane's area and folds the expansion shift in
//! itself rather than declaring a line layout. The components ride in sixteenths
//! of a cell, so they track live font zoom.
//!
//! Cells flow through a ratatui [`Terminal`] (the editor background, the body
//! code, and the widgets' graceful-degradation fallback) and decoration through
//! the widgets into an [`ApcScene`]; the cell diff is flushed first, then the
//! scene. The scene is static: drawn once and held. Run as the PTY shell by the
//! `gutter` example.

use ratatui::{
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Style},
    Frame, Terminal,
};
use std::{
    io::{self, Write},
    thread,
};
use stoatty_protocol::command::BorderStyle;
use stoatty_widgets::{
    border::Border,
    gutter::{Diagnostic, GitMark, Gutter, GutterLine},
    text_run::TextRun,
    ApcScene,
};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so erased cells share a known
/// background the gutter components composite over.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Pane border color (`#4e5666`).
const BORDER_COLOR: [u8; 3] = [78, 86, 102];

/// Line-number color (`#636d83`).
const NUMBER_FG: [u8; 3] = [99, 109, 131];

/// Separator color (`#3c424d`), a hair lighter than the background.
const SEPARATOR_COLOR: [u8; 3] = [60, 66, 77];

/// Line-number glyph size in 256ths of a cell (160 = 0.625x), so the number is
/// smaller than the body text.
const NUMBER_SCALE: u16 = 160;

/// Inline-expansion glyph size in 256ths of a cell (200 = 0.78x), so the inline
/// diagnostic reads smaller than the full-cell body code.
const EXPANSION_SCALE: u16 = 200;

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

/// A gutter's sub-cell sizing, in sixteenths of a cell, so each pane can draw its
/// gutter at a different size.
#[derive(Clone, Copy)]
struct GutterConfig {
    /// Color-bar width.
    bar_width: u16,
    /// Inter-element padding.
    pad: u16,
}

/// One tiled pane: a bordered rectangle drawing [`BUFFER`] with `gutter`'s sizing.
struct Pane {
    top: u16,
    left: u16,
    width: u16,
    height: u16,
    gutter: GutterConfig,
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
    let lines = gutter_lines();

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    let mut scene = ApcScene::new();

    terminal.clear().expect("clear the screen");
    terminal
        .draw(|frame| draw_scene(frame, &mut scene, &lines))
        .expect("draw the scene");

    let mut out = io::stdout();
    scene.flush_to(&mut out).expect("write the decoration");
    out.flush().expect("flush the decoration");

    // Hold so the panes stay still and the window keeps the process alive.
    loop {
        thread::park();
    }
}

/// Draw every pane's cells into `frame` and its decoration into `scene`, then
/// rest the cursor on the first pane's flagged line so the scene reads as
/// mid-edit.
fn draw_scene(frame: &mut Frame<'_>, scene: &mut ApcScene, lines: &[GutterLine]) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, editor_style());

    for pane in &PANES {
        draw_pane(frame, scene, pane, lines);
    }

    let first = &PANES[0];
    let body_col = first.left + 1 + pane_gutter(first, lines).cell_width();
    frame.set_cursor_position((body_col, body_row(first, FLAGGED_LINE)));
}

/// Draw a pane: its border and gutter as widgets, then its code body.
fn draw_pane(frame: &mut Frame<'_>, scene: &mut ApcScene, pane: &Pane, lines: &[GutterLine]) {
    frame.render_stateful_widget(
        Border {
            style: BorderStyle::Rounded,
            color: BORDER_COLOR,
        },
        Rect::new(pane.left, pane.top, pane.width, pane.height),
        scene,
    );

    let gutter = pane_gutter(pane, lines);
    let cell_width = gutter.cell_width();
    frame.render_stateful_widget(
        gutter,
        Rect::new(pane.left + 1, pane.top + 1, cell_width, pane.height - 2),
        scene,
    );

    draw_body(frame, scene, pane, cell_width);
}

/// Write each line's code at its physical row inside the pane, then any
/// inline-expansion rows just beneath it as smaller, error-colored [`TextRun`]s,
/// so the inline diagnostic reads at a different size and color from the code.
fn draw_body(frame: &mut Frame<'_>, scene: &mut ApcScene, pane: &Pane, cell_width: u16) {
    let body_col = pane.left + 1 + cell_width;
    let body_width = pane.width.saturating_sub(cell_width + 2);

    for (index, line) in BUFFER.iter().enumerate() {
        let row = body_row(pane, index);
        frame.buffer_mut().set_stringn(
            body_col,
            row,
            line.code,
            body_width as usize,
            editor_style(),
        );

        for (offset, run) in line.expand.iter().enumerate() {
            frame.render_stateful_widget(
                TextRun {
                    col: 0,
                    row: 0,
                    scale: EXPANSION_SCALE,
                    color: diag_color(Diag::Error),
                    bg: Some(EDITOR_BG),
                    text: run,
                },
                Rect::new(body_col, row + 1 + offset as u16, body_width, 1),
                scene,
            );
        }
    }
}

/// Build the pane's [`Gutter`] over the shared `lines`, sized by its config.
fn pane_gutter<'a>(pane: &Pane, lines: &'a [GutterLine]) -> Gutter<'a> {
    Gutter {
        lines,
        bar_width: pane.gutter.bar_width,
        pad: pane.gutter.pad,
        number_scale: NUMBER_SCALE,
        width_digits: width_digits(),
        number_fg: NUMBER_FG,
        separator: SEPARATOR_COLOR,
        bg: EDITOR_BG,
    }
}

/// The shared gutter lines, one per [`BUFFER`] entry: its number, row height, and
/// git/diagnostic marks.
fn gutter_lines() -> Vec<GutterLine> {
    BUFFER
        .iter()
        .enumerate()
        .map(|(index, line)| GutterLine {
            number: index as u32 + 1,
            height: line_height(line),
            git: line.git.map(|g| GitMark {
                color: git_color(g),
                seam: false,
            }),
            diagnostic: line.diag.map(|diag| Diagnostic {
                color: diag_color(diag),
                mark: diag_mark(diag),
            }),
        })
        .collect()
}

/// Digits in the widest line number, sizing the gutter's number column.
fn width_digits() -> u16 {
    (BUFFER.len() as u32).ilog10() as u16 + 1
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

/// The severity bar's color: red for an error, yellow for a warning.
fn diag_color(diag: Diag) -> [u8; 3] {
    match diag {
        Diag::Error => [224, 108, 117],
        Diag::Warning => [229, 192, 123],
    }
}

/// The severity letter shown in the cell fallback.
fn diag_mark(diag: Diag) -> char {
    match diag {
        Diag::Error => 'E',
        Diag::Warning => 'W',
    }
}

/// The git bar's color: green for an added line, blue for a modified one.
fn git_color(git: Git) -> [u8; 3] {
    match git {
        Git::Added => [152, 195, 121],
        Git::Modified => [97, 175, 239],
    }
}

/// The editor's foreground-on-background cell style, shared by erased cells and
/// body text so the gutter components composite over a known color.
fn editor_style() -> Style {
    Style::default().fg(rgb(EDITOR_FG)).bg(rgb(EDITOR_BG))
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}
