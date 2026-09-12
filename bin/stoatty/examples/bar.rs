//! `cargo run --example bar` opens the stoatty window running the
//! `example_bar_app` emitter as its shell. Sub-cell rectangles are laid at
//! every width over a checkerboard, at every sub-row offset as hairlines,
//! overhanging their area from a negative anchor, and sweeping a sixteenth of
//! a cell per frame.
//!
//! Press `q` to quit.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_bar_app", [72, 20]);
}
