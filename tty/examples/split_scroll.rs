//! `cargo run --example split_scroll` opens the stoatty window running the
//! `example_split_scroll_app` emitter as its shell: a fixed sidebar beside a
//! tall buffer whose viewport chases the cursor via the per-region scroll
//! command, showing the eased per-region scroll against a fixed neighbor.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_split_scroll_app", [60, 16]);
}
