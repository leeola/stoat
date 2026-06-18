//! `cargo run --example edit` opens the stoatty window running the
//! `example_edit_app` emitter as its shell: a plain text-editing scene where the
//! cursor walks each line as code is typed, a typo is backspaced and retyped, and
//! the buffer scrolls once the lines fill the screen -- pure VT exercising the
//! cursor easing and whole-grid eased scroll.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_edit_app", [80, 24]);
}
