//! `cargo run --example scale` opens the stoatty window running the
//! `example_scale_app` emitter as its shell: one glyph drawn at 1x, 2x, and 4x
//! cell size, each scaled glyph claiming the integer cell block it occupies.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_scale_app", [52, 12]);
}
