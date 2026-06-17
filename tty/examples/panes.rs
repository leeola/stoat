//! `cargo run --example panes` opens the stoatty window running the
//! `example_panes_app` emitter as its shell: a tiling editor that splits,
//! rearranges, and merges panes by redrawing their border frames at new
//! positions in discrete steps, each clearing the prior layout with a reset.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_panes_app");
}
