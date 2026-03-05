#![cfg(feature = "dev-tools")]

use gpui::TestAppContext;
use std::time::Duration;
use stoat::test::headless::HeadlessStoat;

#[gpui::test]
async fn hover_with_real_rust_analyzer(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("rust-lsp", cx);

    // Re-load to trigger FileOpened -> ensure_lsp_for_language
    // (initial load during PaneGroupView::new fires before subscriptions exist)
    app.load_file("src/main.rs");

    app.await_lsp_ready(Duration::from_secs(30)).await;

    // Move cursor to `Greeter` on line 1: j(down) w(word) w(word)
    app.type_input("jww");

    app.type_action("LspHover");

    app.await_flash_message(Duration::from_secs(10)).await;

    let msg = app
        .flash_message()
        .expect("Expected flash message from hover");
    assert!(
        msg.contains("Greeter"),
        "Expected hover to mention 'Greeter', got: {msg}"
    );
}
