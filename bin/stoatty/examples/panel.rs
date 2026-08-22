//! `cargo run --example panel` opens the stoatty window running the
//! `example_panel_app` emitter as its shell. It draws a centered modal dialog as
//! off-grid chrome, a hairline rounded frame with a drop shadow and a title on
//! its top edge, floating over the editor cells.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_panel_app", [60, 20]);
}
