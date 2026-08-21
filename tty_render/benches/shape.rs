//! Divan benchmark for text shaping alone, on the CPU, with no device.
//!
//! The compose benches all time a whole frame and wait for the GPU, and at the
//! sizes they draw that wait covers the CPU work, so a change that only moves
//! shaping cost does not move their numbers. This one never touches wgpu, so
//! what it reports is the shaping and nothing else.
//!
//! That number is what decides whether shaping is worth taking off the frame's
//! critical path. Moving it there changes what a frame shows before the shaping
//! it waited for arrives, so the cost has to be worth that.

use std::cell::Cell;
use stoatty_render::gpu::{build_font_system, shape_words};

/// Rows in a screenful, matching what the compose benches lay out at font size
/// 15 in a 1200 by 720 window.
const ROWS: usize = 33;

/// Cell size the app asks for, in logical pixels.
const FONT_SIZE: u32 = 15;

fn main() {
    divan::main();
}

/// A screenful of prose whose words nothing has shaped before.
///
/// The rows share every word but the number that leads them. Scrollback reads
/// the same way, with lines novel as wholes and almost entirely familiar as
/// words. A fling frame pays for the novel ones, so that is what this times.
#[divan::bench]
fn fresh_words(bencher: divan::Bencher<'_, '_>) {
    let mut font_system = build_font_system();
    // Shape one screen untimed. Cosmic caches the shape plan per face, and
    // building it is a startup cost rather than a per-frame one.
    for line in screen(0) {
        shape_words(&mut font_system, FONT_SIZE, &line);
    }

    let frame = Cell::new(1usize);
    bencher
        .with_inputs(|| {
            let at = frame.get();
            frame.set(at + 1);
            screen(at)
        })
        .bench_local_refs(|lines| {
            let glyphs: usize = lines
                .iter()
                .map(|line| shape_words(&mut font_system, FONT_SIZE, line))
                .sum();
            divan::black_box(glyphs);
        });
}

/// A screen of numbered source-like prose, as though `frame` screenfuls had
/// already scrolled past.
fn screen(frame: usize) -> Vec<String> {
    (0..ROWS)
        .map(|row| {
            format!(
                "line {:07} fn handle_event(ev) -> Result<()> {{ self.dispatch(ev)?; }}",
                frame * ROWS + row
            )
        })
        .collect()
}
