//! `cargo run --example scrollback` opens the stoatty window running the
//! `example_scrollback_app` emitter as its shell: a long numbered document
//! printed into the scrollback with periodic seam banners, then held so the
//! history persists for mouse-wheel scrollback, exercising the terminal's own
//! eased scrollback axis.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_scrollback_app", [80, 24]);
}
