//! Divan benchmark for text shaping alone, on the CPU, with no device.
//!
//! The compose benches report a whole frame's CPU, of which shaping is one
//! part. This one never touches wgpu, so what it reports is the shaping alone,
//! which is what says how much of that frame a change to shaping moves.
//!
//! Two numbers come out, because two screens shape very differently.
//! `fresh_words` bounds a screen whose every word is novel, which a hexdump or
//! a stream of ids is. `warm_words` holds the run cache the renderer holds, so
//! it reports what prose scrolling past actually shapes. A line introduces a
//! few words and repeats the rest, and the repeats are cache hits.
//!
//! `warm_words` is the one to weigh against taking shaping off the frame's
//! critical path, since that is what the path pays. Moving it there changes
//! what a frame shows before the shaping it waited for arrives, so the cost has
//! to be worth that.

use std::cell::Cell;
use stoatty_render::gpu::{build_font_system, shape_words, shape_words_cached, RunShapeCache};

/// Rows in a screenful, matching what the compose benches lay out at font size
/// 15 in a 1200 by 720 window.
const ROWS: usize = 33;

/// Cell size the app asks for, in logical pixels.
const FONT_SIZE: u32 = 15;

fn main() {
    divan::main();
}

/// A screenful shaped with every word novel, which bounds the cost above.
///
/// No cache, so the rows' shared words are shaped once per row rather than
/// once. A screen that really is all novel words reads this way, such as a
/// hexdump or a stream of ids. Prose does not. See `warm_words` for a fling,
/// which comes out an order of magnitude cheaper.
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

/// A screenful of scrolling prose against the cache the renderer holds, which
/// is what a fling costs.
///
/// The rows share every word but the number that leads them. Scrollback reads
/// the same way, with lines novel as wholes and almost entirely familiar as
/// words. So the cache is held across iterations, exactly as the text pass
/// holds one across frames, and each screen shapes one word per row.
#[divan::bench]
fn warm_words(bencher: divan::Bencher<'_, '_>) {
    let mut font_system = build_font_system();
    let mut cache = RunShapeCache::default();
    // Shape one screen untimed, which both builds cosmic's per-face shape plan
    // and fills the cache with the words every later screen repeats.
    for line in screen(0) {
        shape_words_cached(&mut cache, &mut font_system, FONT_SIZE, &line);
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
                .map(|line| shape_words_cached(&mut cache, &mut font_system, FONT_SIZE, line))
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
