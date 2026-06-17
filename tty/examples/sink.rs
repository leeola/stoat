//! `cargo run --example sink` opens the stoatty window running the
//! `example_sink_app` emitter as its shell, rendering its baseline TUI end to
//! end through the bytes to PTY to parse to grid to render path.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_sink_app");
}
