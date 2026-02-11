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
    ServerCapabilities, TextDocumentContentChangeEvent, TextDocumentItem,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri, VersionedTextDocumentIdentifier,
};
use parking_lot::Mutex;
use rustc_hash::FxHasher;
use serde::Deserialize;
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use text::BufferSnapshot;

/// LSP notification types for single-pass parsing.
#[derive(Deserialize)]
#[serde(tag = "method")]
enum LspNotification {
    #[serde(rename = "textDocument/publishDiagnostics")]
    PublishDiagnostics { params: PublishDiagnosticsParams },

    #[serde(rename = "$/progress")]
    Progress { params: ProgressNotificationParams },

    #[serde(other)]
    Unknown,
}

/// Progress notification parameters.
#[derive(Deserialize)]
struct ProgressNotificationParams {
    token: ProgressToken,
    value: WorkDoneProgressValue,
}

/// Progress token (can be string or number).
#[derive(Deserialize)]
#[serde(untagged)]
enum ProgressToken {
    String(String),
    Number(i64),
}

impl ProgressToken {
    fn to_string(&self) -> String {
        match self {
            ProgressToken::String(s) => s.clone(),
            ProgressToken::Number(n) => n.to_string(),
        }
    }
}

/// Work done progress value.
#[derive(Deserialize)]
struct WorkDoneProgressValue {
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    percentage: Option<u32>,
}

/// Unique identifier for a language server instance.
pub type ServerId = usize;

/// Internal identifier for tracking cancellable requests.
type PendingRequestId = u64;

/// Internal identifier for buffer diagnostic storage.
/// Using integer IDs avoids repeated PathBuf hashing.
type BufferId = usize;

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

/// Handle to an in-flight LSP request that can be cancelled.
///
/// This handle can be awaited to get the request result, or cancelled to abort the request.
pub struct RequestHandle {
    request_id: PendingRequestId,
    manager: Arc<Mutex<LspManagerInner>>,
    future: futures::future::Abortable<gpui::Task<Result<String>>>,
}

impl RequestHandle {
    /// Cancel this request.
    ///
    /// Aborts the local future and removes from pending request tracking.
    /// If the request has already completed, this is a no-op.
    pub fn cancel(&self) {
        let mut inner = self.manager.lock();
        if let Some(abort_handle) = inner.pending_requests.remove(&self.request_id) {
            abort_handle.abort();
        }
    }
}

impl std::future::Future for RequestHandle {
    type Output = Result<String>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::pin::Pin;
        match Pin::new(&mut self.future).poll(cx) {
            std::task::Poll::Ready(Ok(result)) => {
                // Clean up from pending requests
                self.manager
                    .lock()
                    .pending_requests
                    .remove(&self.request_id);
                std::task::Poll::Ready(result)
            },
            std::task::Poll::Ready(Err(_aborted)) => {
                // Request was cancelled
                self.manager
                    .lock()
                    .pending_requests
                    .remove(&self.request_id);
                std::task::Poll::Ready(Err(anyhow::anyhow!("Request cancelled")))
            },
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Channel capacity for diagnostic update notifications.
/// With bounded channels, slow consumers won't cause unbounded memory growth.
const DIAGNOSTIC_CHANNEL_CAPACITY: usize = 1000;

/// Channel capacity for progress update notifications.
/// rust-analyzer sends large bursts during initial indexing (thousands of
/// Roots Scanned updates in sub-millisecond windows), so this needs headroom.
const PROGRESS_CHANNEL_CAPACITY: usize = 4096;

/// Manages language server lifecycle and diagnostic routing.
///
/// Coordinates multiple language servers with timeout protection for requests.
/// All requests are bounded by a configurable timeout to prevent indefinite hangs
/// from unresponsive servers.
pub struct LspManager {
    inner: Arc<Mutex<LspManagerInner>>,
    executor: gpui::BackgroundExecutor,
    /// Maximum duration to wait for LSP request responses before timing out
    request_timeout: std::time::Duration,
}

struct LspManagerInner {
    /// Active language servers
    servers: HashMap<ServerId, ServerState>,

    /// Next server ID to assign
    next_server_id: ServerId,

    /// Pending LSP requests that can be cancelled
    pending_requests: HashMap<PendingRequestId, futures::future::AbortHandle>,

    /// Next request ID to assign
    next_request_id: Arc<AtomicU64>,

    /// Raw LSP diagnostics per buffer (using integer IDs for faster hashing)
    lsp_diagnostics: HashMap<BufferId, Vec<(ServerId, lsp_types::Diagnostic)>>,

    /// Mapping from buffer ID to file path
    buffer_paths: HashMap<BufferId, PathBuf>,

    /// Mapping from file path to buffer ID
    path_to_buffer: HashMap<PathBuf, BufferId>,

    /// Next buffer ID to assign
    next_buffer_id: BufferId,

    /// Subscribers to diagnostic update notifications (Arc for cheap cloning)
    diagnostic_subscribers: Arc<[Sender<DiagnosticUpdate>]>,

    /// Subscribers to progress notifications (Arc for cheap cloning)
    progress_subscribers: Arc<[Sender<ProgressUpdate>]>,
}

struct ServerState {
    /// Server identifier
    id: ServerId,

    /// Transport for communication
    transport: Arc<dyn LspTransport>,

    /// Server name (e.g., "rust-analyzer")
    name: String,

    /// Text document sync kind (None, Full, or Incremental)
    text_document_sync_kind: Option<lsp_types::TextDocumentSyncKind>,
}

impl LspManager {
    /// Create a new LSP manager with the given executor and request timeout.
    ///
    /// The executor is used to spawn background tasks for listening to LSP notifications.
    /// The request_timeout specifies how long to wait for LSP requests before timing out.
    pub fn new(executor: gpui::BackgroundExecutor, request_timeout: std::time::Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LspManagerInner {
                servers: HashMap::new(),
                next_server_id: 0,
                pending_requests: HashMap::new(),
                next_request_id: Arc::new(AtomicU64::new(1)),
                lsp_diagnostics: HashMap::new(),
                buffer_paths: HashMap::new(),
                path_to_buffer: HashMap::new(),
                next_buffer_id: 0,
                diagnostic_subscribers: Arc::from(vec![]),
                progress_subscribers: Arc::from(vec![]),
            })),
            executor,
            request_timeout,
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
        let (tx, rx) = async_channel::bounded(DIAGNOSTIC_CHANNEL_CAPACITY);

        // Copy-on-write: clone Arc to Vec, push, create new Arc
        let mut inner = self.inner.lock();
        let mut subs = inner.diagnostic_subscribers.as_ref().to_vec();
        subs.push(tx);
        inner.diagnostic_subscribers = Arc::from(subs);

        rx
    }

    /// Subscribe to LSP progress notifications.
    ///
    /// Returns a channel that receives notifications when language servers
    /// report progress ($/progress). Used to track operations like indexing.
    pub fn subscribe_progress_updates(&self) -> Receiver<ProgressUpdate> {
        let (tx, rx) = async_channel::bounded(PROGRESS_CHANNEL_CAPACITY);

        // Copy-on-write: clone Arc to Vec, push, create new Arc
        let mut inner = self.inner.lock();
        let mut subs = inner.progress_subscribers.as_ref().to_vec();
        subs.push(tx);
        inner.progress_subscribers = Arc::from(subs);

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
                text_document_sync_kind: None,
            },
        );

        server_id
    }

    /// Set server capabilities after initialization.
    ///
    /// Parses the server capabilities response to determine what sync mode the server supports.
    /// Should be called after receiving the initialize response from the server.
    pub fn set_capabilities(&self, server_id: ServerId, capabilities: ServerCapabilities) {
        let mut inner = self.inner.lock();

        if let Some(server) = inner.servers.get_mut(&server_id) {
            server.text_document_sync_kind =
                capabilities.text_document_sync.and_then(|sync| match sync {
                    TextDocumentSyncCapability::Kind(kind) => Some(kind),
                    TextDocumentSyncCapability::Options(opts) => opts.change,
                });

            if let Some(kind) = server.text_document_sync_kind {
                tracing::info!("Server {} supports sync kind: {:?}", server_id, kind);
            }
        }
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
    /// Returns a handle that can be awaited to get the result, or cancelled to abort the request.
    ///
    /// # Arguments
    ///
    /// * `server_id` - Server to send request to
    /// * `request` - JSON-RPC request payload
    ///
    /// # Returns
    ///
    /// RequestHandle that can be awaited for JSON response or cancelled
    ///
    /// # Errors
    ///
    /// Returns error if the request times out, fails, or is cancelled
    pub fn request(
        &self,
        server_id: ServerId,
        request: serde_json::Value,
    ) -> Result<RequestHandle> {
        let (transport, request_id_counter) = {
            let inner = self.inner.lock();
            let transport = inner
                .servers
                .get(&server_id)
                .ok_or_else(|| anyhow::anyhow!("Server not found"))?
                .transport
                .clone();
            (transport, inner.next_request_id.clone())
        };

        let request_str = serde_json::to_string(&request)?;

        let timeout_duration = self.request_timeout;
        let executor = self.executor.clone();

        // Generate unique request ID
        let request_id = request_id_counter.fetch_add(1, Ordering::SeqCst);

        // Create the combined timeout + request future
        let combined_future = {
            let executor_clone = executor.clone();
            executor.spawn(async move {
                let timeout_future = executor_clone.spawn(async move {
                    smol::Timer::after(timeout_duration).await;
                    Err::<String, anyhow::Error>(anyhow::anyhow!(
                        "LSP request timed out after {:?} (server may be unresponsive)",
                        timeout_duration
                    ))
                });

                let request_future =
                    executor_clone.spawn(async move { transport.send_request(request_str).await });

                match futures::future::select(request_future, timeout_future).await {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right((timeout_err, _)) => timeout_err,
                }
            })
        };

        // Wrap in abortable to allow cancellation
        let (abortable_future, abort_handle) = futures::future::abortable(combined_future);

        // Track this request
        self.inner
            .lock()
            .pending_requests
            .insert(request_id, abort_handle);

        Ok(RequestHandle {
            request_id,
            manager: self.inner.clone(),
            future: abortable_future,
        })
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
        // Single-pass parsing using typed enum
        let notif: LspNotification =
            serde_json::from_str(notification).context("Failed to parse notification")?;

        match notif {
            LspNotification::PublishDiagnostics { params } => {
                Self::handle_publish_diagnostics(inner, server_id, params)?;
            },
            LspNotification::Progress { params } => {
                Self::handle_progress(inner, server_id, params)?;
            },
            LspNotification::Unknown => {
                // Ignore other notifications
            },
        }

        Ok(())
    }

    /// Compute hash of diagnostics for change detection.
    ///
    /// Hashes server ID, range, message, and severity to detect identical diagnostic sets.
    fn compute_diagnostic_hash(diagnostics: &[(ServerId, lsp_types::Diagnostic)]) -> u64 {
        let mut hasher = FxHasher::default();

        for (server_id, diag) in diagnostics {
            server_id.hash(&mut hasher);
            diag.range.start.line.hash(&mut hasher);
            diag.range.start.character.hash(&mut hasher);
            diag.range.end.line.hash(&mut hasher);
            diag.range.end.character.hash(&mut hasher);
            diag.message.hash(&mut hasher);
            // DiagnosticSeverity is a newtype, hash the inner value
            if let Some(severity) = diag.severity {
                use lsp_types::DiagnosticSeverity;
                let severity_u8: u8 = match severity {
                    DiagnosticSeverity::ERROR => 1,
                    DiagnosticSeverity::WARNING => 2,
                    DiagnosticSeverity::INFORMATION => 3,
                    DiagnosticSeverity::HINT => 4,
                    _ => 0, // Unknown severity
                };
                severity_u8.hash(&mut hasher);
            }
        }

        hasher.finish()
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

        let (changed, subscribers) = {
            let mut inner_guard = inner.lock();

            // Get or create buffer ID for this path
            let buffer_id = if let Some(&id) = inner_guard.path_to_buffer.get(&path) {
                id
            } else {
                let id = inner_guard.next_buffer_id;
                inner_guard.next_buffer_id += 1;
                inner_guard.buffer_paths.insert(id, path.clone());
                inner_guard.path_to_buffer.insert(path.clone(), id);
                id
            };

            let diagnostics_for_file = inner_guard
                .lsp_diagnostics
                .entry(buffer_id)
                .or_insert_with(Vec::new);

            // Compute hash before update
            let old_hash = Self::compute_diagnostic_hash(diagnostics_for_file);

            // Remove old diagnostics from this server for this file
            diagnostics_for_file.retain(|(sid, _)| *sid != server_id);

            // Add new diagnostics from this server
            for diag in params.diagnostics {
                diagnostics_for_file.push((server_id, diag));
            }

            // Compute hash after update
            let new_hash = Self::compute_diagnostic_hash(diagnostics_for_file);

            // Clean up closed channels
            let mut subs = inner_guard.diagnostic_subscribers.as_ref().to_vec();
            let old_len = subs.len();
            subs.retain(|tx| !tx.is_closed());
            if subs.len() != old_len {
                inner_guard.diagnostic_subscribers = Arc::from(subs);
            }

            (
                old_hash != new_hash,
                inner_guard.diagnostic_subscribers.clone(), // Arc clone (cheap)
            )
        };

        // Only notify if diagnostics actually changed
        if changed {
            let update = DiagnosticUpdate { path, server_id };

            // Send to all subscribers with backpressure handling
            for tx in subscribers.iter() {
                match tx.try_send(update.clone()) {
                    Ok(_) => {},
                    Err(async_channel::TrySendError::Full(_)) => {
                        tracing::warn!(
                            "Diagnostic channel full, dropping update for {}",
                            update.path.display()
                        );
                    },
                    Err(async_channel::TrySendError::Closed(_)) => {
                        // Subscriber closed, will be cleaned up on next notification
                    },
                }
            }
        }

        Ok(())
    }

    /// Handle $/progress notification from language server.
    fn handle_progress(
        inner: &Arc<Mutex<LspManagerInner>>,
        server_id: ServerId,
        params: ProgressNotificationParams,
    ) -> Result<()> {
        let token = params.token.to_string();

        let kind = match params.value.kind.as_str() {
            "begin" => ProgressKind::Begin,
            "report" => ProgressKind::Report,
            "end" => ProgressKind::End,
            _ => return Ok(()), // Ignore unknown kinds
        };

        let title = params.value.title;
        let message = params.value.message;
        let percentage = params.value.percentage;

        let update = ProgressUpdate {
            server_id,
            token,
            kind,
            title,
            message,
            percentage,
        };

        // Notify subscribers (clean up closed channels and clone Arc)
        let subscribers = {
            let mut inner_guard = inner.lock();

            // Clean up closed channels
            let mut subs = inner_guard.progress_subscribers.as_ref().to_vec();
            let old_len = subs.len();
            subs.retain(|tx| !tx.is_closed());
            if subs.len() != old_len {
                inner_guard.progress_subscribers = Arc::from(subs);
            }

            inner_guard.progress_subscribers.clone() // Arc clone (cheap)
        };

        for tx in subscribers.iter() {
            match tx.try_send(update.clone()) {
                Ok(_) => {},
                Err(async_channel::TrySendError::Full(_)) => {
                    tracing::warn!(
                        "Progress channel full, dropping update for server {} token {}",
                        update.server_id,
                        update.token
                    );
                },
                Err(async_channel::TrySendError::Closed(_)) => {
                    // Subscriber closed, will be cleaned up on next notification
                },
            }
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
        // Clone raw diagnostics while holding lock, convert outside lock
        let lsp_diags = {
            let inner = self.inner.lock();
            let buffer_id = inner.path_to_buffer.get(path)?;
            inner.lsp_diagnostics.get(buffer_id)?.clone()
        };
        // Lock released here - allows concurrent diagnostic fetches

        let mut set = DiagnosticSet::new();

        for (server_id, lsp_diag) in &lsp_diags {
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

    /// Send didChange notification with incremental or full sync.
    ///
    /// Uses incremental sync if the server supports it, otherwise falls back to full sync.
    /// The method checks the server's `text_document_sync_kind` capability and sends either
    /// individual edits or the full document content accordingly.
    ///
    /// # Note
    ///
    /// This requires editor-level change tracking to be fully utilized. Currently, the editor
    /// must provide `Edit<Point>` events when the buffer changes.
    pub async fn did_change_incremental(
        &self,
        server_id: ServerId,
        uri: Uri,
        version: i32,
        edits: Vec<text::Edit<text::Point>>,
        snapshot: &BufferSnapshot,
    ) -> Result<()> {
        let (transport, sync_kind) = {
            let inner = self.inner.lock();
            let server = inner.servers.get(&server_id).context("Server not found")?;
            (server.transport.clone(), server.text_document_sync_kind)
        };

        let content_changes = match sync_kind {
            Some(TextDocumentSyncKind::INCREMENTAL) => edits
                .iter()
                .map(|edit| {
                    let range = crate::conversion::point_range_to_lsp(&edit.new, snapshot);
                    let text: String = snapshot.text_for_range(edit.new.clone()).collect();

                    TextDocumentContentChangeEvent {
                        range: Some(range),
                        range_length: None,
                        text,
                    }
                })
                .collect(),
            _ => {
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: snapshot.text(),
                }]
            },
        };

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes,
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
