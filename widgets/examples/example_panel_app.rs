//! A panel stoatty demo showing a centered modal dialog drawn as off-grid
//! chrome. The dialog is a hairline rounded frame with a soft drop shadow
//! floating over the editor cells, plus a title text run sitting on the top
//! edge.
//!
//! The frame flows through the [`Panel`] widget and the title through the
//! [`TextRun`] widget into an [`ApcScene`]. The cells inside keep their own
//! background. In any other terminal the panel degrades to a box-drawing border
//! and the title to ordinary cells, so the dialog still reads. Run as the PTY
//! shell by the `panel` example.

use ratatui::{
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Style},
    Frame, Terminal,
};
use std::{io, thread};
use stoat_widgets::{panel::Panel, text_run::TextRun, ApcScene, ApcSession, SessionOptions};
use stoatty_protocol::command::{BorderStyle, PanelShadow};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so the scene looks the same under any
/// theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Frame color (`#5c6370`), a muted line, and title color (`#61afef`) accent.
const FRAME_FG: [u8; 3] = [92, 99, 112];
const TITLE_FG: [u8; 3] = [97, 175, 239];

/// The title text run's glyph size in 256ths of a cell, matching the body text.
const TITLE_SCALE: u16 = 256;

fn main() {
    let mut session = ApcSession::new(SessionOptions {
        hide_cursor: true,
        ..SessionOptions::default()
    });

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");

    terminal.clear().expect("clear the screen");
    terminal
        .draw(|frame| draw_scene(frame, session.scene()))
        .expect("draw the scene");
    session.flush().expect("write the decoration");

    // Hold so the scene stays still and the window keeps the process alive;
    // nothing animates. Closing the window ends the process outright, so the
    // session's own restore never runs and its panic hook is what covers a
    // crash.
    loop {
        thread::park();
    }
}

/// Fill the background, write the dialog's body text into the cells, then emit
/// the panel frame and the title text run over it.
fn draw_scene(frame: &mut Frame<'_>, scene: &mut ApcScene) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, editor_style());

    let dialog = centered(area, 34, 8);

    frame.buffer_mut().set_string(
        dialog.x + 3,
        dialog.y + 3,
        "A modal panel drawn off the grid.",
        editor_style(),
    );

    frame.render_stateful_widget(
        Panel {
            style: BorderStyle::Rounded,
            border: FRAME_FG,
            corner_radius: 6,
            fill: None,
            shadow: PanelShadow::Drop,
            inset_x: 0,
            above_pools: false,
            anchor: None,
        },
        dialog,
        scene,
    );

    // The title sits on the top edge, its background masking the hairline
    // beneath it the way a ratatui block title breaks its border.
    frame.render_stateful_widget(
        TextRun {
            col: 3 * 16,
            row: 0,
            scale: TITLE_SCALE,
            color: TITLE_FG,
            bg: Some(EDITOR_BG),
            text: " Panel ",
            follow: 0,
            anchor: None,
        },
        dialog,
        scene,
    );
}

/// A `width` by `height` rectangle centered within `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

fn editor_style() -> Style {
    Style::default().fg(rgb(EDITOR_FG)).bg(rgb(EDITOR_BG))
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}
