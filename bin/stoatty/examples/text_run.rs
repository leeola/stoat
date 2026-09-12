//! `cargo run --example text_run` opens the stoatty window running the
//! `example_text_run_app` emitter as its shell. One string is drawn across a
//! scale ramp, boxed beside box-less over a checkerboard, and at sub-cell
//! nudges over drawn cell edges.
//!
//! Press `+` and `-` to step the live run's scale, and `q` to quit.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_text_run_app", [80, 30]);
}
