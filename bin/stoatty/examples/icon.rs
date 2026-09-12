//! `cargo run --example icon` opens the stoatty window running the
//! `example_icon_app` emitter as its shell. A grid puts every icon kind across
//! sizes 1 through 4, and a last row draws one kind at pixel offsets over drawn
//! cell edges.
//!
//! The scene is static. Close the window to end it.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_icon_app", [60, 22]);
}
