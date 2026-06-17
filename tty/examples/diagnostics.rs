//! `cargo run --example diagnostics` opens the stoatty window running the
//! `example_diagnostics_app` emitter as its shell: an editor scene with
//! severity-colored curly underlines on an error and a warning span, and a
//! rounded tooltip hovering below the error span with a severity icon and
//! message -- the VS Code hover-on-error look.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_diagnostics_app");
}
