//! `cargo run --example smooth_scroll_pages` opens the stoatty window running
//! the `example_smooth_scroll_pages_app` emitter as its shell: a tall
//! numbered document streamed into the recycled page pool, then driven down and
//! back up by absolute scroll targets stoatty eases between, exercising the
//! app-pushed document-pool smooth-scroll path.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_smooth_scroll_pages_app", [80, 24]);
}
