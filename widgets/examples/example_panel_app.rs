//! A panel stoatty demo. Every shadow style is drawn against every border
//! weight, so the look of a modal is picked rather than guessed at.
//!
//! [`PanelShadow`] has four styles and [`BorderStyle`] four weights, and the
//! corner radius, the fill, and the horizontal inset change the box as well.
//! The grid puts a row per shadow beside a column per weight. The other three
//! vary across it, so all three read against every shadow: square corners on
//! the left half and rounded on the right, a fill on every other row, and an
//! inset on the last column.
//!
//! Each panel carries its own title, so a combination that reads well is named
//! where it is seen.
//!
//! The frame flows through the [`Panel`] widget and the title through the
//! [`TextRun`] widget into an [`ApcScene`]. In any other terminal the panel
//! degrades to a box-drawing border and the title to ordinary cells, so the
//! grid still reads. Run as the PTY shell by the `panel` example.

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

/// The fill the even rows carry (`#31353f`), a shade off the editor background
/// so a filled box reads against an unfilled one.
const PANEL_FILL: [u8; 3] = [49, 53, 63];

/// The title's glyph size in 256ths of a cell, small enough that a name fits on
/// an 18-cell top edge.
const TITLE_SCALE: u16 = 160;

/// One grid cell's panel, in cells, and the gaps between them.
const PANEL_W: u16 = 18;
const PANEL_H: u16 = 6;
const GAP_X: u16 = 4;
const GAP_Y: u16 = 2;

/// The grid's top-left corner, in cells.
const ORIGIN_X: u16 = 2;
const ORIGIN_Y: u16 = 1;

/// The shadow styles the rows step through, with the name each is titled by.
const SHADOWS: [(PanelShadow, &str); 4] = [
    (PanelShadow::None_, "none"),
    (PanelShadow::Drop, "drop"),
    (PanelShadow::Tucked, "tuck"),
    (PanelShadow::Overhang, "over"),
];

/// The border weights the columns step through, with the name each is titled by.
const BORDERS: [(BorderStyle, &str); 4] = [
    (BorderStyle::Light, "light"),
    (BorderStyle::Heavy, "heavy"),
    (BorderStyle::Double, "double"),
    (BorderStyle::Rounded, "round"),
];

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

/// Fill the background, then draw one panel per shadow and border pair.
///
/// The corner radius, the fill, and the inset vary across the grid rather than
/// getting rows of their own, so all three read against every shadow without a
/// grid too large to take in at once.
fn draw_scene(frame: &mut Frame<'_>, scene: &mut ApcScene) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, editor_style());

    for (row, (shadow, shadow_name)) in SHADOWS.into_iter().enumerate() {
        for (column, (style, style_name)) in BORDERS.into_iter().enumerate() {
            let panel = Rect {
                x: ORIGIN_X + column as u16 * (PANEL_W + GAP_X),
                y: ORIGIN_Y + row as u16 * (PANEL_H + GAP_Y),
                width: PANEL_W,
                height: PANEL_H,
            };

            frame.render_stateful_widget(
                Panel {
                    style,
                    border: FRAME_FG,
                    corner_radius: match column < 2 {
                        true => 0,
                        false => 8,
                    },
                    fill: (row % 2 == 0).then_some(PANEL_FILL),
                    shadow,
                    inset_x: match column == BORDERS.len() - 1 {
                        true => 4,
                        false => 0,
                    },
                    above_pools: false,
                    anchor: None,
                },
                panel,
                scene,
            );

            // The title sits on the top edge, its background masking the
            // hairline beneath it the way a ratatui block title breaks its
            // border.
            frame.render_stateful_widget(
                TextRun {
                    col: 2 * 16,
                    row: 0,
                    scale: TITLE_SCALE,
                    color: TITLE_FG,
                    bg: Some(EDITOR_BG),
                    text: &format!(" {shadow_name} {style_name} "),
                    follow: 0,
                    anchor: None,
                },
                panel,
                scene,
            );
        }
    }

    frame.buffer_mut().set_string(
        ORIGIN_X,
        ORIGIN_Y + SHADOWS.len() as u16 * (PANEL_H + GAP_Y),
        "rows: shadow style   columns: border weight   \
         square corners left, rounded right, fill on alternate rows, inset last column",
        editor_style(),
    );
}

fn editor_style() -> Style {
    Style::default().fg(rgb(EDITOR_FG)).bg(rgb(EDITOR_BG))
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}
