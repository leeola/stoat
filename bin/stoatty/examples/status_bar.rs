//! `cargo run --example status_bar` opens the stoatty window running the
//! `example_status_bar_app` emitter as its shell. An editor-colored body sits
//! above a bar of scaled segments packed from both edges.
//!
//! Press `m` to cycle the mode segment, `p` to swap in a path long enough that
//! the left run clips and the right run drops, `s` to step the glyph scale, and
//! `q` to quit.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_status_bar_app", [80, 12]);
}
