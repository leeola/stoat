//! Mock LSP server for testing.
//!
//! Provides a sophisticated mock that simulates language server behavior
//! without spawning real processes. This enables fast, deterministic tests.

use crate::{
    protocol::{Message, Notification, Response},
    test::test_helpers::parse_range_notation,
    transport::LspTransport,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use lsp_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, PublishDiagnosticsParams, Range, Uri,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, pin::Pin, str::FromStr, sync::Arc};

/// Rich mock that simulates language server behavior.
///
/// Provides programmatic control over diagnostics, capabilities, and responses
/// to enable comprehensive unit testing without real language servers.
#[derive(Clone)]
pub struct MockLspServer {
    inner: Arc<Mutex<MockLspServerInner>>,
}

struct MockLspServerInner {
    /// Programmed diagnostics by file path
    diagnostics: HashMap<PathBuf, Vec<MockDiagnostic>>,
    /// Server behavior preset
    behavior: MockBehavior,
    /// Buffered notifications (simple Vec for testing)
    notification_buffer: Vec<String>,
}

/// Pre-configured behavioral presets.
#[derive(Debug, Clone, Copy)]
pub enum MockBehavior {
    /// Simulate rust-analyzer behavior
    RustAnalyzer,
    /// Simulate TypeScript language server
    TypeScriptLS,
    /// Custom behavior
    Custom,
}

/// High-level diagnostic specification.
///
/// Provides a concise DSL for specifying diagnostics in tests without
/// dealing with verbose LSP types.
#[derive(Debug, Clone)]
pub struct MockDiagnostic {
    /// Position in source code (line:col-line:col notation)
    pub range: &'static str,
    /// Diagnostic type
    pub kind: DiagnosticKind,
    /// Optional custom message (auto-generated if empty)
    pub message: String,
}

/// Common diagnostic types with automatic message generation.
#[derive(Debug, Clone)]
pub enum DiagnosticKind {
    /// Undefined variable/function
    UndefinedName,
    /// Type mismatch
    TypeMismatch {
        expected: &'static str,
        found: &'static str,
    },
    /// Unused variable
    UnusedVariable,
    /// Syntax error
    SyntaxError,
    /// Custom diagnostic
    Custom {
        severity: DiagnosticSeverity,
        code: Option<String>,
    },
}

impl MockLspServer {
    /// Create mock with rust-analyzer preset.
    pub fn rust_analyzer() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockLspServerInner {
                diagnostics: HashMap::new(),
                behavior: MockBehavior::RustAnalyzer,
                notification_buffer: Vec::new(),
            })),
        }
    }

    /// Add diagnostics for a file.
    ///
    /// These diagnostics will be sent when the file is opened via
    /// `textDocument/didOpen` notification.
    pub fn with_diagnostics(
        self,
        file: impl Into<PathBuf>,
        diagnostics: Vec<MockDiagnostic>,
    ) -> Self {
        self.inner
            .lock()
            .diagnostics
            .insert(file.into(), diagnostics);
        self
    }

    /// Get buffered notifications for testing.
    ///
    /// Returns all notifications that have been buffered by this mock server.
    #[cfg(feature = "test-support")]
    pub fn buffered_notifications(&self) -> Vec<String> {
        self.inner.lock().notification_buffer.clone()
    }

    /// Handle a textDocument/didOpen notification.
    ///
    /// Responds by publishing diagnostics for the file.
    fn handle_did_open(&self, params: &Value, source: &str) {
        let uri_str = params["textDocument"]["uri"]
            .as_str()
            .expect("Missing textDocument.uri");

        let uri = Uri::from_str(uri_str).expect("Invalid URI");

        // Extract path from file:// URI
        let path = if let Some(stripped) = uri_str.strip_prefix("file://") {
            PathBuf::from(stripped)
        } else {
            PathBuf::from(uri.path().as_str())
        };

        let mut inner = self.inner.lock();

        // Get source_name while holding lock
        let source_name = match inner.behavior {
            MockBehavior::RustAnalyzer => "rust-analyzer",
            MockBehavior::TypeScriptLS => "typescript",
            MockBehavior::Custom => "mock-lsp",
        };

        // Early return if no diagnostics programmed at all
        if inner.diagnostics.is_empty() {
            return;
        }

        if let Some(mock_diagnostics) = inner.diagnostics.get(&path) {
            let lsp_diagnostics: Vec<Diagnostic> = mock_diagnostics
                .iter()
                .map(|d| self.to_lsp_diagnostic(d, source, source_name))
                .collect();

            let publish = PublishDiagnosticsParams {
                uri,
                diagnostics: lsp_diagnostics,
                version: None,
            };

            let notification = Notification::new(
                "textDocument/publishDiagnostics",
                Some(serde_json::to_value(publish).expect("Failed to serialize")),
            );

            let notification_json = notification.to_json().expect("Failed to serialize");

            // Buffer notification for test retrieval
            inner.notification_buffer.push(notification_json);
        }
    }

    /// Convert MockDiagnostic to LSP Diagnostic (for testing).
    pub fn to_lsp_diagnostic(
        &self,
        mock: &MockDiagnostic,
        source: &str,
        source_name: &str,
    ) -> Diagnostic {
        let range = parse_range_notation(mock.range, source).expect("Invalid range notation");

        let (severity, code, message) = match &mock.kind {
            DiagnosticKind::UndefinedName => {
                let text = extract_text_at_range(&range, source);
                (
                    DiagnosticSeverity::ERROR,
                    Some("E0425".to_string()),
                    format!("cannot find value `{}` in this scope", text),
                )
            },
            DiagnosticKind::TypeMismatch { expected, found } => (
                DiagnosticSeverity::ERROR,
                Some("E0308".to_string()),
                format!(
                    "mismatched types: expected `{}`, found `{}`",
                    expected, found
                ),
            ),
            DiagnosticKind::UnusedVariable => {
                let text = extract_text_at_range(&range, source);
                (
                    DiagnosticSeverity::WARNING,
                    Some("unused_variables".to_string()),
                    if mock.message.is_empty() {
                        format!("unused variable: `{}`", text)
                    } else {
                        mock.message.clone()
                    },
                )
            },
            DiagnosticKind::SyntaxError => (
                DiagnosticSeverity::ERROR,
                None,
                if mock.message.is_empty() {
                    "syntax error".to_string()
                } else {
                    mock.message.clone()
                },
            ),
            DiagnosticKind::Custom { severity, code } => {
                (*severity, code.clone(), mock.message.clone())
            },
        };

        Diagnostic {
            range,
            severity: Some(severity),
            code: code.map(NumberOrString::String),
            source: Some(source_name.to_string()),
            message,
            ..Default::default()
        }
    }
}

/// Extract text at a given LSP Range from source.
fn extract_text_at_range(range: &Range, source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();

    if range.start.line == range.end.line {
        let line = lines[range.start.line as usize];
        let start = range.start.character as usize;
        let end = range.end.character as usize;
        line[start..end].to_string()
    } else {
        // Multi-line range - just return placeholder
        "...".to_string()
    }
}

#[async_trait]
impl LspTransport for MockLspServer {
    async fn send_request(&self, request: String) -> Result<String> {
        let msg = crate::protocol::parse_message(&request)?;

        match msg {
            Message::Request(req) => {
                let response = match req.method.as_str() {
                    "initialize" => {
                        let result = json!({
                            "capabilities": {
                                "textDocumentSync": 1,
                                "diagnosticProvider": true,
                            },
                            "serverInfo": {
                                "name": "mock-lsp",
                                "version": "0.1.0"
                            }
                        });
                        Response::success(req.id, result)
                    },
                    _ => Response::success(req.id, json!(null)),
                };

                Ok(response.to_json()?)
            },
            _ => anyhow::bail!("Expected request, got {:?}", msg),
        }
    }

    async fn send_notification(&self, notification: String) -> Result<()> {
        let msg = crate::protocol::parse_message(&notification)?;

        match msg {
            Message::Notification(notif) => {
                if notif.method == "textDocument/didOpen" {
                    let params = notif.params.unwrap_or(json!({}));
                    let source = params["textDocument"]["text"].as_str().unwrap_or("");
                    self.handle_did_open(&params, source);
                }
                Ok(())
            },
            _ => anyhow::bail!("Expected notification, got {:?}", msg),
        }
    }

    fn subscribe_notifications(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        // For testing, notifications are retrieved via buffered_notifications()
        // Return empty stream since tests don't use subscriptions
        Box::pin(futures::stream::empty())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    #[cfg(feature = "test-support")]
    fn buffered_notifications(&self) -> Vec<String> {
        self.inner.lock().notification_buffer.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_rust_analyzer_mock() {
        let mock = MockLspServer::rust_analyzer();
        let inner = mock.inner.lock();
        assert!(matches!(inner.behavior, MockBehavior::RustAnalyzer));
    }

    #[test]
    fn add_diagnostics() {
        let mock = MockLspServer::rust_analyzer().with_diagnostics(
            "test.rs",
            vec![MockDiagnostic {
                range: "0:0-0:3",
                kind: DiagnosticKind::UndefinedName,
                message: String::new(),
            }],
        );

        let inner = mock.inner.lock();
        assert_eq!(inner.diagnostics.len(), 1);
    }

    #[test]
    fn generate_undefined_name_diagnostic() {
        let mock = MockLspServer::rust_analyzer();
        let source = "let foo = bar;";
        let mock_diag = MockDiagnostic {
            range: "0:10-0:13",
            kind: DiagnosticKind::UndefinedName,
            message: String::new(),
        };

        let lsp_diag = mock.to_lsp_diagnostic(&mock_diag, source, "rust-analyzer");
        assert_eq!(lsp_diag.severity, Some(DiagnosticSeverity::ERROR));
        assert!(lsp_diag.message.contains("cannot find value"));
        assert!(lsp_diag.message.contains("bar"));
    }

    #[test]
    fn generate_type_mismatch_diagnostic() {
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

        let lsp_diag = mock.to_lsp_diagnostic(&mock_diag, source, "rust-analyzer");
        assert_eq!(lsp_diag.severity, Some(DiagnosticSeverity::ERROR));
        assert!(lsp_diag.message.contains("expected `i32`, found `&str`"));
    }
}
