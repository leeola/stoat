#![cfg(feature = "dev-tools")]

use gpui::TestAppContext;
use std::time::Duration;
use stoat::test::headless::HeadlessStoat;

/// Retry LspHover until hover blocks appear, to handle rust-analyzer indexing delay.
async fn hover_with_retry(app: &mut HeadlessStoat<'_>, timeout: Duration) {
    let start = std::time::Instant::now();
    loop {
        app.type_action("LspHover");
        app.await_flash_message(Duration::from_secs(5)).await;

        if app.hover_visible() && !app.hover_blocks().is_empty() {
            return;
        }

        if start.elapsed() >= timeout {
            panic!(
                "hover_with_retry timed out after {timeout:?}. hover_visible={}, blocks={:?}, flash={:?}",
                app.hover_visible(),
                app.hover_blocks(),
                app.flash_message(),
            );
        }

        // Wait before retrying (rust-analyzer may still be indexing)
        app.sleep(Duration::from_secs(1)).await;
    }
}

#[gpui::test]
async fn hover_with_real_rust_analyzer(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("rust-lsp", cx);

    // Re-load to trigger FileOpened -> ensure_lsp_for_language
    // (initial load during PaneGroupView::new fires before subscriptions exist)
    app.load_file("src/main.rs");

    app.await_lsp_ready(Duration::from_secs(30)).await;

    // Move cursor to `Greeter` on line 1: j(down) w(word) w(word)
    app.type_input("jww");

    hover_with_retry(&mut app, Duration::from_secs(30)).await;

    let blocks = app.hover_blocks();
    assert!(!blocks.is_empty(), "Expected hover blocks");

    // Type signature present
    assert!(
        blocks.iter().any(|b| b.text.contains("Greeter")),
        "Expected hover to mention 'Greeter', got: {blocks:?}"
    );

    // Structured content preserved (type signature in text)
    assert!(
        blocks.iter().any(|b| b.text.contains("pub struct Greeter")),
        "Expected type signature 'pub struct Greeter', got: {blocks:?}"
    );

    // Doc comment present
    assert!(
        blocks
            .iter()
            .any(|b| b.text.to_lowercase().contains("greeter")),
        "Expected doc comment mentioning 'greeter', got: {blocks:?}"
    );

    // Flash message still emitted as summary
    let msg = app
        .flash_message()
        .expect("Expected flash message from hover");
    assert!(!msg.is_empty(), "Expected non-empty flash summary");
}

#[gpui::test]
async fn hover_method_docs(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("rust-lsp", cx);

    app.load_file("src/main.rs");
    app.await_lsp_ready(Duration::from_secs(30)).await;

    // Move cursor to `.greet()` call on line 22 (0-indexed line 21)
    // Line: "    let msg = g.greet();"
    // Navigate: 21j (down 21 lines), f. (find dot), w (to 'greet')
    app.type_input("21jf.w");

    hover_with_retry(&mut app, Duration::from_secs(30)).await;

    let blocks = app.hover_blocks();
    assert!(!blocks.is_empty(), "Expected hover blocks for greet()");

    // Method signature present
    assert!(
        blocks.iter().any(|b| b.text.contains("greet")),
        "Expected hover to mention 'greet', got: {blocks:?}"
    );

    // Doc comment present
    assert!(
        blocks
            .iter()
            .any(|b| b.text.to_lowercase().contains("greeting")),
        "Expected doc comment about greeting, got: {blocks:?}"
    );
}
