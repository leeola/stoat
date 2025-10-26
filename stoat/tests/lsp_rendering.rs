//! End-to-end LSP diagnostic rendering tests.
//!
//! Tests the complete flow:
//! MockLspServer to LspManager to BufferItem.update_diagnostics() to EditorElement rendering

use gpui::{AppContext, TestAppContext};
use std::sync::Arc;
use stoat::buffer::BufferItem;
use stoat_lsp::{
    test::{DiagnosticKind, MockDiagnostic, MockLspServer},
    LspManager,
};
use stoat_text::Language;
use text::BufferId;

#[gpui::test]
async fn diagnostic_flow_end_to_end(cx: &mut TestAppContext) {
    let source = "let foo = bar;";
    let buffer_id = BufferId::new(1).unwrap();

    // Create buffer and manager in sync context
    let (buffer_item, manager) = cx.update(|cx| {
        let buffer = cx.new(|cx| text::Buffer::new(0, buffer_id, source));
        let buffer_item = cx.new(|cx| BufferItem::new(buffer, Language::Rust, cx));
        let manager = Arc::new(LspManager::new());
        (buffer_item, manager)
    });

    // Create mock LSP server with programmed diagnostics
    let mock = Arc::new(
        MockLspServer::rust_analyzer().with_diagnostics(
            "/test.rs",
            vec![MockDiagnostic {
                range: "0:10-0:13", // "bar"
                kind: DiagnosticKind::UndefinedName,
                message: String::new(),
            }],
        ),
    );

    // Spawn server and send didOpen (async operations)
    let server_id = manager.spawn_server("rust-analyzer", mock).await.unwrap();
    manager
        .did_open(
            server_id,
            "file:///test.rs".parse().unwrap(),
            "rust".to_string(),
            0,
            source.to_string(),
        )
        .await
        .unwrap();

    // Wait for diagnostics to be processed
    smol::Timer::after(std::time::Duration::from_millis(100)).await;

    // Update buffer with diagnostics (sync context)
    let path = std::path::PathBuf::from("/test.rs");
    cx.update(|cx| {
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        if let Some(diag_set) = manager.diagnostics_for_buffer(&path, &snapshot) {
            buffer_item.update(cx, |item, cx| {
                item.update_diagnostics(server_id, diag_set, cx);
            });
        }
    });

    // Verify diagnostics were stored
    let (has_diagnostics, diag_count) = cx.read(|cx| {
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let diags: Vec<_> = buffer_item
            .read(cx)
            .diagnostics_for_row(0, &snapshot)
            .collect();
        (!diags.is_empty(), diags.len())
    });

    assert!(has_diagnostics, "BufferItem should have diagnostics");
    assert_eq!(diag_count, 1, "Should have exactly 1 diagnostic");

    // Verify diagnostic properties
    let snapshot = cx.read(|cx| buffer_item.read(cx).buffer().read(cx).snapshot());
    let diagnostic = cx.read(|cx| {
        buffer_item
            .read(cx)
            .diagnostics_for_row(0, &snapshot)
            .next()
            .cloned()
    });

    let diag = diagnostic.expect("Should have a diagnostic on row 0");
    assert_eq!(diag.severity, stoat_lsp::DiagnosticSeverity::Error);
    assert!(diag.message.contains("cannot find value"));
    assert_eq!(diag.server_id, server_id);

    // Verify anchor positions
    use text::ToPoint;
    let snapshot = cx.read(|cx| buffer_item.read(cx).buffer().read(cx).snapshot());

    let start = diag.range.start.to_point(&snapshot);
    let end = diag.range.end.to_point(&snapshot);
    assert_eq!(start.row, 0);
    assert_eq!(start.column, 10);
    assert_eq!(end.row, 0);
    assert_eq!(end.column, 13);
}
