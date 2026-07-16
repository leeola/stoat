//! A hover-doc stoatty demo: a code buffer with a documentation tooltip anchored
//! under the word beneath the cursor.
//!
//! The tooltip is a rounded [`Popover`] anchored sub-cell to the word, its
//! content drawn at a larger-than-grid font and overflowing the box so the
//! renderer auto-scrolls it -- the Zed/VS Code hover-on-symbol look. The code
//! cells flow through a ratatui [`Terminal`]; the tooltip flows through the
//! widget into an [`ApcScene`].
//!
//! The scene sets no palette, so the code rides on the terminal default
//! background, as it did before. It is static: drawn once and held, so the
//! screen-anchored tooltip keeps tracking the word. Run as the PTY shell by the
//! `doc_tooltip` example.

use ratatui::{backend::CrosstermBackend, layout::Rect, style::Style, Frame, Terminal};
use std::{
    io::{self, Write},
    thread,
};
use stoatty_widgets::{popover::Popover, ApcScene};

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

/// The documentation text, longer than the box so the renderer auto-scrolls it.
const TOOLTIP_LINES: [&str; 14] = [
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
];

fn main() {
    let content = TOOLTIP_LINES.join("\n");

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    let mut scene = ApcScene::new();

    terminal.clear().expect("clear the screen");
    terminal
        .draw(|frame| draw_scene(frame, &mut scene, &content))
        .expect("draw the scene");

    let mut out = io::stdout();
    scene.flush_to(&mut out).expect("write the decoration");
    out.flush().expect("flush the scene");

    // Hold so the buffer stays still and the window keeps the process alive; the
    // app auto-scrolls the overflowing tooltip content on its own.
    loop {
        thread::park();
    }
}

/// Draw the code buffer and the documentation popover, resting the cursor on the
/// documented word so the scene reads as a hover.
///
/// Setting the frame cursor makes ratatui show it on the word; the popover APC
/// frame does not move it, so no separate cursor escape is needed.
fn draw_scene(frame: &mut Frame<'_>, scene: &mut ApcScene, content: &str) {
    for (row, line) in BUFFER.iter().enumerate() {
        frame
            .buffer_mut()
            .set_string(0, row as u16, line, Style::default());
    }

    frame.render_stateful_widget(
        Popover {
            fill: [22, 24, 34],
            border: [120, 170, 255],
            content_fg: [228, 232, 240],
            scale: 2,
            offset: [6, 4],
            bold: false,
            content,
        },
        Rect::new(WORD_COL - 2, WORD_ROW + 1, 56, 12),
        scene,
    );

    frame.set_cursor_position((WORD_COL, WORD_ROW));
}
