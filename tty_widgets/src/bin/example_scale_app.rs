//! A scale stoatty demo: one glyph drawn at 1x, 2x, and 4x cell size, side by
//! side, each scaled glyph owning the integer cell block it occupies.
//!
//! The base glyph is an ordinary VT cell; the [`Scale`] widget emits a `scale`
//! frame over it, so the terminal redraws that cell's glyph across a
//! scale-by-scale block and claims the rest of the block. The three glyphs are
//! spaced so no block draws into another, the same occupancy a wide character
//! already demands. The cells flow through a ratatui [`Terminal`]; the scale
//! frames flow through the widget into an [`ApcScene`].
//!
//! The scene is static: drawn once and held. In any other terminal the scale
//! frames are ignored and the three glyphs render at their normal size. Run as
//! the PTY shell by the `scale` example.

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
use stoatty_widgets::{scale::Scale, ApcScene};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so the scene looks the same under any
/// theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Color of the scaled glyphs (`#61afef`), so they stand out from the labels.
const GLYPH_FG: [u8; 3] = [97, 175, 239];

/// The glyph drawn at each scale. The terminal scales whatever glyph the cell
/// holds, so the same character at three scales shows the size difference alone.
const GLYPH: &str = "A";

/// `(scale, column)` per demo glyph. The columns leave each scaled glyph's
/// scale-by-scale block clear of the next, so no glyph draws into another's
/// claimed cells.
const GLYPHS: [(u8, u16); 3] = [(1, 6), (2, 16), (4, 30)];

/// Row the glyphs' top-left cell sits on, with the per-scale labels one row
/// above so they clear even the 4x block.
const GLYPH_ROW: u16 = 6;
const LABEL_ROW: u16 = 4;

fn main() {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    let mut scene = ApcScene::new();

    terminal.clear().expect("clear the screen");
    terminal
        .draw(|frame| draw_scene(frame, &mut scene))
        .expect("draw the scene");

    let mut out = io::stdout();
    out.write_all(b"\x1b[?25l").expect("hide the cursor");
    scene.flush_to(&mut out).expect("write the decoration");
    out.flush().expect("flush the scene");

    // Hold so the scene stays still and the window keeps the process alive;
    // nothing animates.
    loop {
        thread::park();
    }
}

/// Draw the title and per-scale labels, write each base glyph, then emit a
/// `scale` frame over each glyph's block so the terminal draws it at 1x, 2x, 4x.
fn draw_scene(frame: &mut Frame<'_>, scene: &mut ApcScene) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, editor_style());

    frame.buffer_mut().set_string(
        2,
        1,
        "A glyph drawn at 1x, 2x, and 4x cell size",
        editor_style(),
    );

    for (scale, col) in GLYPHS {
        frame
            .buffer_mut()
            .set_string(col, LABEL_ROW, format!("{scale}x"), editor_style());
        frame
            .buffer_mut()
            .set_string(col, GLYPH_ROW, GLYPH, glyph_style());

        frame.render_stateful_widget(
            Scale { scale },
            Rect::new(col, GLYPH_ROW, u16::from(scale), u16::from(scale)),
            scene,
        );
    }
}

/// The editor's foreground-on-background cell style, shared by erased cells, the
/// title, and the labels.
fn editor_style() -> Style {
    Style::default().fg(rgb(EDITOR_FG)).bg(rgb(EDITOR_BG))
}

/// The scaled glyph's accent-on-background style.
fn glyph_style() -> Style {
    Style::default().fg(rgb(GLYPH_FG)).bg(rgb(EDITOR_BG))
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}
