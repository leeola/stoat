//! `cargo run --example panel` opens the stoatty window running the
//! `example_panel_app` emitter as its shell. A grid draws every shadow style
//! against every border weight, with the corner radius, the fill, and the
//! horizontal inset varying across it.
//!
//! The scene is static. Close the window to end it.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_panel_app", [96, 36]);
}
