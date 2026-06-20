//! An app-driven document-pool smooth-scroll stoatty demo: push a tall numbered
//! document into the recycled page pool, then drive the viewport down and back
//! to the top by reporting absolute scroll targets stoatty eases between.
//!
//! Each page is one viewport of numbered lines. The program first streams every
//! page into its pool slot with a `fill`/`fill_end` redirect pair, so the page
//! content lands off the live grid; it then loops emitting `Gstoatty;scroll`
//! targets, stepping the sub-page fraction across each page and advancing the
//! page index, so stoatty eases the live offset across the buffered pages.
//!
//! Until the renderer reads the pool at the live offset, document scroll is
//! invisible, so the loop also paints the current page onto the live grid as a
//! degradation view: today the page snaps per page with no easing; once the pool
//! read lands the same run eases smoothly and the live-grid paint is ignored.
//! Run as the PTY shell by the `smooth_scroll_pages` example.

use std::{
    io::{self, Write},
    thread,
    time::Duration,
};
use stoatty_protocol::command::{
    encode_fill_end_into, encode_fill_into, encode_scroll_into, ScrollCommand,
};

/// Viewport size in cells, matching the window the `pool_scroll` example opens.
const COLS: usize = 80;
const VIEWPORT_H: usize = 24;

/// Pages pushed into the pool. Kept at the pool's capacity so every page stays
/// buffered and no scroll target addresses an evicted slot.
const NUM_PAGES: u64 = 5;

/// Sub-page scroll targets emitted per page. Each step advances `fraction` by
/// `65536 / FRACTION_STEPS`, so a page eases in even increments.
const FRACTION_STEPS: u16 = 16;

/// Delay between scroll targets, so the eased offset trails each one smoothly.
const STEP_DELAY: Duration = Duration::from_millis(55);

/// Pause at the document's top and bottom before reversing direction.
const REST_DELAY: Duration = Duration::from_millis(700);

/// Editor background (`#282c34`) and foreground (`#abb2bf`), the One Dark colors
/// the default theme uses, set explicitly so the scene looks the same under any
/// theme.
const EDITOR_BG: [u8; 3] = [40, 44, 52];
const EDITOR_FG: [u8; 3] = [171, 178, 191];

/// Color of each page's header line, so page seams stay legible while scrolling.
const HEADER_FG: [u8; 3] = [97, 175, 239];

fn main() {
    let mut out = Vec::new();

    out.extend_from_slice(b"\x1b[?25l");
    for page in 0..NUM_PAGES {
        encode_fill_into(&mut out, page);
        write_page(&mut out, page);
        encode_fill_end_into(&mut out);
    }
    flush(&mut out);

    loop {
        scroll_through(&mut out, (0..NUM_PAGES).collect());
        scroll_through(&mut out, (0..NUM_PAGES).rev().collect());
    }
}

/// Drive the viewport across `pages` in order, emitting one degradation paint and
/// a sweep of sub-page scroll targets per page.
///
/// The last page in the sequence is the resting end of a sweep: its top is the
/// deepest valid offset (a deeper target would address an unbuffered page), so it
/// holds at `fraction` 0 for [`REST_DELAY`] rather than stepping further.
fn scroll_through(out: &mut Vec<u8>, pages: Vec<u64>) {
    let last = pages.len() - 1;

    for (position, page) in pages.into_iter().enumerate() {
        write_page(out, page);
        flush(out);

        if position == last {
            encode_scroll_into(out, &ScrollCommand { page, fraction: 0 });
            flush(out);
            thread::sleep(REST_DELAY);
            continue;
        }

        for step in 0..FRACTION_STEPS {
            let fraction = (u32::from(step) * 65536 / u32::from(FRACTION_STEPS)) as u16;
            encode_scroll_into(out, &ScrollCommand { page, fraction });
            flush(out);
            thread::sleep(STEP_DELAY);
        }
    }
}

/// Append a full viewport of one page's numbered lines, homing the cursor first
/// so the same bytes paint a fresh pool slot or repaint the live grid in place.
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
