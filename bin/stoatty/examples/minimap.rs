//! `cargo run --example minimap` opens the stoatty window running the
//! `example_minimap_app` emitter as its shell. A 3000-line document sits beside
//! a strip that maps the whole file onto a column of cells, with the thumb
//! tracking the viewport.
//!
//! Scroll with the wheel or the arrow, page, home, and end keys. Click the
//! strip to jump to that line. Press `d` and `i` to splice lines out of and
//! into the content store, which redraws the strip from an edit rather than
//! from a fresh declaration. Press `q` to quit.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_minimap_app", [96, 40]);
}
