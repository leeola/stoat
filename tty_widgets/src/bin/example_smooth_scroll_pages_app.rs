//! An interactive document-pool smooth-scroll stoatty demo: the mouse wheel
//! scrolls a tall numbered document the program streams into the recycled page
//! pool, and stoatty eases the live offset toward each reported target.
//!
//! Each page is one viewport of numbered lines, pushed into its pool slot with a
//! `fill`/`fill_end` redirect pair so the content lands off the live grid. The
//! program keeps a window of pages buffered around the cursor and, on each wheel
//! notch, advances a fractional document position by a few rows and emits a
//! `Gstoatty;scroll` target; stoatty reads the visible region from the pool at
//! that offset and glides to it at sub-cell granularity.
//!
//! Runs in raw mode with mouse reporting on, so stoatty forwards the wheel as SGR
//! mouse reports the event loop consumes; `q` or Ctrl-C quits. In any other
//! terminal the `Gstoatty` frames are ignored and the wheel does nothing. Run as
//! the PTY shell by the `smooth_scroll_pages` example.

use ratatui::crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::io::{self, Write};
use stoatty_protocol::command::{
    encode_fill_end_into, encode_fill_into, encode_scroll_into, ScrollCommand,
};

/// Viewport size in cells, matching the window the `smooth_scroll_pages` example
/// opens.
const COLS: usize = 80;
const VIEWPORT_H: usize = 24;

/// Pages kept buffered around the cursor, the pool's capacity, so the visible
/// region and its straddle neighbour are always present.
const WINDOW_PAGES: u64 = 5;

/// Rows a single wheel notch scrolls. Kept a sub-page step so the wheel nudges
/// the document a few lines at a time and stoatty eases across them.
const STEP_ROWS: f32 = 3.0;

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so the scene looks the same under any
/// theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Color of each page's header line, so page seams stay legible while scrolling.
const HEADER_FG: [u8; 3] = [97, 175, 239];

fn main() {
    enable_raw_mode().expect("enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, EnableMouseCapture).expect("enable mouse capture");
    let _ = stdout.write_all(b"\x1b[?25l");
    let _ = stdout.flush();

    run();

    let _ = execute!(stdout, DisableMouseCapture);
    let _ = stdout.write_all(b"\x1b[?25h");
    let _ = stdout.flush();
    disable_raw_mode().ok();
}

/// Scroll the document under wheel control until the user quits, returning so
/// [`main`] can restore the terminal.
fn run() {
    let step = STEP_ROWS / VIEWPORT_H as f32;
    let mut out = Vec::new();
    let mut position = 0.0_f32;
    let mut window_start = None;

    refill_window(&mut out, position, &mut window_start);
    emit_scroll(&mut out, position);
    flush(&mut out);

    loop {
        match event::read().expect("read a terminal event") {
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => position += step,
                MouseEventKind::ScrollUp => position = (position - step).max(0.0),
                _ => continue,
            },
            Event::Key(key) => {
                let quit = key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    break;
                }
                continue;
            },
            _ => continue,
        }

        refill_window(&mut out, position, &mut window_start);
        emit_scroll(&mut out, position);
        flush(&mut out);
    }
}

/// Buffer the pool window around `position`, refilling only when the integer page
/// changes so a sub-page move reuses the already-buffered pages.
fn refill_window(out: &mut Vec<u8>, position: f32, window_start: &mut Option<u64>) {
    let start = (position as u64).saturating_sub(1);
    if *window_start == Some(start) {
        return;
    }
    *window_start = Some(start);

    for page in start..start + WINDOW_PAGES {
        encode_fill_into(out, page);
        write_page(out, page);
        encode_fill_end_into(out);
    }
}

/// Emit the smooth-scroll target for `position`, split into a page index and a
/// sub-page fraction in 1/65536ths of a page.
fn emit_scroll(out: &mut Vec<u8>, position: f32) {
    let page = position.floor();
    let fraction = ((position - page) * 65536.0) as u16;
    encode_scroll_into(
        out,
        &ScrollCommand {
            page: page as u64,
            fraction,
        },
    );
}

/// Append a full viewport of one page's numbered lines, homing the cursor first
/// so the bytes paint a fresh pool slot.
fn write_page(out: &mut Vec<u8>, page: u64) {
    out.extend_from_slice(b"\x1b[H");

    for row in 0..VIEWPORT_H {
        let line = page as usize * VIEWPORT_H + row + 1;
        if row == 0 {
            let first = page as usize * VIEWPORT_H + 1;
            let last = first + VIEWPORT_H - 1;
            write_line(
                out,
                HEADER_FG,
                &format!("PAGE {page}  (lines {first}-{last})"),
            );
        } else {
            write_line(
                out,
                EDITOR_FG,
                &format!("{line:>4} | pooled document line {line}"),
            );
        }

        if row + 1 < VIEWPORT_H {
            out.extend_from_slice(b"\r\n");
        }
    }
}

/// Append one row of `text` in `fg` over the editor background, padded to [`COLS`]
/// so it overwrites whatever the row held before.
fn write_line(out: &mut Vec<u8>, fg: [u8; 3], text: &str) {
    let _ = write!(
        out,
        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
        fg[0], fg[1], fg[2], EDITOR_BG[0], EDITOR_BG[1], EDITOR_BG[2],
    );

    let mut text = text.to_string();
    text.truncate(COLS);
    let _ = write!(out, "{text:<COLS$}");

    out.extend_from_slice(b"\x1b[0m");
}

/// Write the accumulated bytes to stdout and clear the buffer for the next batch.
fn flush(out: &mut Vec<u8>) {
    let mut stdout = io::stdout();
    stdout.write_all(out).expect("write to stdout");
    stdout.flush().expect("flush stdout");
    out.clear();
}
