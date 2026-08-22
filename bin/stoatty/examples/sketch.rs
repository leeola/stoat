//! `cargo run --example sketch` opens the stoatty window running the
//! `example_sketch_app` emitter as its shell. It draws code with hand-drawn
//! marks over it: a circle closing around one identifier, a curved connector
//! growing to a filled box, and labels fading in as the marks they name finish.
//!
//! Press `r` to replay the scene from the start, `q` to quit.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_sketch_app", [88, 20]);
}
