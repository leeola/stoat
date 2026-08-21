//! Divan benchmarks for the terminal ingest path.
//!
//! Each case generates its byte stream once outside the timed body and feeds
//! it to a fresh [`Terminal`], so the number is the parse and the screen
//! updates alone. The three streams span the shapes a real child produces:
//! a scrolling flood, dense color changes, and alternate-screen redraws.
//!
//! `Terminal::advance` is windowless and deterministic, so these run anywhere.

use stoatty_term::{term::Terminal, theme::Theme};

/// Grid the streams are generated against, wide enough that a typical line
/// neither wraps nor leaves most of the row untouched.
const ROWS: usize = 50;
const COLS: usize = 200;

fn main() {
    divan::main();
}

fn terminal() -> Terminal {
    Terminal::new(ROWS, COLS, Theme::default())
}

/// Numbered lines past the bottom of the screen, which is what a build log or
/// a `cat` of a large file looks like. Every line scrolls the grid.
fn scroll_flood() -> Vec<u8> {
    (0..2000)
        .map(|i| format!("line {i:05} of the flood, scrolling past the bottom\r\n"))
        .collect::<String>()
        .into_bytes()
}

/// Color changes every few characters, which is what a syntax-highlighted
/// pager or a colored diff produces. The parse work is in the SGR runs rather
/// than in the text.
fn sgr_dense() -> Vec<u8> {
    let mut out = String::new();
    for line in 0..500 {
        for word in 0..12 {
            let fg = 31 + (word % 7);
            let bg = 40 + (line % 7);
            out.push_str(&format!("\x1b[{fg};{bg};1mword{word:02}\x1b[0m "));
        }
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// Full-screen redraws inside the alternate screen, each addressing every row
/// absolutely, which is what a TUI's repaint looks like. No line scrolls, so
/// the cost is cursor addressing and in-place cell writes.
fn alt_screen_motion() -> Vec<u8> {
    let mut out = String::from("\x1b[?1049h");
    for frame in 0..40 {
        out.push_str("\x1b[2J");
        for row in 1..=ROWS {
            out.push_str(&format!("\x1b[{row};1H"));
            out.push_str(&format!("row {row:03} frame {frame:03} redrawn in place"));
        }
    }
    out.push_str("\x1b[?1049l");
    out.into_bytes()
}

#[divan::bench]
fn scroll_flood_2k_lines(bencher: divan::Bencher<'_, '_>) {
    let bytes = scroll_flood();
    bencher
        .with_inputs(terminal)
        .bench_local_refs(|term| term.advance(&bytes));
}

#[divan::bench]
fn sgr_dense_500_lines(bencher: divan::Bencher<'_, '_>) {
    let bytes = sgr_dense();
    bencher
        .with_inputs(terminal)
        .bench_local_refs(|term| term.advance(&bytes));
}

#[divan::bench]
fn alt_screen_40_redraws(bencher: divan::Bencher<'_, '_>) {
    let bytes = alt_screen_motion();
    bencher
        .with_inputs(terminal)
        .bench_local_refs(|term| term.advance(&bytes));
}

/// The same flood split into 4 KiB chunks, the way a reader thread delivers it.
/// A per-chunk cost the whole-stream case hides shows up here.
#[divan::bench]
fn scroll_flood_in_reader_chunks(bencher: divan::Bencher<'_, '_>) {
    let bytes = scroll_flood();
    let chunks: Vec<&[u8]> = bytes.chunks(4096).collect();
    bencher.with_inputs(terminal).bench_local_refs(|term| {
        for chunk in &chunks {
            term.advance(chunk);
        }
    });
}
