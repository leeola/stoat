//! An interactive document-pool smooth-scroll stoatty demo: the mouse wheel
//! scrolls a tall numbered document the program streams into the recycled page
//! pool, and stoatty eases the live offset toward each reported target.
//!
//! The pool occupies a declared sub-rectangle (`Gstoatty;pool_region`), not the
//! whole viewport: a reversed title bar on the top row, a status line on the
//! bottom row, and a left border down the side are written once to the live grid
//! as ordinary VT and stay fixed while the pooled rows scroll beneath them. Each
//! page is one region of numbered lines, pushed into its pool slot with a
//! `fill`/`fill_end` redirect pair so the content lands off the live grid. The
//! program keeps a window of pages buffered around the cursor and, on each wheel
//! notch, advances a fractional document position by a few rows and emits a
//! `Gstoatty;scroll` target; stoatty reads the visible region from the pool at
//! that offset and glides to it at sub-cell granularity.
//!
//! Runs in raw mode with mouse reporting on, so stoatty forwards the wheel as SGR
//! mouse reports the event loop consumes. Ctrl-F and Ctrl-B skip a whole page at
//! a time, like a pager, to cover the document far faster than the wheel; `q` or
//! Ctrl-C quits. In any other terminal the `Gstoatty` frames are ignored and the
//! static chrome renders as a plain framed screen. Run as the PTY shell by the
//! `smooth_scroll_pages` example.

use ratatui::crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::io::{self, Write};
use stoatty_protocol::command::{
    encode_fill_end_into, encode_fill_into, encode_pool_region_into, encode_scroll_into,
    PoolRegionCommand, ScrollCommand,
};

/// Viewport size in cells, matching the window the `smooth_scroll_pages` example
/// opens.
const COLS: usize = 80;
const VIEWPORT_H: usize = 24;

/// The pooled sub-rectangle inside the viewport: a one-row header above and a
/// one-row footer below, with a two-column left margin for the border, so the
/// pool scrolls within a frame of static chrome rather than the whole screen.
const REGION_TOP: u16 = 1;
const REGION_LEFT: u16 = 2;
const REGION_W: usize = 76;
const REGION_H: usize = 22;

/// Pages kept buffered around the cursor, the pool's capacity, so the visible
/// region and its straddle neighbour are always present.
const WINDOW_PAGES: u64 = 5;

/// Rows a single wheel notch scrolls. Kept a sub-page step so the wheel nudges
/// the document a few lines at a time and stoatty eases across them.
const STEP_ROWS: f32 = 3.0;

/// Pages a single Ctrl-F / Ctrl-B press skips, a full region like a pager's
/// page key, so the document scrolls far faster than the wheel's [`STEP_ROWS`].
const PAGE_STEP: f32 = 1.0;

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so the scene looks the same under any
/// theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Color of each page's header line, so page seams stay legible while scrolling.
const HEADER_FG: [u8; 3] = [97, 175, 239];

/// Chrome foreground (`#e5c07b`, One Dark yellow) for the title bar, footer, and
/// border, so the static frame reads distinctly from the scrolling body.
const CHROME_FG: [u8; 3] = [229, 192, 123];

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
    let step = STEP_ROWS / REGION_H as f32;
    let mut out = Vec::new();
    let mut position = 0.0_f32;
    let mut window_start = None;

    write_chrome(&mut out);
    encode_pool_region_into(
        &mut out,
        &PoolRegionCommand {
            top: REGION_TOP,
            left: REGION_LEFT,
            width: REGION_W as u16,
            height: REGION_H as u16,
        },
    );

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
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if ctrl => break,
                    // Page forward and back a whole region at a time, so the
                    // document covers far faster than the wheel; stoatty eases
                    // across each page.
                    KeyCode::Char('f') if ctrl => position += PAGE_STEP,
                    KeyCode::Char('b') if ctrl => position = (position - PAGE_STEP).max(0.0),
                    _ => continue,
                }
            },
            _ => continue,
        }

        refill_window(&mut out, position, &mut window_start);
        emit_scroll(&mut out, position);
        flush(&mut out);
    }
}

/// Paint the static frame around the pool region: a reversed title bar on the top
/// row, a status line on the bottom row, and a vertical border down the column
/// left of the region.
///
/// Ordinary VT writes to the live grid, so they stay fixed while the pooled rows
/// scroll inside the region and degrade to a plain framed screen in any other
/// terminal.
fn write_chrome(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[H");
    let _ = write!(
        out,
        "\x1b[7;38;2;{};{};{}m",
        CHROME_FG[0], CHROME_FG[1], CHROME_FG[2],
    );
    let title = " stoatty pool-region smooth scroll  (wheel / Ctrl-F / Ctrl-B, q to quit) ";
    let _ = write!(out, "{title:<COLS$}");
    out.extend_from_slice(b"\x1b[0m");

    for r in 0..REGION_H {
        let row = REGION_TOP as usize + r + 1;
        let _ = write!(
            out,
            "\x1b[{};{}H\x1b[38;2;{};{};{}m\u{2502}\x1b[0m",
            row, REGION_LEFT, CHROME_FG[0], CHROME_FG[1], CHROME_FG[2],
        );
    }

    let _ = write!(
        out,
        "\x1b[{};1H\x1b[38;2;{};{};{};48;2;{};{};{}m",
        VIEWPORT_H,
        CHROME_FG[0],
        CHROME_FG[1],
        CHROME_FG[2],
        EDITOR_BG[0],
        EDITOR_BG[1],
        EDITOR_BG[2],
    );
    let footer = " ready ";
    let _ = write!(out, "{footer:<COLS$}");
    out.extend_from_slice(b"\x1b[0m");
}

/// Buffer the pool window centered on `position`, refilling only when the integer
/// page changes so a sub-page move reuses the already-buffered pages.
///
/// Centering leaves pages buffered on both sides of the target, so a Ctrl-F /
/// Ctrl-B page jump stays covered while stoatty's ease lags behind it (a forward
/// jump leaves the lagging rows below buffered, a backward one the rows above).
fn refill_window(out: &mut Vec<u8>, position: f32, window_start: &mut Option<u64>) {
    let start = (position as u64).saturating_sub(WINDOW_PAGES / 2);
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

/// Append one region of a page's numbered lines, homing the cursor first so the
/// bytes paint a fresh pool slot sized to the region rather than the viewport.
fn write_page(out: &mut Vec<u8>, page: u64) {
    out.extend_from_slice(b"\x1b[H");

    for row in 0..REGION_H {
        let line = page as usize * REGION_H + row + 1;
        if row == 0 {
            let first = page as usize * REGION_H + 1;
            let last = first + REGION_H - 1;
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

        if row + 1 < REGION_H {
            out.extend_from_slice(b"\r\n");
        }
    }
}

/// Append one row of `text` in `fg` over the editor background, padded to
/// [`REGION_W`] so it overwrites whatever the row held before.
fn write_line(out: &mut Vec<u8>, fg: [u8; 3], text: &str) {
    let _ = write!(
        out,
        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
        fg[0], fg[1], fg[2], EDITOR_BG[0], EDITOR_BG[1], EDITOR_BG[2],
    );

    let mut text = text.to_string();
    text.truncate(REGION_W);
    let _ = write!(out, "{text:<REGION_W$}");

    out.extend_from_slice(b"\x1b[0m");
}

/// Write the accumulated bytes to stdout and clear the buffer for the next batch.
fn flush(out: &mut Vec<u8>) {
    let mut stdout = io::stdout();
    stdout.write_all(out).expect("write to stdout");
    stdout.flush().expect("flush stdout");
    out.clear();
}
