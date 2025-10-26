//! LSP manager for server lifecycle and diagnostic routing.
//!
//! [`LspManager`] coordinates multiple language servers, routes diagnostics
//! to buffers, and manages document synchronization.

use crate::{lsp_range_to_anchors, BufferDiagnostic, DiagnosticSet, LspTransport, StdioTransport};
use anyhow::{Context as _, Result};
use async_channel::{Receiver, Sender};
use futures::StreamExt;
use lsp_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, PublishDiagnosticsParams,
    TextDocumentContentChangeEvent, TextDocumentItem, Uri, VersionedTextDocumentIdentifier,
};
use parking_lot::Mutex;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use text::BufferSnapshot;

/// Unique identifier for a language server instance.
pub type ServerId = usize;

/// Notification when diagnostics change for a file.
#[derive(Debug, Clone)]
pub struct DiagnosticUpdate {
    /// File path that received diagnostic updates
    pub path: PathBuf,
    /// Server that sent the diagnostics
    pub server_id: ServerId,
}

/// Manages language server lifecycle and diagnostic routing.
pub struct LspManager {
    inner: Arc<Mutex<LspManagerInner>>,
}

struct LspManagerInner {
    /// Active language servers
    servers: HashMap<ServerId, ServerState>,

    /// Next server ID to assign
    next_server_id: ServerId,

    /// Raw LSP diagnostics per file
    lsp_diagnostics: HashMap<PathBuf, Vec<(ServerId, lsp_types::Diagnostic)>>,

    /// Subscribers to diagnostic update notifications
    diagnostic_subscribers: Vec<Sender<DiagnosticUpdate>>,
}

struct ServerState {
    /// Server identifier
    id: ServerId,

    /// Transport for communication
    transport: Arc<dyn LspTransport>,

    /// Server name (e.g., "rust-analyzer")
    name: String,
}

impl LspManager {
    /// Create a new LSP manager.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LspManagerInner {
                servers: HashMap::new(),
                next_server_id: 0,
                lsp_diagnostics: HashMap::new(),
                diagnostic_subscribers: Vec::new(),
            })),
        }
    }

    /// Subscribe to diagnostic update notifications.
    ///
    /// Returns a channel that receives notifications whenever diagnostics change
    /// for any file. The application should listen to this channel and update
    /// corresponding BufferItems by calling `diagnostics_for_buffer()`.
    ///
    /// # Integration Pattern
    ///
    /// ```rust,ignore
    /// let manager = LspManager::new();
    /// let updates = manager.subscribe_diagnostic_updates();
    ///
    /// // Spawn task to handle updates
    /// smol::spawn(async move {
    ///     while let Ok(update) = updates.recv().await {
    ///         // Find BufferItem for update.path
    ///         if let Some(buffer_item) = find_buffer(&update.path) {
    ///             let snapshot = buffer_item.buffer_snapshot(&cx);
    ///             if let Some(diag_set) = manager.diagnostics_for_buffer(&update.path, &snapshot) {
    ///                 buffer_item.update_diagnostics(update.server_id, diag_set, &mut cx);
    ///             }
    ///         }
    ///     }
    /// }).detach();
    /// ```
    pub fn subscribe_diagnostic_updates(&self) -> Receiver<DiagnosticUpdate> {
        let (tx, rx) = async_channel::unbounded();
        self.inner.lock().diagnostic_subscribers.push(tx);
        rx
    }

    /// Spawn a language server with a custom transport.
    ///
    /// Returns the server ID for this instance.
    pub async fn spawn_server(
        &self,
        name: impl Into<String>,
        transport: Arc<dyn LspTransport>,
    ) -> Result<ServerId> {
        let name = name.into();
        let server_id = {
            let mut inner = self.inner.lock();
            let id = inner.next_server_id;
            inner.next_server_id += 1;

            inner.servers.insert(
                id,
                ServerState {
                    id,
                    transport: transport.clone(),
                    name: name.clone(),
                },
            );

            id
        };

        // Subscribe to notifications from this server
        self.subscribe_notifications(server_id, transport).await?;

        Ok(server_id)
    }

    /// Spawn rust-analyzer language server.
    ///
    /// Convenience method that spawns rust-analyzer using default configuration.
    /// Equivalent to spawning with StdioTransport configured for rust-analyzer.
    ///
    /// # Arguments
    ///
    /// * `command_path` - Path to rust-analyzer executable (defaults to "rust-analyzer" in PATH)
    ///
    /// # Errors
    ///
    /// Returns error if rust-analyzer process fails to spawn.
    pub async fn spawn_rust_analyzer(&self, command_path: Option<PathBuf>) -> Result<ServerId> {
        let path = command_path.unwrap_or_else(|| PathBuf::from("rust-analyzer"));
        let transport = Arc::new(StdioTransport::spawn(path, vec![])?);
        self.spawn_server("rust-analyzer", transport).await
    }

    /// Subscribe to server notifications (PublishDiagnostics, etc).
    async fn subscribe_notifications(
        &self,
        server_id: ServerId,
        transport: Arc<dyn LspTransport>,
    ) -> Result<()> {
        let mut stream = transport.subscribe_notifications();
        let inner = self.inner.clone();

        // Spawn task to handle notifications
        smol::spawn(async move {
            while let Some(notification) = stream.next().await {
                if let Err(e) = Self::handle_notification(&inner, server_id, &notification) {
                    tracing::warn!("Failed to handle notification: {}", e);
                }
            }
        })
        .detach();

        // Yield to give the spawned task a chance to start polling
        smol::future::yield_now().await;

        Ok(())
    }

    /// Handle a notification from a language server.
    fn handle_notification(
        inner: &Arc<Mutex<LspManagerInner>>,
        server_id: ServerId,
        notification: &str,
    ) -> Result<()> {
        let message: serde_json::Value =
            serde_json::from_str(notification).context("Failed to parse notification")?;

        let method = message["method"].as_str().context("Missing method field")?;

        match method {
            "textDocument/publishDiagnostics" => {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(message["params"].clone())
                        .context("Failed to parse PublishDiagnostics params")?;

                Self::handle_publish_diagnostics(inner, server_id, params)?;
            },
            _ => {
                // Ignore other notifications for now
            },
        }

        Ok(())
    }

    /// Handle PublishDiagnostics notification.
    fn handle_publish_diagnostics(
        inner: &Arc<Mutex<LspManagerInner>>,
        server_id: ServerId,
        params: PublishDiagnosticsParams,
    ) -> Result<()> {
        // Extract file path from URI (strip file:// prefix)
        let uri_str = params.uri.as_str();
        let path = if let Some(path_str) = uri_str.strip_prefix("file://") {
            PathBuf::from(path_str)
        } else {
            anyhow::bail!("Invalid file URI (not file://): {}", uri_str);
        };

        let subscribers = {
            let mut inner_guard = inner.lock();

            // Remove old diagnostics from this server for this file
            let diagnostics_for_file = inner_guard
                .lsp_diagnostics
                .entry(path.clone())
                .or_insert_with(Vec::new);

            diagnostics_for_file.retain(|(sid, _)| *sid != server_id);

            // Add new diagnostics from this server
            for diag in params.diagnostics {
                diagnostics_for_file.push((server_id, diag));
            }

            // Clean up closed channels and clone remaining subscribers
            inner_guard
                .diagnostic_subscribers
                .retain(|tx| !tx.is_closed());
            inner_guard.diagnostic_subscribers.clone()
        };

        // Notify subscribers that diagnostics changed (outside lock)
        let update = DiagnosticUpdate { path, server_id };

        // Send to all subscribers (non-blocking for unbounded channels)
        for tx in subscribers {
            let _ = tx.try_send(update.clone());
        }

        Ok(())
    }

    /// Convert LSP diagnostics for a file to BufferDiagnostic set.
    ///
    /// Takes LSP diagnostics received from language servers and converts them to
    /// BufferDiagnostic objects with anchor-based positions. This allows the diagnostics
    /// to automatically track through buffer edits.
    ///
    /// # Arguments
    ///
    /// * `path` - File path to get diagnostics for
    /// * `snapshot` - Buffer snapshot for creating anchors
    ///
    /// # Returns
    ///
    /// DiagnosticSet with anchor-based positions, or None if no diagnostics exist
    pub fn diagnostics_for_buffer(
        &self,
        path: &PathBuf,
        snapshot: &BufferSnapshot,
    ) -> Option<DiagnosticSet> {
        let inner = self.inner.lock();
        let lsp_diags = inner.lsp_diagnostics.get(path)?;

        let mut set = DiagnosticSet::new();

        for (server_id, lsp_diag) in lsp_diags {
            // Convert LSP range to anchors
            let range = match lsp_range_to_anchors(&lsp_diag.range, snapshot) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "Failed to convert LSP diagnostic range for {}: {}",
                        path.display(),
                        e
                    );
                    continue;
                },
            };

            let buffer_diag = BufferDiagnostic {
                range,
                severity: lsp_diag
                    .severity
                    .map(|s| match s {
                        lsp_types::DiagnosticSeverity::ERROR => crate::DiagnosticSeverity::Error,
                        lsp_types::DiagnosticSeverity::WARNING => {
                            crate::DiagnosticSeverity::Warning
                        },
                        lsp_types::DiagnosticSeverity::INFORMATION => {
                            crate::DiagnosticSeverity::Information
                        },
                        lsp_types::DiagnosticSeverity::HINT => crate::DiagnosticSeverity::Hint,
                        _ => crate::DiagnosticSeverity::Information,
                    })
                    .unwrap_or(crate::DiagnosticSeverity::Information),
                code: lsp_diag.code.as_ref().map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s.clone(),
                }),
                source: lsp_diag.source.clone(),
                message: lsp_diag.message.clone(),
                server_id: *server_id,
            };

            set.insert(buffer_diag);
        }

        Some(set)
    }

    /// Send didOpen notification for a file.
    pub async fn did_open(
        &self,
        server_id: ServerId,
        uri: Uri,
        language_id: String,
        version: i32,
        text: String,
    ) -> Result<()> {
        let transport = {
            let inner = self.inner.lock();
            inner
                .servers
                .get(&server_id)
                .context("Server not found")?
                .transport
                .clone()
        };

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id,
                version,
                text,
            },
        };

        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": params,
        });

        transport
            .send_notification(notification.to_string())
            .await?;

        Ok(())
    }

    /// Send didChange notification for a file.
    ///
    /// Sends the full text content. For incremental sync, use `did_change_incremental`.
    pub async fn did_change(
        &self,
        server_id: ServerId,
        uri: Uri,
        version: i32,
        text: String,
    ) -> Result<()> {
        let transport = {
            let inner = self.inner.lock();
            inner
                .servers
                .get(&server_id)
                .context("Server not found")?
                .transport
                .clone()
        };

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text,
            }],
        };

        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": params,
        });

        transport
            .send_notification(notification.to_string())
            .await?;

        Ok(())
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::{run_async_test, DiagnosticKind, MockDiagnostic, MockLspServer};

    #[test]
    fn spawn_server_assigns_id() {
        run_async_test(|| async {
            let manager = LspManager::new();
            let mock = Arc::new(MockLspServer::rust_analyzer());

            let server_id = manager.spawn_server("rust-analyzer", mock).await.unwrap();
            assert_eq!(server_id, 0);

            let mock2 = Arc::new(MockLspServer::rust_analyzer());
            let server_id2 = manager.spawn_server("rust-analyzer", mock2).await.unwrap();
            assert_eq!(server_id2, 1);
        });
    }

    #[test]
    #[ignore] // TODO: MockNotificationStream needs proper async waker implementation
    fn diagnostic_updates_emit_notifications() {
        run_async_test(|| async {
            let manager = LspManager::new();
            let mock = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
                "/test.rs",
                vec![MockDiagnostic {
                    range: "0:10-0:13",
                    kind: DiagnosticKind::UndefinedName,
                    message: String::new(),
                }],
            ));

            // Subscribe before spawning server
            let updates = manager.subscribe_diagnostic_updates();

            let server_id = manager
                .spawn_server("rust-analyzer", mock.clone())
                .await
                .unwrap();

            // Send didOpen to trigger diagnostics
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

            // Wait for notification with timeout
            let update = smol::future::or(
                async {
                    smol::Timer::after(std::time::Duration::from_secs(2)).await;
                    None
                },
                async { updates.recv().await.ok() },
            )
            .await;

            // Should receive diagnostic update notification
            assert!(update.is_some(), "Should receive diagnostic update");
            let update = update.unwrap();
            assert_eq!(update.path, PathBuf::from("/test.rs"));
            assert_eq!(update.server_id, server_id);
        });
    }

    #[test]
    #[ignore] // TODO: MockNotificationStream needs proper async waker implementation
    fn multiple_subscribers_receive_updates() {
        run_async_test(|| async {
            let manager = LspManager::new();
            let mock = Arc::new(MockLspServer::rust_analyzer().with_diagnostics(
                "/test.rs",
                vec![MockDiagnostic {
                    range: "0:0-0:3",
                    kind: DiagnosticKind::UndefinedName,
                    message: String::new(),
                }],
            ));

            // Create multiple subscribers
            let updates1 = manager.subscribe_diagnostic_updates();
            let updates2 = manager.subscribe_diagnostic_updates();

            let server_id = manager
                .spawn_server("rust-analyzer", mock.clone())
                .await
                .unwrap();

            // Trigger diagnostics
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

            // Both subscribers should receive the update
            let update1 = smol::future::or(
                async {
                    smol::Timer::after(std::time::Duration::from_secs(2)).await;
                    None
                },
                async { updates1.recv().await.ok() },
            )
            .await;

            let update2 = smol::future::or(
                async {
                    smol::Timer::after(std::time::Duration::from_secs(2)).await;
                    None
                },
                async { updates2.recv().await.ok() },
            )
            .await;

            assert!(update1.is_some() && update2.is_some());
            assert_eq!(
                update1.as_ref().unwrap().path,
                update2.as_ref().unwrap().path
            );
        });
    }

    #[test]
    fn did_open_sends_notification() {
        run_async_test(|| async {
            let manager = LspManager::new();
            let mock = Arc::new(MockLspServer::rust_analyzer());

            let server_id = manager
                .spawn_server("rust-analyzer", mock.clone())
                .await
                .unwrap();

            // Send didOpen
            manager
                .did_open(
                    server_id,
                    "file:///test.rs".parse().unwrap(),
                    "rust".to_string(),
                    0,
                    "fn main() {}".to_string(),
                )
                .await
                .unwrap();

            // FIXME: Add assertion that mock received the notification
        });
    }

    #[test]
    fn did_change_sends_notification() {
        run_async_test(|| async {
            let manager = LspManager::new();
            let mock = Arc::new(MockLspServer::rust_analyzer());

            let server_id = manager
                .spawn_server("rust-analyzer", mock.clone())
                .await
                .unwrap();

            // Send didChange
            manager
                .did_change(
                    server_id,
                    "file:///test.rs".parse().unwrap(),
                    1,
                    "fn main() { println!(\"Hello\"); }".to_string(),
                )
                .await
                .unwrap();

            // FIXME: Add assertion that mock received the notification
        });
    }
}
