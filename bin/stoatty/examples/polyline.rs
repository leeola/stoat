//! `cargo run --example polyline` opens the stoatty window running the
//! `example_polyline_app` emitter as its shell. A commit-graph figure of lanes,
//! dots, and one merge edge sits beside a canvas the pointer draws on.
//!
//! Click the canvas to add a vertex. Press `n` for a new path, `w` and `W` to
//! step the stroke width, `a` to toggle a forty-point sine that animates past
//! the twelve points one instance carries, `c` to clear, and `q` to quit.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_polyline_app", [90, 28]);
}
