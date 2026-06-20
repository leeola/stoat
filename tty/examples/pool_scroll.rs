//! `cargo run --example pool_scroll` opens the stoatty window running the
//! `example_pool_scroll_app` emitter as its shell: a tall numbered document
//! streamed into the recycled page pool, then driven down and back up by
//! absolute scroll targets stoatty eases between, exercising the app-pushed
//! document-pool smooth-scroll path.

#[path = "common/launcher.rs"]
mod launcher;

fn main() {
    launcher::run("example_pool_scroll_app", [80, 24]);
}
