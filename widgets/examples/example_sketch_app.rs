//! A sketch stoatty demo. Hand-drawn marks annotate code, each drawing itself
//! on as if by pen.
//!
//! A circle closes around one identifier, a curved connector grows from it to a
//! filled box, and two labels fade in as the marks they name finish. The
//! staggered delays are what make it read as an explanation rather than a
//! diagram appearing at once.
//!
//! Nothing here animates a frame. Each mark is declared once with a delay and a
//! duration, and the terminal draws the stroke at the display refresh rate. The
//! scene is re-emitted only on a resize or a replay.
//!
//! Press `r` to replay. That bumps every id, which is what starts a new draw,
//! since the terminal latches a mark's timing when its id first appears. `q` or
//! Ctrl-C quits. In any other terminal the marks draw nothing and the code
//! stands on its own. Run as the PTY shell by the `sketch` example.

use ratatui::{
    backend::CrosstermBackend,
    crossterm::event::{self, Event, KeyCode, KeyModifiers},
    style::{Color, Style},
    Frame, Terminal,
};
use std::io;
use stoat_widgets::{
    sketch::{SketchEllipse, SketchLine, SketchRect},
    text_run::TextRun,
    ApcScene, ApcSession, SessionOptions,
};
use stoatty_protocol::command::{
    SketchBounds, SketchEnd, SketchFill, SketchFillStyle, SketchSide, SketchStyle, SketchTiming,
};

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so the scene looks the same under any
/// theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// The circle and its label draw in amber (`#e5c07b`), the box and its label in
/// blue (`#61afef`).
const CIRCLE_FG: [u8; 3] = [229, 192, 123];
const BOX_FG: [u8; 3] = [97, 175, 239];

/// Sixteenths in one cell, the unit every mark is placed in.
const CELL: i16 = 16;

/// Ids a replay adds to every mark, wide enough that no round reuses another's.
///
/// A mark's timing is latched when its id first appears, so re-declaring the
/// same id changes nothing. A fresh id is what starts the draw again.
const REPLAY_STRIDE: u32 = 16;

/// The lines the marks annotate. Short enough that the whole scene fits the
/// window the `sketch` example opens.
const CODE: [&str; 5] = [
    "fn resolve(&self, id: u32) -> Option<Slot> {",
    "    let entry = self.table.get(&id)?;",
    "    let slot = entry.slot.load(Ordering::Acquire);",
    "    (slot != EMPTY).then_some(Slot(slot))",
    "}",
];

/// Where the code block starts, in cells.
const CODE_X: u16 = 4;
const CODE_Y: u16 = 2;

/// The identifier the circle closes around, as a column range within its line.
const SUBJECT_LINE: u16 = 2;
const SUBJECT_START: u16 = 15;
const SUBJECT_LEN: u16 = 6;

fn main() {
    let mut session = ApcSession::new(SessionOptions {
        raw_mode: true,
        hide_cursor: true,
        ..SessionOptions::default()
    });

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    terminal.clear().expect("clear the screen");

    let mut round = 0;
    loop {
        terminal
            .draw(|frame| draw_scene(frame, session.scene(), round))
            .expect("draw the scene");
        session.flush().expect("write the decoration");

        match event::read().expect("read a terminal event") {
            Event::Key(key) if key.code == KeyCode::Char('q') => break,
            Event::Key(key)
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                break
            },
            // Fresh ids, so every mark starts its draw over.
            Event::Key(key) if key.code == KeyCode::Char('r') => round += 1,
            // A resize re-emits the scene where the new layout puts it. Every
            // other event redraws the same marks under the ids they already
            // have, which the terminal recognizes and leaves running.
            Event::Resize(..) => {},
            _ => continue,
        }
    }
}

/// Write the code into the cells, then declare the marks over it.
///
/// `round` offsets every id, so a replay declares marks the terminal has not
/// seen and each one draws again from nothing.
fn draw_scene(frame: &mut Frame<'_>, scene: &mut ApcScene, round: u32) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, editor_style());

    for (offset, line) in CODE.iter().enumerate() {
        frame
            .buffer_mut()
            .set_string(CODE_X, CODE_Y + offset as u16, line, editor_style());
    }

    frame.buffer_mut().set_string(
        CODE_X,
        area.height.saturating_sub(2),
        "r replays the scene    q quits",
        Style::default().fg(Color::Rgb(99, 109, 131)),
    );

    let id = |slot: u32| slot + round * REPLAY_STRIDE;

    // The circle sits a little outside the identifier on every side, the margin
    // that makes a hand-drawn ring read as circling rather than underlining.
    const MARGIN: i16 = 6;
    let subject = SketchBounds {
        x: (CODE_X + SUBJECT_START) as i16 * CELL - MARGIN,
        y: (CODE_Y + SUBJECT_LINE) as i16 * CELL - MARGIN,
        w: SUBJECT_LEN * CELL as u16 + 2 * MARGIN as u16,
        h: CELL as u16 + 2 * MARGIN as u16,
    };

    frame.render_stateful_widget(
        SketchEllipse {
            id: id(1),
            style: SketchStyle::marker(CIRCLE_FG),
            timing: SketchTiming::after(200, 700),
            bounds: subject,
            anchor: None,
        },
        area,
        scene,
    );

    // The box the connector arrives at, off to the right of the code.
    let note = SketchBounds {
        x: 56 * CELL,
        y: 7 * CELL,
        w: 20 * CELL as u16,
        h: 4 * CELL as u16,
    };
    frame.render_stateful_widget(
        SketchRect {
            id: id(2),
            style: SketchStyle::marker(BOX_FG),
            timing: SketchTiming::after(1100, 700),
            bounds: note,
            radius: 8,
            fill: Some(SketchFill {
                color: [30, 40, 56],
                alpha: 210,
                style: SketchFillStyle::Solid,
            }),
            anchor: None,
        },
        area,
        scene,
    );

    // Both ends name a mark, so the connector tracks them and leaves each on the
    // side facing the other. It draws between the two, once the circle has
    // closed and before the box opens.
    frame.render_stateful_widget(
        SketchLine {
            id: id(3),
            style: SketchStyle::marker(BOX_FG),
            timing: SketchTiming::after(900, 500),
            from: SketchEnd::Component {
                id: id(1),
                side: SketchSide::Auto,
            },
            to: SketchEnd::Component {
                id: id(2),
                side: SketchSide::Auto,
            },
            bend: 0,
            heads: 2,
            anchor: None,
        },
        area,
        scene,
    );

    // Each label follows the mark it names, so it fades in as that mark closes
    // rather than sitting there while the pen is still moving.
    frame.render_stateful_widget(
        TextRun {
            col: 57 * CELL,
            row: 8 * CELL,
            scale: 224,
            color: BOX_FG,
            bg: None,
            text: "the slot may be EMPTY",
            follow: id(2),
            anchor: None,
        },
        area,
        scene,
    );
    frame.render_stateful_widget(
        TextRun {
            col: 57 * CELL,
            row: 9 * CELL + 8,
            scale: 224,
            color: EDITOR_FG,
            bg: None,
            text: "which is not an id",
            follow: id(2),
            anchor: None,
        },
        area,
        scene,
    );
}

fn editor_style() -> Style {
    Style::default()
        .fg(Color::Rgb(EDITOR_FG[0], EDITOR_FG[1], EDITOR_FG[2]))
        .bg(Color::Rgb(EDITOR_BG[0], EDITOR_BG[1], EDITOR_BG[2]))
}
