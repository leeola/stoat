//! `cargo run --example image` opens the stoatty window running the
//! `example_image_app` emitter as its shell, which transmits a gradient as a
//! Kitty graphics image and holds it on screen.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_image_app", [80, 24]);
}
