//! A status-bar stoatty demo. An editor-colored body sits above a bar of
//! scaled segments packed from both edges.
//!
//! [`StatusBar`]'s only consumer inside the editor is the pane, so a segment's
//! weight against its bar, and the hairline reading through the segments that
//! share the row's background, otherwise need the editor open on a file.
//!
//! The bar's two packing rules are what the keys drive. The left run cuts its
//! last segment to the glyphs that fit before the right edge. A right segment
//! overlapping the left run is dropped whole. The `p` key swaps in a path long
//! enough to force both at once.
//!
//! The widget writes no cell fallback, so a session no stoatty answers paints
//! the segment texts as plain cells instead.
//!
//! Run as the PTY shell by the `status_bar` example.

use ratatui::{
    backend::CrosstermBackend,
    crossterm::event::{self, Event, KeyCode, KeyModifiers},
    layout::Rect,
    style::{Color, Style},
    Frame, Terminal,
};
use std::{
    io,
    time::{Duration, Instant},
};
use stoat_widgets::{
    status_bar::{StatusBar, StatusSegment},
    ApcScene, ApcSession, SessionOptions,
};

/// The window the `status_bar` example opens, in cells. The rows come from the
/// live area, so only the width is named here.
const COLS: u16 = 80;

/// The editor body's background, which the bar carries as its own so a segment
/// sharing it paints no box.
const EDITOR_BG: [u8; 3] = [40, 44, 52];

/// The hairline along the bar's top edge.
const SEPARATOR: [u8; 3] = [60, 66, 77];

/// Body text and the foreground a segment on the bar's own background takes.
const BODY_FG: [u8; 3] = [171, 178, 191];

/// The foreground every segment on a colored bar takes.
const ON_COLOR: [u8; 3] = [40, 44, 52];

/// The green a diff segment sits on, and the amber a language segment does.
const DIFF_BG: [u8; 3] = [152, 195, 121];
const LANG_BG: [u8; 3] = [229, 192, 123];

/// The short file the bar names, and the long one `p` swaps in.
///
/// The long one is wide enough that the left run cuts it and the right run has
/// nowhere left to pack into, which is both rules at once.
const SHORT_PATH: &str = " src/main.rs ";
const LONG_PATH: &str = " crates/renderer/src/passes/composite/overlay_blend.rs ";

/// The two glyph sizes `s` steps between, in 256ths of a cell.
const SCALES: [u16; 2] = [160, 256];

/// The body text drawn above the bar.
const BODY: [&str; 6] = [
    "fn compose(frame: &Frame) -> Scene {",
    "    let mut scene = Scene::new();",
    "    for pass in frame.passes() {",
    "        scene.extend(pass.record());",
    "    }",
    "    scene",
];

/// One editor mode the `m` key cycles through, with the bar color it sits on.
struct Mode {
    label: &'static str,
    bg: [u8; 3],
}

const MODES: [Mode; 3] = [
    Mode {
        label: " NORMAL ",
        bg: [97, 175, 239],
    },
    Mode {
        label: " INSERT ",
        bg: [152, 195, 121],
    },
    Mode {
        label: " SELECT ",
        bg: [198, 120, 221],
    },
];

fn main() {
    // The session holds raw mode and the hidden cursor for as long as it lives,
    // and gives both back on the way out of main -- including the way out an
    // `expect` below takes, which its panic hook covers.
    let session = ApcSession::new(SessionOptions {
        raw_mode: true,
        hide_cursor: true,
        ..SessionOptions::default()
    });

    run(session);
}

/// Draw the body and the bar until the user quits, returning so [`main`]'s
/// session restores the terminal.
fn run(mut session: ApcSession) {
    let live = session.live();
    let started = Instant::now();

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    terminal.clear().expect("clear the screen");

    let mut state = BarState {
        mode: 0,
        long_path: false,
        scale: 0,
    };

    loop {
        session.scene().clear();
        terminal
            .draw(|frame| render_frame(frame, session.scene(), &state, started, live))
            .expect("draw a frame");
        session.flush().expect("write the frame");

        // Polled rather than blocking, so the uptime segment ticks on its own.
        if !event::poll(Duration::from_millis(250)).expect("poll for an event") {
            continue;
        }
        match event::read().expect("read a terminal event") {
            // A resize re-packs both runs against the new width, which is the
            // one case the two rules take on their own.
            Event::Resize(_, _) => {},
            Event::Key(key) => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return;
                }
                match key.code {
                    KeyCode::Char('q') => return,
                    KeyCode::Char('m') => state.mode = (state.mode + 1) % MODES.len(),
                    KeyCode::Char('p') => state.long_path = !state.long_path,
                    KeyCode::Char('s') => state.scale = 1 - state.scale,
                    _ => {},
                }
            },
            _ => {},
        }
    }
}

/// What the keys have set on the bar.
struct BarState {
    /// Index into [`MODES`].
    mode: usize,
    /// Whether the file segment carries [`LONG_PATH`].
    long_path: bool,
    /// Index into [`SCALES`].
    scale: usize,
}

/// Draw the editor body, then the bar across the bottom row.
fn render_frame(
    frame: &mut Frame<'_>,
    scene: &mut ApcScene,
    state: &BarState,
    started: Instant,
    live: bool,
) {
    let area = frame.area();
    let bar = Rect {
        x: 0,
        y: area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };

    draw_body(frame, bar.y);

    let mode = &MODES[state.mode];
    let path = match state.long_path {
        true => LONG_PATH,
        false => SHORT_PATH,
    };
    let uptime = format!(
        " {:02}:{:02} ",
        started.elapsed().as_secs() / 60,
        started.elapsed().as_secs() % 60
    );

    let left = [
        StatusSegment {
            text: mode.label,
            fg: ON_COLOR,
            bg: mode.bg,
        },
        // On the bar's own background, so no box is painted and the hairline
        // reads through the segment.
        StatusSegment {
            text: path,
            fg: BODY_FG,
            bg: EDITOR_BG,
        },
        StatusSegment {
            text: " +12 -3 ",
            fg: ON_COLOR,
            bg: DIFF_BG,
        },
    ];
    let right = [
        StatusSegment {
            text: " Rust ",
            fg: ON_COLOR,
            bg: LANG_BG,
        },
        StatusSegment {
            text: " UTF-8 ",
            fg: BODY_FG,
            bg: EDITOR_BG,
        },
        StatusSegment {
            text: &uptime,
            fg: BODY_FG,
            bg: EDITOR_BG,
        },
    ];

    if !live {
        paint_fallback(frame, bar, &left, &right);
        return;
    }

    StatusBar {
        left: &left,
        right: &right,
        scale: SCALES[state.scale],
        separator: SEPARATOR,
        bg: EDITOR_BG,
    }
    .draw_components(bar, frame.buffer_mut(), scene);
}

/// Write the body lines and a caption above the bar.
fn draw_body(frame: &mut Frame<'_>, bar_y: u16) {
    let style = Style::default().fg(Color::Rgb(BODY_FG[0], BODY_FG[1], BODY_FG[2]));
    for (row, line) in BODY.iter().enumerate() {
        frame
            .buffer_mut()
            .set_stringn(2, 1 + row as u16, line, COLS as usize - 4, style);
    }

    let caption = "m: mode   p: long path   s: scale   q: quit";
    frame.buffer_mut().set_stringn(
        2,
        bar_y.saturating_sub(2),
        caption,
        COLS as usize - 4,
        style,
    );
}

/// Write the segment texts as plain cells, for a host that renders no
/// components.
///
/// The widget emits off-grid frames and writes no fallback of its own, so a
/// caller that wants one paints it. The right run is written from the right
/// edge inward, which is the order the bar packs it in.
fn paint_fallback(
    frame: &mut Frame<'_>,
    bar: Rect,
    left: &[StatusSegment<'_>],
    right: &[StatusSegment<'_>],
) {
    let style = Style::default().fg(Color::Rgb(BODY_FG[0], BODY_FG[1], BODY_FG[2]));

    let mut cursor = bar.x;
    for seg in left {
        let room = (bar.x + bar.width).saturating_sub(cursor) as usize;
        frame
            .buffer_mut()
            .set_stringn(cursor, bar.y, seg.text, room, style);
        cursor += seg.text.chars().count() as u16;
    }

    let mut anchor = bar.x + bar.width;
    for seg in right {
        let width = seg.text.chars().count() as u16;
        let start = anchor.saturating_sub(width);
        if start < cursor {
            continue;
        }
        frame
            .buffer_mut()
            .set_stringn(start, bar.y, seg.text, width as usize, style);
        anchor = start;
    }
}
