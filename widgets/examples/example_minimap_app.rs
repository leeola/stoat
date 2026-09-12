//! A minimap stoatty demo. A long document sits beside a strip that maps the
//! whole file onto a column of cells.
//!
//! The document is generated pseudo-code, summarized into the per-line run
//! summaries the [`Minimap`] widget's content store renders. The strip's thumb
//! tracks the viewport, a click on the strip jumps to the line under the
//! pointer, and the `d` and `i` keys splice lines out of and into the store so
//! the strip redraws from an edit rather than from a fresh declaration.
//!
//! Three lanes carry the mark. The strip itself is a per-frame declaration
//! through the scene. The line summaries and the viewport thumb are out-of-band
//! commands written straight to stdout after the scene flush, because both
//! outlive a frame. The store persists until it is dropped, and a redeclared
//! strip keeps whatever content it was pointed at.
//!
//! Run as the PTY shell by the `minimap` example.

use ratatui::{
    backend::CrosstermBackend,
    crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind},
    layout::Rect,
    style::Style,
    Frame, Terminal,
};
use std::{
    io::{self, Write},
    sync::Arc,
};
use stoat_widgets::{
    border::Border, minimap, minimap::Minimap, ApcScene, ApcSession, SessionOptions,
};
use stoatty_protocol::command::{
    encode_minimap_lines_into, encode_minimap_view_into, BorderStyle, LineSummary,
    MinimapLinesCommand, MinimapRun, MinimapViewCommand,
};

/// The window the `minimap` example opens, in cells.
const COLS: u16 = 96;
const ROWS: u16 = 40;

/// The strip's width in cells, over the right edge of the window.
const STRIP_W: u16 = 8;

/// The document pane's border, left of the strip.
const PANE: Rect = Rect {
    x: 0,
    y: 0,
    width: COLS - STRIP_W,
    height: ROWS,
};

/// The pane's interior, where the document text is drawn.
const TEXT: Rect = Rect {
    x: PANE.x + 1,
    y: PANE.y + 1,
    width: PANE.width - 2,
    height: PANE.height - 2,
};

/// The strip, over the rightmost columns and the full window height.
const STRIP: Rect = Rect {
    x: COLS - STRIP_W,
    y: 0,
    width: STRIP_W,
    height: ROWS,
};

/// Buffer lines the pane shows at once.
const VIEWPORT: u32 = TEXT.height as u32;

/// Lines in the generated document.
const LINES: usize = 3000;

/// Buffer lines the strip draws per vertical cell, which is what a pointer row
/// spans. Declared on the strip and passed to the click math, so the two agree.
const LINES_PER_CELL: u8 = 8;

/// Widest line, in minimap columns, the strip renders before clipping.
const MAX_COLUMNS: u8 = 120;

/// Lines one `d` or `i` splices out of or into the store.
const SPLICE: usize = 50;

/// The strip's run palette, indexed by a summary run's class.
///
/// One entry per class the demo's `summarize` produces: body text, a keyword, a
/// number, punctuation, and a comment.
const PALETTE: [[u8; 3]; 5] = [
    [171, 178, 191],
    [198, 120, 221],
    [209, 154, 102],
    [86, 182, 194],
    [92, 99, 112],
];

/// Words `summarize` calls out as keywords, which is what gives the strip its
/// colored texture rather than one flat class.
const KEYWORDS: [&str; 8] = [
    "fn", "let", "pub", "struct", "impl", "match", "return", "for",
];

/// The line shapes the document cycles through, each with its line number
/// substituted for `{}`.
///
/// Fixed rather than random so the strip reads the same on every run, which is
/// what makes a look worth refining against it.
const TEMPLATES: [&str; 12] = [
    "// summary of the block at line {}",
    "pub fn handler_{}(input: &str) -> Result<Frame> {{",
    "    let parsed = decode(input, {})?;",
    "    match parsed.kind {{",
    "        Kind::Header => header(parsed, {}),",
    "        Kind::Body => body(parsed),",
    "    }}",
    "",
    "struct Record{} {{",
    "    offset: usize,",
    "}}",
    "impl Record{} {{ pub fn len(&self) -> usize {{ self.offset }} }}",
];

fn main() {
    // The session holds raw mode, mouse reporting, and the hidden cursor for as
    // long as it lives, and gives all three back on the way out of main --
    // including the way out an `expect` below takes, which its panic hook
    // covers.
    let session = ApcSession::new(SessionOptions {
        raw_mode: true,
        mouse_capture: true,
        hide_cursor: true,
        ..SessionOptions::default()
    });

    run(session);
}

/// Scroll and splice the document under mouse and key control until the user
/// quits, returning so [`main`]'s session restores the terminal.
fn run(mut session: ApcSession) {
    let live = session.live();
    let mut document: Vec<String> = (0..LINES).map(document_line).collect();
    let mut top: u32 = 0;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("build the terminal");
    terminal.clear().expect("clear the screen");

    let draw = |session: &mut ApcSession, terminal: &mut Terminal<_>, document: &[String], top| {
        session.scene().clear();
        terminal
            .draw(|frame| render_frame(frame, session.scene(), document, top))
            .expect("draw a frame");
        session.flush().expect("write the frame");
    };

    draw(&mut session, &mut terminal, &document, top);
    if live {
        // The whole document at once. The encoder paginates it across as many
        // frames as the APC payload cap needs.
        write_commands(&MinimapLinesCommand {
            content_id: 1,
            start: 0,
            removed: 0,
            lines: document.iter().map(|line| summarize(line)).collect(),
        });
        write_view(top, document.len());
    }

    loop {
        let moved = match event::read().expect("read a terminal event") {
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => step(&mut top, 3, document.len()),
                MouseEventKind::ScrollUp => step(&mut top, -3, document.len()),
                MouseEventKind::Down(MouseButton::Left) if in_strip(mouse.column) => {
                    let line = minimap::click_target_line(
                        STRIP.height,
                        LINES_PER_CELL,
                        mouse.row - STRIP.y,
                        document.len() as f32,
                        top as f32,
                        VIEWPORT as f32,
                    );
                    let centered = i64::from(line) - i64::from(VIEWPORT) / 2;
                    jump(&mut top, centered, document.len())
                },
                _ => continue,
            },
            Event::Key(key) => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return;
                }
                match key.code {
                    KeyCode::Char('q') => return,
                    KeyCode::Up => step(&mut top, -1, document.len()),
                    KeyCode::Down => step(&mut top, 1, document.len()),
                    KeyCode::PageUp => step(&mut top, -(VIEWPORT as i64), document.len()),
                    KeyCode::PageDown => step(&mut top, VIEWPORT as i64, document.len()),
                    KeyCode::Home => jump(&mut top, 0, document.len()),
                    KeyCode::End => jump(&mut top, i64::MAX, document.len()),
                    KeyCode::Char('d') => {
                        let removed = splice_out(&mut document, top as usize);
                        if live && removed > 0 {
                            write_commands(&MinimapLinesCommand {
                                content_id: 1,
                                start: top,
                                removed: removed as u32,
                                lines: Vec::new(),
                            });
                        }
                        let at = i64::from(top);
                        jump(&mut top, at, document.len())
                    },
                    KeyCode::Char('i') => {
                        let inserted = splice_in(&mut document, top as usize);
                        if live {
                            write_commands(&MinimapLinesCommand {
                                content_id: 1,
                                start: top,
                                removed: 0,
                                lines: inserted.iter().map(|line| summarize(line)).collect(),
                            });
                        }
                        true
                    },
                    _ => continue,
                }
            },
            _ => continue,
        };

        draw(&mut session, &mut terminal, &document, top);
        if live && moved {
            write_view(top, document.len());
        }
    }
}

/// Move the viewport by `delta` lines, reporting whether it landed somewhere new.
fn step(top: &mut u32, delta: i64, total: usize) -> bool {
    jump(top, i64::from(*top) + delta, total)
}

/// Put the viewport's first line at `line`, clamped to the document, reporting
/// whether it landed somewhere new.
fn jump(top: &mut u32, line: i64, total: usize) -> bool {
    let last = (total as i64 - i64::from(VIEWPORT)).max(0);
    let landed = line.clamp(0, last) as u32;
    let moved = landed != *top;
    *top = landed;
    moved
}

/// Whether `column` falls on the strip.
fn in_strip(column: u16) -> bool {
    (STRIP.x..STRIP.x + STRIP.width).contains(&column)
}

/// Drop up to [`SPLICE`] lines at `at`, returning how many went.
fn splice_out(document: &mut Vec<String>, at: usize) -> usize {
    let end = (at + SPLICE).min(document.len());
    let removed = end.saturating_sub(at);
    document.drain(at..end);
    removed
}

/// Insert [`SPLICE`] generated lines at `at`, returning them so the caller
/// summarizes the same text the pane draws.
fn splice_in(document: &mut Vec<String>, at: usize) -> Vec<String> {
    let lines: Vec<String> = (at..at + SPLICE).map(document_line).collect();
    document.splice(at..at, lines.iter().cloned());
    lines
}

/// The text of document line `index`, which is a template from [`TEMPLATES`]
/// with the line number substituted in.
fn document_line(index: usize) -> String {
    TEMPLATES[index % TEMPLATES.len()].replace("{}", &index.to_string())
}

/// Summarize one line into the colored runs the strip draws.
///
/// A run is a stretch of non-whitespace in one class, capped at
/// [`MAX_COLUMNS`]. The twelfth run swallows the rest of the line, which bounds
/// what a pathological line costs the store without leaving its tail unpainted.
fn summarize(line: &str) -> LineSummary {
    const MAX_RUNS: usize = 12;
    let comment = line.trim_start().starts_with("//");

    let mut runs: Vec<MinimapRun> = Vec::new();
    let mut col = 0_usize;
    let mut start = None::<usize>;

    for ch in line.chars() {
        if col >= MAX_COLUMNS as usize {
            break;
        }
        match ch.is_whitespace() {
            true => {
                close_run(&mut runs, line, &mut start, col, comment);
            },
            false if start.is_none() => start = Some(col),
            false => {},
        }
        col += 1;
    }
    close_run(&mut runs, line, &mut start, col, comment);

    if runs.len() > MAX_RUNS {
        let tail = runs[MAX_RUNS - 1];
        let last = runs[runs.len() - 1];
        runs.truncate(MAX_RUNS - 1);
        runs.push(MinimapRun {
            start_col: tail.start_col,
            len: (last.start_col + last.len).saturating_sub(tail.start_col),
            class: tail.class,
            weight: 255,
        });
    }

    Arc::from(runs)
}

/// Close the run open at `start`, if any, as the columns up to `col`.
fn close_run(
    runs: &mut Vec<MinimapRun>,
    line: &str,
    start: &mut Option<usize>,
    col: usize,
    comment: bool,
) {
    let Some(from) = start.take() else {
        return;
    };
    let word: String = line.chars().skip(from).take(col - from).collect();
    runs.push(MinimapRun {
        start_col: from as u8,
        len: (col - from) as u8,
        class: class_of(&word, comment),
        weight: 255,
    });
}

/// The palette class a run of `word` draws in.
fn class_of(word: &str, comment: bool) -> u8 {
    if comment {
        return 4;
    }
    if KEYWORDS.contains(&word) {
        return 1;
    }
    if word.chars().all(|ch| ch.is_ascii_digit()) {
        return 2;
    }
    if word.chars().all(|ch| ch.is_ascii_punctuation()) {
        return 3;
    }
    0
}

/// Draw the pane border, the document window, and the strip declaration.
fn render_frame(frame: &mut Frame<'_>, scene: &mut ApcScene, document: &[String], top: u32) {
    frame.render_stateful_widget(
        Border {
            style: BorderStyle::Light,
            color: [120, 130, 150],
        },
        PANE,
        scene,
    );

    for row in 0..TEXT.height {
        let Some(line) = document.get(top as usize + row as usize) else {
            break;
        };
        frame.buffer_mut().set_stringn(
            TEXT.x,
            TEXT.y + row,
            line,
            TEXT.width as usize,
            Style::default(),
        );
    }

    frame.render_stateful_widget(
        Minimap {
            strip_id: 1,
            content_id: 1,
            lines_per_cell: LINES_PER_CELL,
            max_columns: MAX_COLUMNS,
            bg: [0, 0, 0, 0],
            thumb: [99, 109, 131, 64],
            thumb_border: [60, 66, 77],
            palette: &PALETTE,
        },
        STRIP,
        scene,
    );
}

/// Write a line splice to stdout, out of band from the scene.
fn write_commands(command: &MinimapLinesCommand) {
    let mut out = Vec::new();
    encode_minimap_lines_into(&mut out, command);
    write_out(&out);
}

/// Place the strip's thumb for a viewport at `top` over a `total`-line document.
fn write_view(top: u32, total: usize) {
    let mut out = Vec::new();
    encode_minimap_view_into(
        &mut out,
        &MinimapViewCommand {
            strip_id: 1,
            top_256: top.saturating_mul(256),
            visible_lines: VIEWPORT.min(total as u32) as u16,
        },
    );
    write_out(&out);
}

fn write_out(bytes: &[u8]) {
    let mut stdout = io::stdout();
    stdout.write_all(bytes).expect("write to stdout");
    stdout.flush().expect("flush stdout");
}
