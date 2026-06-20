//! A split-pane stoatty demo: a fixed sidebar beside a tall buffer whose
//! viewport chases a cursor via the per-region scroll command.
//!
//! The left pane is static; the right pane shows a window onto a longer buffer.
//! A fixed sequence of cursor jumps moves the window, and each jump reports the
//! window's absolute scroll position through the [`ScrollRegion`] widget, so the
//! renderer eases the change while the left pane stays put.
//!
//! The panes are framed by the [`Border`] widget and the sidebar and viewport
//! flow through a ratatui [`Terminal`]. The two decorations have different
//! lifecycles: the borders are static, so a dedicated [`ApcScene`] flushes them
//! once (its skip-when-unchanged emits nothing after the first frame); the
//! scroll region updates every jump, so its scene's bytes are written directly,
//! never behind a `Gstoatty;reset` that would drop the eased position. Run as the
//! PTY shell by the `split_scroll` example.

use ratatui::{backend::CrosstermBackend, layout::Rect, style::Style, Frame, Terminal};
use std::{
    io::{self, Write},
    thread,
    time::Duration,
};
use stoatty_protocol::command::BorderStyle;
use stoatty_widgets::{border::Border, scroll_region::ScrollRegion, ApcScene};

/// Left (fixed) pane border, in 0-based grid coordinates.
const LEFT: Rect = Rect {
    x: 2,
    y: 1,
    width: 20,
    height: 14,
};

/// Right (scrolling) pane border, in 0-based grid coordinates.
const RIGHT: Rect = Rect {
    x: 26,
    y: 1,
    width: 32,
    height: 14,
};

/// The right pane's interior: where the scrolled window is drawn.
const INNER: Rect = Rect {
    x: RIGHT.x + 1,
    y: RIGHT.y + 1,
    width: RIGHT.width - 2,
    height: RIGHT.height - 2,
};

/// Both panes' border color.
const BORDER_COLOR: [u8; 3] = [120, 130, 150];

/// Rows of buffer the right pane shows at once: its border's interior height.
const VIEWPORT_H: usize = INNER.height as usize;

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

/// The `render.rs` the sidebar lists, shown in the scrolling pane as one
/// coherent file so the scroll reads as real source rather than a repeating
/// cycle. Each line fits the pane's interior so none clips.
const RENDER_RS: [&str; BUFFER_LINES] = [
    "use crate::grid::Grid;",
    "use crate::cell::Rgb;",
    "",
    "/// Draws a cell grid.",
    "pub struct Renderer {",
    "    grid: Grid,",
    "    bg: Rgb,",
    "}",
    "",
    "impl Renderer {",
    "    pub fn new(",
    "        grid: Grid,",
    "    ) -> Renderer {",
    "        let bg =",
    "            Rgb::black();",
    "        Self { grid, bg }",
    "    }",
    "",
    "    /// Paint one frame.",
    "    pub fn frame(",
    "        &self,",
    "    ) -> Vec<u8> {",
    "        let mut out =",
    "            Vec::new();",
    "        for c in",
    "            self.cells()",
    "        {",
    "            out.push(c);",
    "        }",
    "        out",
    "    }",
    "",
    "    /// Grid width.",
    "    pub fn cols(",
    "        &self,",
    "    ) -> usize {",
    "        self.grid.cols()",
    "    }",
    "}",
    "",
];

fn main() {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    let mut border_scene = ApcScene::new();
    let mut scroll_scene = ApcScene::new();

    terminal.clear().expect("clear the screen");

    loop {
        for (cursor_line, settle_ms) in CURSOR_JUMPS {
            let scroll = cursor_line.saturating_sub(CURSOR_MARGIN).min(MAX_SCROLL);

            border_scene.clear();
            scroll_scene.clear();
            terminal
                .draw(|frame| {
                    render_frame(
                        frame,
                        &mut border_scene,
                        &mut scroll_scene,
                        scroll,
                        cursor_line,
                    )
                })
                .expect("draw a frame");

            let mut out = io::stdout();
            border_scene.flush_to(&mut out).expect("write the borders");
            out.write_all(scroll_scene.buffer())
                .expect("write the scroll region");
            out.flush().expect("flush a frame");

            thread::sleep(Duration::from_millis(settle_ms));
        }
    }
}

/// Draw both panes' borders, the fixed sidebar, and the right pane's window for
/// `scroll`, report the scroll position, and rest the cursor on `cursor_line`.
///
/// The window keeps the cursor [`CURSOR_MARGIN`] rows from its top where it can,
/// so a cursor jump moves the window with it.
fn render_frame(
    frame: &mut Frame<'_>,
    border_scene: &mut ApcScene,
    scroll_scene: &mut ApcScene,
    scroll: usize,
    cursor_line: usize,
) {
    for rect in [LEFT, RIGHT] {
        frame.render_stateful_widget(
            Border {
                style: BorderStyle::Light,
                color: BORDER_COLOR,
            },
            rect,
            border_scene,
        );
    }

    draw_sidebar(frame);
    draw_window(frame, scroll);

    frame.render_stateful_widget(
        ScrollRegion {
            offset: scroll as u16,
        },
        INNER,
        scroll_scene,
    );

    frame.set_cursor_position((INNER.x, INNER.y + (cursor_line - scroll) as u16));
}

/// Write the fixed sidebar lines inside the left pane.
fn draw_sidebar(frame: &mut Frame<'_>) {
    for (row, line) in SIDEBAR.iter().enumerate() {
        frame
            .buffer_mut()
            .set_string(LEFT.x + 1, LEFT.y + 1 + row as u16, line, Style::default());
    }
}

/// Write the right pane's window starting at buffer line `scroll`, each line
/// clipped to the interior width so nothing spills across the border.
fn draw_window(frame: &mut Frame<'_>, scroll: usize) {
    for row in 0..VIEWPORT_H {
        let line = buffer_line(scroll + row);
        frame.buffer_mut().set_stringn(
            INNER.x,
            INNER.y + row as u16,
            &line,
            INNER.width as usize,
            Style::default(),
        );
    }
}

/// The text of buffer line `index`: a line number and the source line at that
/// position in [`RENDER_RS`], so the scroll reads as one continuous file.
fn buffer_line(index: usize) -> String {
    format!("{:>3}  {}", index + 1, RENDER_RS[index])
}
