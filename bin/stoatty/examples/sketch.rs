//! `cargo run --example sketch` opens the stoatty window running the
//! `example_sketch_app` emitter as its shell. Hand-drawn marks annotate code. A
//! circle closes around one identifier, a curved connector grows to a filled
//! box, and labels fade in as the marks they name finish.
//!
//! Under the code, a band varies one stroke knob per row -- roughness, width,
//! and alpha -- and a last row draws the three reveal easings over one span.
//!
//! Press `r` to replay from the start, `x` to replay in the exit phase so the
//! scene un-draws itself, and `q` to quit.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_sketch_app", [88, 28]);
}
