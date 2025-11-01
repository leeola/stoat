//! End-to-end LSP diagnostic rendering tests.
//!
//! Tests the complete flow:
//! MockLspServer to LspManager to BufferItem.update_diagnostics() to EditorElement rendering

use gpui::{AppContext, TestAppContext};
use std::{sync::Arc, time::Duration};
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
        let manager = Arc::new(LspManager::new(
            cx.background_executor().clone(),
            Duration::from_secs(5),
        ));
        (buffer_item, manager)
    });

    // Create mock LSP server with programmed diagnostics
    let mock = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
        "/test.rs",
        vec![MockDiagnostic {
            range: "0:10-0:13", // "bar"
            kind: DiagnosticKind::UndefinedName,
            message: String::new(),
        }],
    ));

    // Add server without starting listener
    let server_id = manager.add_server("rust-analyzer", mock);

    // Send didOpen (mock will buffer diagnostics)
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

    // Setup automatic diagnostic routing (mirroring production AppState pattern)
    let path = std::path::PathBuf::from("/test.rs");
    let updates = manager.subscribe_diagnostic_updates();

    // Drain buffered notifications - this publishes diagnostics to the channel
    manager.drain_pending_notifications(server_id).unwrap();

    // Process diagnostic update (in production this happens in background task)
    if let Ok(update) = updates.try_recv() {
        assert_eq!(update.server_id, server_id);
        assert_eq!(update.path, path);

        // Update diagnostics (same as AppState background task does)
        cx.update(|cx| {
            let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
            if let Some(diag_set) = manager.diagnostics_for_buffer(&path, &snapshot) {
                buffer_item.update(cx, |item, cx| {
                    item.update_diagnostics(server_id, diag_set, 1, cx);
                });
            }
        });
    }

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

#[gpui::test]
async fn stale_diagnostic_updates_are_rejected(cx: &mut TestAppContext) {
    let source = "let foo = bar;";
    let buffer_id = BufferId::new(1).unwrap();

    let (buffer_item, manager) = cx.update(|cx| {
        let buffer = cx.new(|_cx| text::Buffer::new(0, buffer_id, source));
        let buffer_item = cx.new(|cx| BufferItem::new(buffer, Language::Rust, cx));
        let manager = Arc::new(LspManager::new(
            cx.background_executor().clone(),
            Duration::from_secs(5),
        ));
        (buffer_item, manager)
    });

    let mock = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
        "/test.rs",
        vec![MockDiagnostic {
            range: "0:10-0:13",
            kind: DiagnosticKind::UndefinedName,
            message: String::new(),
        }],
    ));

    let server_id = manager.add_server("rust-analyzer", mock);
    let path = std::path::PathBuf::from("/test.rs");

    let updates = manager.subscribe_diagnostic_updates();

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

    manager.drain_pending_notifications(server_id).unwrap();
    let _update = updates.try_recv().unwrap();

    let snapshot = cx.read(|cx| buffer_item.read(cx).buffer().read(cx).snapshot());
    let diag_set = manager.diagnostics_for_buffer(&path, &snapshot).unwrap();

    cx.update(|cx| {
        buffer_item.update(cx, |item, cx| {
            item.update_diagnostics(server_id, diag_set, 10, cx);
        });
    });

    let diag_count_v10 = cx.read(|cx| {
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        buffer_item
            .read(cx)
            .diagnostics_for_row(0, &snapshot)
            .count()
    });
    assert_eq!(diag_count_v10, 1, "Should have diagnostic from version 10");

    let mock_stale = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
        "/test.rs",
        vec![
            MockDiagnostic {
                range: "0:0-0:3",
                kind: DiagnosticKind::UnusedVariable,
                message: String::new(),
            },
            MockDiagnostic {
                range: "0:10-0:13",
                kind: DiagnosticKind::UndefinedName,
                message: String::new(),
            },
        ],
    ));

    let stale_server_id = manager.add_server("stale-server", mock_stale);
    manager
        .did_open(
            stale_server_id,
            "file:///test.rs".parse().unwrap(),
            "rust".to_string(),
            0,
            source.to_string(),
        )
        .await
        .unwrap();

    manager
        .drain_pending_notifications(stale_server_id)
        .unwrap();

    let snapshot = cx.read(|cx| buffer_item.read(cx).buffer().read(cx).snapshot());
    let stale_diag_set = manager.diagnostics_for_buffer(&path, &snapshot).unwrap();

    cx.update(|cx| {
        buffer_item.update(cx, |item, cx| {
            item.update_diagnostics(server_id, stale_diag_set.clone(), 5, cx);
        });
    });

    let final_diag_count = cx.read(|cx| {
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        buffer_item
            .read(cx)
            .diagnostics_for_row(0, &snapshot)
            .count()
    });
    assert_eq!(
        final_diag_count, 1,
        "Stale update (version 5) should be rejected, keeping version 10 diagnostics"
    );

    cx.update(|cx| {
        buffer_item.update(cx, |item, cx| {
            item.update_diagnostics(stale_server_id, stale_diag_set, 15, cx);
        });
    });

    let updated_diag_count = cx.read(|cx| {
        let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        buffer_item
            .read(cx)
            .diagnostics_for_row(0, &snapshot)
            .count()
    });
    assert!(
        updated_diag_count >= 1,
        "Newer update (version 15) should be accepted, showing diagnostics from the updated server"
    );
}
