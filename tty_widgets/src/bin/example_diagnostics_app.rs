//! A diagnostics stoatty demo: an editor scene where an error span and a warning
//! span carry severity-colored curly underlines, and a rounded tooltip hovers
//! just below the error span -- the VS Code hover-on-error look.
//!
//! The tooltip is a [`Popover`] with a severity-colored rounded border anchored
//! at a sub-cell pixel offset under the span's start, holding a severity message
//! and an [`Icon`] composited above its fill. The editor cells flow through a
//! ratatui [`Terminal`]; the tooltip's decoration flows through the widgets into
//! an [`ApcScene`].
//!
//! The underlines are plain VT (undercurl plus an underline color). ratatui's
//! cell model has no undercurl, so they are re-stamped as raw VT over the cells
//! ratatui drew, after the cell diff. The scene is static: drawn once and held,
//! so the screen-anchored tooltip keeps sitting under the span. Run as the PTY
//! shell by the `diagnostics` example.

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
use stoatty_protocol::command::IconKind;
use stoatty_widgets::{icon::Icon, popover::Popover, ApcScene};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so erased cells and body text share the
/// editor colors regardless of the active theme.
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

/// The tooltip message, indented two cells so the icon's cell stays clear of the
/// text.
const TOOLTIP_CONTENT: &str = "  no method `from_str` found\n  help: did you mean `parse`?";

fn main() {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    let mut scene = ApcScene::new();

    terminal.clear().expect("clear the screen");
    terminal
        .draw(|frame| draw_scene(frame, &mut scene))
        .expect("draw the scene");

    let mut out = io::stdout();
    underline_spans(&mut out);
    scene.flush_to(&mut out).expect("write the decoration");
    rest_cursor(&mut out);
    out.flush().expect("flush the scene");

    // Hold so the buffer stays still and the window keeps the process alive;
    // nothing animates.
    loop {
        thread::park();
    }
}

/// Draw the editor cells and the tooltip's decoration into `frame` and `scene`.
///
/// The curly underlines are emitted separately as raw VT; the frame cursor is set
/// so ratatui shows it, and [`rest_cursor`] places it after the raw re-stamps.
fn draw_scene(frame: &mut Frame<'_>, scene: &mut ApcScene) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, editor_style());

    for (row, line) in BUFFER.iter().enumerate() {
        frame
            .buffer_mut()
            .set_string(0, row as u16, line, editor_style());
    }

    frame.render_stateful_widget(
        Popover {
            fill: TOOLTIP_FILL,
            border: ERROR,
            content_fg: ERROR,
            scale: 1,
            offset: [3, 6],
            bold: false,
            content: TOOLTIP_CONTENT,
        },
        Rect::new(ERROR_COL, ERROR_ROW + 1, 34, 4),
        scene,
    );

    frame.render_stateful_widget(
        Icon {
            kind: IconKind::Error,
            color: ERROR,
            size: 1,
            offset: [0, 0],
        },
        Rect::new(ERROR_COL, ERROR_ROW + 1, 1, 1),
        scene,
    );

    frame.set_cursor_position((ERROR_COL, ERROR_ROW));
}

/// Re-stamp the warning and error spans with severity-colored curly underlines.
///
/// ratatui's cell model has no undercurl, so the squiggles are raw VT over the
/// cells ratatui already drew. The scene is static, so this overlay is never
/// overwritten by a later draw.
fn underline_spans(out: &mut impl Write) {
    underline_span(out, WARNING_ROW, WARNING_COL, WARNING_TEXT, WARNING);
    underline_span(out, ERROR_ROW, ERROR_COL, ERROR_TEXT, ERROR);
}

/// Re-stamp `text` at (`row`, `col`) with a curly underline in `color`, keeping
/// the editor foreground and background so only the decoration is added.
fn underline_span(out: &mut impl Write, row: u16, col: u16, text: &str, color: [u8; 3]) {
    let [fr, fg, fb] = EDITOR_FG;
    let [br, bg, bb] = EDITOR_BG;
    let [ur, ug, ub] = color;
    write!(
        out,
        "\x1b[{};{}H\x1b[38;2;{};{};{};48;2;{};{};{};4:3;58:2::{}:{}:{}m{}\x1b[0m",
        row + 1,
        col + 1,
        fr,
        fg,
        fb,
        br,
        bg,
        bb,
        ur,
        ug,
        ub,
        text,
    )
    .expect("write the underline");
}

/// Rest the cursor on the error span so the scene reads as a hover-on-error.
fn rest_cursor(out: &mut impl Write) {
    write!(out, "\x1b[{};{}H", ERROR_ROW + 1, ERROR_COL + 1).expect("write the cursor rest");
}

/// The editor's foreground-on-background cell style, shared by erased cells and
/// body text.
fn editor_style() -> Style {
    Style::default().fg(rgb(EDITOR_FG)).bg(rgb(EDITOR_BG))
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}
