//! `cargo run --example hello` opens the stoatty window running the
//! `example_hello_app` emitter as its shell, the minimal end-to-end proof of
//! the bytes to PTY to parse to grid to render path.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_hello_app");
}
