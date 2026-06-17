//! `cargo run --example doc_tooltip` opens the stoatty window running the
//! `example_doc_tooltip_app` emitter as its shell: a code buffer with a rounded,
//! sub-cell-anchored documentation tooltip under the word beneath the cursor,
//! its larger-than-grid content scrolling inside the box.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_doc_tooltip_app");
}
