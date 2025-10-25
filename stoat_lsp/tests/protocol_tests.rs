//! Protocol layer integration tests.

use stoat_lsp::{
    test::{DiagnosticKind, MockDiagnostic, MockLspServer},
    transport::LspTransport,
};

#[tokio::test]
async fn mock_server_initialize() {
    let mock = MockLspServer::rust_analyzer();

    let request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let response = mock
        .send_request(request.to_string())
        .await
        .expect("Request failed");

    assert!(response.contains("capabilities"));
    assert!(response.contains("textDocumentSync"));
}

#[tokio::test]
async fn mock_server_publishes_diagnostics_on_did_open() {
    let mock = MockLspServer::rust_analyzer().with_diagnostics(
        "/test.rs", // Path after stripping "file://" prefix
        vec![MockDiagnostic {
            range: "0:10-0:13",
            kind: DiagnosticKind::UndefinedName,
            message: String::new(),
        }],
    );

    let source = "let foo = bar;";
    let notification = format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///test.rs","text":"{}"}}}}}}"#,
        source
    );

    mock.send_notification(notification)
        .await
        .expect("Notification failed");

    // Check that notification was queued
    use futures::StreamExt;
    let mut stream = mock.subscribe_notifications();

    // Add timeout to prevent hanging
    let published = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("Timeout waiting for notification")
        .expect("No notification published");

    assert!(published.contains("textDocument/publishDiagnostics"));
    assert!(published.contains("cannot find value"));
    assert!(published.contains("bar"));
}

#[tokio::test]
async fn mock_generates_realistic_undefined_name_diagnostic() {
    let mock = MockLspServer::rust_analyzer();
    let source = "fn main() {\n    undefined_var\n}";

    let mock_diag = MockDiagnostic {
        range: "1:4-1:17",
        kind: DiagnosticKind::UndefinedName,
        message: String::new(),
    };

    let lsp_diag = mock.to_lsp_diagnostic(&mock_diag, source);

    assert_eq!(
        lsp_diag.severity,
        Some(lsp_types::DiagnosticSeverity::ERROR)
    );
    assert!(lsp_diag.message.contains("cannot find value"));
    assert!(lsp_diag.message.contains("undefined_var"));
    assert_eq!(
        lsp_diag.code,
        Some(lsp_types::NumberOrString::String("E0425".to_string()))
    );
    assert_eq!(lsp_diag.source, Some("rust-analyzer".to_string()));
}

#[tokio::test]
async fn mock_generates_type_mismatch_diagnostic() {
    let mock = MockLspServer::rust_analyzer();
    let source = r#"let x: i32 = "string";"#;

    let mock_diag = MockDiagnostic {
        range: "0:13-0:21",
        kind: DiagnosticKind::TypeMismatch {
            expected: "i32",
            found: "&str",
        },
        message: String::new(),
    };

    let lsp_diag = mock.to_lsp_diagnostic(&mock_diag, source);

    assert_eq!(
        lsp_diag.severity,
        Some(lsp_types::DiagnosticSeverity::ERROR)
    );
    assert!(lsp_diag.message.contains("expected `i32`, found `&str`"));
    assert_eq!(
        lsp_diag.code,
        Some(lsp_types::NumberOrString::String("E0308".to_string()))
    );
}

#[tokio::test]
async fn mock_generates_unused_variable_warning() {
    let mock = MockLspServer::rust_analyzer();
    let source = "let unused = 42;";

    let mock_diag = MockDiagnostic {
        range: "0:4-0:10",
        kind: DiagnosticKind::UnusedVariable,
        message: String::new(),
    };

    let lsp_diag = mock.to_lsp_diagnostic(&mock_diag, source);

    assert_eq!(
        lsp_diag.severity,
        Some(lsp_types::DiagnosticSeverity::WARNING)
    );
    assert!(lsp_diag.message.contains("unused variable"));
    assert!(lsp_diag.message.contains("unused"));
}
