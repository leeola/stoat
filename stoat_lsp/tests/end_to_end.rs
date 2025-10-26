//! End-to-end tests for LSP diagnostic flow.
//!
//! Tests the complete flow: MockLspServer -> LspManager -> BufferDiagnostic conversion.

use std::{path::PathBuf, sync::Arc};
use stoat_lsp::{
    lsp_types,
    test::{run_async_test, DiagnosticKind, MockDiagnostic, MockLspServer},
    DiagnosticSeverity, LspManager,
};
use text::{Buffer, BufferId};

#[test]
#[ignore] // TODO: MockNotificationStream needs proper async waker implementation
fn lsp_diagnostics_convert_to_buffer_diagnostics() {
    run_async_test(|| async {
        // Create buffer
        let buffer = Buffer::new(0, BufferId::new(1).unwrap(), "let foo = bar;");
        let snapshot = buffer.snapshot();

        // Create mock with programmed diagnostics
        //  Note: Path must match what will be extracted from the file:// URI
        let mock = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
            "/test.rs",
            vec![MockDiagnostic {
                range: "0:10-0:13",
                kind: DiagnosticKind::UndefinedName,
                message: String::new(),
            }],
        ));

        // Create manager and spawn server
        let manager = LspManager::new();
        let server_id = manager.spawn_server("rust-analyzer", mock).await.unwrap();

        // Send didOpen - this triggers PublishDiagnostics from mock
        manager
            .did_open(
                server_id,
                "file:///test.rs".parse().unwrap(),
                "rust".to_string(),
                0,
                "let foo = bar;".to_string(),
            )
            .await
            .unwrap();

        // Give async notification processing time to complete
        smol::Timer::after(std::time::Duration::from_millis(100)).await;

        // Convert LSP diagnostics to BufferDiagnostic
        let path = PathBuf::from("/test.rs");
        let diag_set = manager
            .diagnostics_for_buffer(&path, &snapshot)
            .expect("Should have diagnostics");

        // Verify diagnostics were converted correctly
        assert_eq!(diag_set.len(), 1);

        let diags: Vec<_> = diag_set.diagnostics_for_row(0, &snapshot).collect();
        assert_eq!(diags.len(), 1);

        let diag = &diags[0];
        assert_eq!(diag.severity, DiagnosticSeverity::Error);
        assert!(diag.message.contains("cannot find value"));
        assert_eq!(diag.server_id, server_id);

        // Verify anchor positions match expected range
        use text::ToPoint;
        let start = diag.range.start.to_point(&snapshot);
        let end = diag.range.end.to_point(&snapshot);
        assert_eq!(start.row, 0);
        assert_eq!(start.column, 10);
        assert_eq!(end.row, 0);
        assert_eq!(end.column, 13);
    });
}

#[test]
#[ignore] // TODO: MockNotificationStream needs proper async waker implementation
fn multiple_servers_diagnostics_merged() {
    run_async_test(|| async {
        // Create buffer
        let buffer = Buffer::new(0, BufferId::new(1).unwrap(), "let foo = bar;");
        let snapshot = buffer.snapshot();

        // Create two mocks with overlapping diagnostics at same position
        let mock1 = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
            "/test.rs",
            vec![MockDiagnostic {
                range: "0:10-0:13",
                kind: DiagnosticKind::UndefinedName,
                message: String::new(),
            }],
        ));

        let mock2 = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
            "/test.rs",
            vec![MockDiagnostic {
                range: "0:10-0:13",
                kind: DiagnosticKind::Custom {
                    severity: lsp_types::DiagnosticSeverity::WARNING,
                    code: Some("unused".to_string()),
                },
                message: "unused variable".to_string(),
            }],
        ));

        // Create manager and spawn both servers
        let manager = LspManager::new();
        let server1 = manager
            .spawn_server("rust-analyzer-1", mock1)
            .await
            .unwrap();
        let server2 = manager
            .spawn_server("rust-analyzer-2", mock2)
            .await
            .unwrap();

        // Send didOpen to both servers
        for server_id in [server1, server2] {
            manager
                .did_open(
                    server_id,
                    "file:///test.rs".parse().unwrap(),
                    "rust".to_string(),
                    0,
                    "let foo = bar;".to_string(),
                )
                .await
                .unwrap();
        }

        // Give async processing time
        smol::Timer::after(std::time::Duration::from_millis(100)).await;

        // Get diagnostics - should have both
        let path = PathBuf::from("/test.rs");
        let diag_set = manager
            .diagnostics_for_buffer(&path, &snapshot)
            .expect("Should have diagnostics");

        // Both diagnostics should be present (they're from different servers)
        assert_eq!(diag_set.len(), 2);
    });
}

#[test]
#[ignore] // TODO: MockNotificationStream needs proper async waker implementation
fn diagnostics_track_through_buffer_edits() {
    run_async_test(|| async {
        // Create buffer
        let mut buffer = Buffer::new(0, BufferId::new(1).unwrap(), "let foo = bar;");
        let snapshot = buffer.snapshot();

        // Create mock with diagnostic on "bar"
        let mock = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
            "/test.rs",
            vec![MockDiagnostic {
                range: "0:10-0:13",
                kind: DiagnosticKind::UndefinedName,
                message: String::new(),
            }],
        ));

        let manager = LspManager::new();
        let server_id = manager.spawn_server("rust-analyzer", mock).await.unwrap();

        manager
            .did_open(
                server_id,
                "file:///test.rs".parse().unwrap(),
                "rust".to_string(),
                0,
                "let foo = bar;".to_string(),
            )
            .await
            .unwrap();

        smol::Timer::after(std::time::Duration::from_millis(100)).await;

        // Get diagnostics with anchors
        let path = PathBuf::from("/test.rs");
        let diag_set = manager
            .diagnostics_for_buffer(&path, &snapshot)
            .expect("Should have diagnostics");

        // Edit buffer - insert text before the diagnostic
        use text::Point;
        buffer.edit([(Point::new(0, 0)..Point::new(0, 0), "// comment\n")]);
        let new_snapshot = buffer.snapshot();

        // Diagnostic anchors should have tracked to new position
        let diags: Vec<_> = diag_set.diagnostics_for_row(1, &new_snapshot).collect();
        assert_eq!(diags.len(), 1);

        use text::ToPoint;
        let start = diags[0].range.start.to_point(&new_snapshot);
        let end = diags[0].range.end.to_point(&new_snapshot);

        // Should have moved to next line
        assert_eq!(start.row, 1);
        assert_eq!(start.column, 10);
        assert_eq!(end.row, 1);
        assert_eq!(end.column, 13);
    });
}
