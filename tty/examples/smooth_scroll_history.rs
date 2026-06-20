//! `cargo run --example smooth_scroll_history` opens the stoatty window running
//! the `example_smooth_scroll_history_app` emitter as its shell: a long
//! numbered document printed into the scrollback with periodic seam banners,
//! then held so the history persists for mouse-wheel scrollback, exercising the
//! terminal's own eased scrollback axis.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_smooth_scroll_history_app", [80, 24]);
}
