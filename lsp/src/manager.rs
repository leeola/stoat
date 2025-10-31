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

/// Notification when LSP server reports progress.
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    /// Server that sent the progress
    pub server_id: ServerId,
    /// Progress token (identifies the operation)
    pub token: String,
    /// Kind of progress event
    pub kind: ProgressKind,
    /// Title of the operation
    pub title: String,
    /// Optional message with details
    pub message: Option<String>,
    /// Optional percentage (0-100)
    pub percentage: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressKind {
    Begin,
    Report,
    End,
}

/// Manages language server lifecycle and diagnostic routing.
pub struct LspManager {
    inner: Arc<Mutex<LspManagerInner>>,
    executor: gpui::BackgroundExecutor,
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

    /// Subscribers to progress notifications
    progress_subscribers: Vec<Sender<ProgressUpdate>>,
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
    /// Create a new LSP manager with the given executor.
    ///
    /// The executor is used to spawn background tasks for listening to LSP notifications.
    pub fn new(executor: gpui::BackgroundExecutor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LspManagerInner {
                servers: HashMap::new(),
                next_server_id: 0,
                lsp_diagnostics: HashMap::new(),
                diagnostic_subscribers: Vec::new(),
                progress_subscribers: Vec::new(),
            })),
            executor,
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
    /// let manager = LspManager::new(cx.background_executor().clone());
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

    /// Subscribe to LSP progress notifications.
    ///
    /// Returns a channel that receives notifications when language servers
    /// report progress ($/progress). Used to track operations like indexing.
    pub fn subscribe_progress_updates(&self) -> Receiver<ProgressUpdate> {
        let (tx, rx) = async_channel::unbounded();
        self.inner.lock().progress_subscribers.push(tx);
        rx
    }

    /// Add a language server without starting notification listener.
    ///
    /// Returns the server ID. Call `start_listener()` to begin processing notifications.
    /// Use `spawn_server()` to add and start listener in one call.
    pub fn add_server(
        &self,
        name: impl Into<String>,
        transport: Arc<dyn LspTransport>,
    ) -> ServerId {
        let name = name.into();
        let mut inner = self.inner.lock();
        let server_id = inner.next_server_id;
        inner.next_server_id += 1;

        inner.servers.insert(
            server_id,
            ServerState {
                id: server_id,
                transport,
                name,
            },
        );

        server_id
    }

    /// Start background listener task for a server.
    ///
    /// Spawns a background task that listens for notifications from the server
    /// and processes them. The task runs until the notification stream ends.
    pub fn start_listener(&self, server_id: ServerId) -> Result<()> {
        let transport = {
            let inner = self.inner.lock();
            inner
                .servers
                .get(&server_id)
                .context("Server not found")?
                .transport
                .clone()
        };

        let mut stream = transport.subscribe_notifications();
        let inner = self.inner.clone();

        self.executor
            .spawn(async move {
                while let Some(notification) = stream.next().await {
                    if let Err(e) = Self::handle_notification(&inner, server_id, &notification) {
                        tracing::warn!("Failed to handle notification: {}", e);
                    }
                }
            })
            .detach();

        Ok(())
    }

    /// Spawn a language server with a custom transport.
    ///
    /// Adds the server and starts its notification listener.
    /// Returns the server ID for this instance.
    pub async fn spawn_server(
        &self,
        name: impl Into<String>,
        transport: Arc<dyn LspTransport>,
    ) -> Result<ServerId> {
        let server_id = self.add_server(name, transport);
        self.start_listener(server_id)?;
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
        let transport = Arc::new(StdioTransport::spawn(path, vec![], self.executor.clone())?);
        self.spawn_server("rust-analyzer", transport).await
    }

    /// Send a request to a language server and wait for response.
    ///
    /// # Arguments
    ///
    /// * `server_id` - Server to send request to
    /// * `request` - JSON-RPC request payload
    ///
    /// # Returns
    ///
    /// JSON response from the server as a string
    pub async fn request(&self, server_id: ServerId, request: serde_json::Value) -> Result<String> {
        let transport = self
            .inner
            .lock()
            .servers
            .get(&server_id)
            .ok_or_else(|| anyhow::anyhow!("Server not found"))?
            .transport
            .clone();

        transport
            .send_request(serde_json::to_string(&request)?)
            .await
    }

    /// Send a notification to a language server (no response expected).
    ///
    /// # Arguments
    ///
    /// * `server_id` - Server to send notification to
    /// * `notification` - JSON-RPC notification payload
    pub async fn notify(&self, server_id: ServerId, notification: serde_json::Value) -> Result<()> {
        let transport = self
            .inner
            .lock()
            .servers
            .get(&server_id)
            .ok_or_else(|| anyhow::anyhow!("Server not found"))?
            .transport
            .clone();

        transport
            .send_notification(serde_json::to_string(&notification)?)
            .await
    }

    /// Get list of active server IDs.
    pub fn active_servers(&self) -> Vec<ServerId> {
        self.inner.lock().servers.keys().copied().collect()
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
            "$/progress" => {
                Self::handle_progress(inner, server_id, &message)?;
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

    /// Handle $/progress notification from language server.
    fn handle_progress(
        inner: &Arc<Mutex<LspManagerInner>>,
        server_id: ServerId,
        message: &serde_json::Value,
    ) -> Result<()> {
        let params = &message["params"];

        // Token can be string or number
        let token = params["token"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| params["token"].as_i64().map(|i| i.to_string()))
            .context("Missing progress token")?;

        let value = &params["value"];
        let kind_str = value["kind"].as_str().context("Missing progress kind")?;

        let kind = match kind_str {
            "begin" => ProgressKind::Begin,
            "report" => ProgressKind::Report,
            "end" => ProgressKind::End,
            _ => return Ok(()), // Ignore unknown kinds
        };

        let title = value["title"].as_str().unwrap_or("").to_string();
        let message = value["message"].as_str().map(|s| s.to_string());
        let percentage = value["percentage"].as_u64().map(|p| p as u32);

        let update = ProgressUpdate {
            server_id,
            token,
            kind,
            title,
            message,
            percentage,
        };

        // Notify subscribers (clean up closed channels and clone active ones)
        let subscribers = {
            let mut inner_guard = inner.lock();
            inner_guard
                .progress_subscribers
                .retain(|tx| !tx.is_closed());
            inner_guard.progress_subscribers.clone()
        };

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

    /// Drain and process all buffered notifications for a server.
    ///
    /// This is test-only. Production code uses background listener tasks via `start_listener()`.
    ///
    /// Synchronously processes all buffered notifications from the mock server.
    /// Returns the number of notifications processed.
    #[cfg(feature = "test-support")]
    pub fn drain_pending_notifications(&self, server_id: ServerId) -> Result<usize> {
        let transport = {
            let inner = self.inner.lock();
            inner
                .servers
                .get(&server_id)
                .context("Server not found")?
                .transport
                .clone()
        };

        // Get all buffered notifications from the mock transport
        let notifications = transport.buffered_notifications();
        let count = notifications.len();

        // Process each notification synchronously
        for notif in notifications {
            Self::handle_notification(&self.inner, server_id, &notif)?;
        }

        Ok(count)
    }
}
