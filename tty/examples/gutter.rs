//! `cargo run --example gutter` opens the stoatty window running the
//! `example_gutter_app` emitter as its shell: an editor scene whose gutter packs
//! a smaller-than-grid line number, thin git and diagnostic color bars, and a
//! hairline separator into a few columns as off-grid components, while the code
//! stays on the uniform cell grid.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_gutter_app");
}
