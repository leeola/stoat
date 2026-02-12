//! LSP transport layer abstractions.
//!
//! The [`LspTransport`] trait enables dependency injection for testing,
//! allowing both real process-based communication ([`StdioTransport`])
//! and mock servers ([`MockLspServer`](crate::test::MockLspServer)) to be used
//! interchangeably.

use anyhow::Result;
use async_channel::{unbounded, Receiver, Sender};
use async_trait::async_trait;
use futures::{
    io::{BufReader, BufWriter},
    lock::Mutex as AsyncMutex,
    AsyncBufReadExt, AsyncWriteExt, Stream,
};
use parking_lot::Mutex;
use smol::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::{
    collections::HashMap,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Abstraction for LSP communication.
///
/// Enables dependency injection: production code uses [`StdioTransport`]
/// while tests use [`MockLspServer`](crate::test::MockLspServer).
#[async_trait]
pub trait LspTransport: Send + Sync {
    /// Send a request and wait for response.
    ///
    /// The request string should be a complete JSON-RPC message without
    /// the Content-Length header (the transport adds framing).
    async fn send_request(&self, request: String) -> Result<String>;

    /// Send a notification (no response expected).
    ///
    /// The notification string should be a complete JSON-RPC message without
    /// the Content-Length header.
    async fn send_notification(&self, notification: String) -> Result<()>;

    /// Subscribe to server-initiated notifications.
    ///
    /// Returns a stream of notification messages (without Content-Length headers).
    fn subscribe_notifications(&self) -> Pin<Box<dyn Stream<Item = String> + Send>>;

    /// Shutdown the transport.
    ///
    /// For process-based transports, this sends shutdown request and kills
    /// the process. For mock transports, this is a no-op.
    async fn shutdown(&self) -> Result<()>;

    /// Get buffered notifications (test-only).
    ///
    /// Returns notifications that have been buffered but not yet consumed via subscription.
    /// Only implemented for mock transports. Production transports return empty vec.
    #[cfg(feature = "test-support")]
    fn buffered_notifications(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Production LSP transport using process stdin/stdout.
///
/// Communicates with an LSP server process using the standard JSON-RPC
/// protocol with Content-Length framing.
///
/// # Architecture
///
/// Uses a background reader task that continuously reads from the server's stdout
/// and routes messages to the appropriate handlers:
/// - Responses (with `id` field) are routed to waiting request futures via channels
/// - Notifications (without `id` field) are broadcast to subscribers
///
/// # Protocol
///
/// Messages are framed using HTTP-style headers:
///
/// ```text
/// Content-Length: 123\r\n
/// \r\n
/// {"jsonrpc":"2.0",...}
/// ```
pub struct StdioTransport {
    /// Shared stdin for sending messages (async-aware mutex)
    stdin: Arc<AsyncMutex<BufWriter<ChildStdin>>>,

    /// Monotonically increasing request ID generator
    next_request_id: AtomicU64,

    /// Maps request IDs to response channels
    pending_requests: Arc<Mutex<HashMap<u64, Sender<String>>>>,

    /// Broadcasts server-initiated notifications
    _notification_tx: Sender<String>,
    notification_rx: Receiver<String>,

    /// Background task reading stdout
    _reader_task: gpui::Task<Result<()>>,

    /// Process handle for shutdown
    process: Arc<Mutex<Option<Child>>>,
}

impl StdioTransport {
    /// Spawn a new LSP server process with background reader.
    ///
    /// # Arguments
    ///
    /// * `command` - Path to the LSP server executable
    /// * `args` - Command-line arguments
    /// * `executor` - Executor for spawning background tasks
    ///
    /// # Errors
    ///
    /// Returns error if process fails to spawn.
    pub fn spawn(
        command: PathBuf,
        args: Vec<String>,
        env: Option<HashMap<String, String>>,
        executor: gpui::BackgroundExecutor,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = env {
            cmd.env_clear().envs(env);
        }
        let mut process = cmd.spawn()?;

        let stdin = Arc::new(AsyncMutex::new(BufWriter::new(
            process
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture stdin"))?,
        )));

        let stdout = BufReader::new(
            process
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout"))?,
        );

        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let (notification_tx, notification_rx) = unbounded();

        let pending_requests_clone = pending_requests.clone();
        let notification_tx_clone = notification_tx.clone();

        let reader_task = executor.spawn(async move {
            Self::reader_loop(stdout, pending_requests_clone, notification_tx_clone).await
        });

        Ok(Self {
            stdin,
            next_request_id: AtomicU64::new(1),
            pending_requests,
            _notification_tx: notification_tx,
            notification_rx,
            _reader_task: reader_task,
            process: Arc::new(Mutex::new(Some(process))),
        })
    }

    /// Background loop that reads messages from stdout and routes them.
    async fn reader_loop(
        mut stdout: BufReader<ChildStdout>,
        pending_requests: Arc<Mutex<HashMap<u64, Sender<String>>>>,
        notification_tx: Sender<String>,
    ) -> Result<()> {
        loop {
            let message = match Self::read_message_from_stdout(&mut stdout).await {
                Ok(msg) => msg,
                Err(e) => {
                    tracing::error!("Failed to read LSP message: {}", e);
                    break;
                },
            };

            let json: serde_json::Value = match serde_json::from_str(&message) {
                Ok(j) => j,
                Err(e) => {
                    tracing::error!("Failed to parse LSP message as JSON: {}", e);
                    continue;
                },
            };

            if let Some(id) = json.get("id").and_then(|v| v.as_u64()) {
                let tx_opt = pending_requests.lock().remove(&id);
                if let Some(tx) = tx_opt {
                    let _ = tx.send(message).await;
                }
            } else if json.get("method").is_some() {
                let _ = notification_tx.send(message).await;
            }
        }

        Ok(())
    }

    /// Read a JSON-RPC message from stdout, stripping Content-Length framing.
    async fn read_message_from_stdout(stdout: &mut BufReader<ChildStdout>) -> Result<String> {
        let mut header = String::new();
        stdout.read_line(&mut header).await?;

        if !header.starts_with("Content-Length: ") {
            anyhow::bail!("Invalid message header: {header}");
        }

        let content_length: usize = header["Content-Length: ".len()..]
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid Content-Length: {e}"))?;

        let mut empty = String::new();
        stdout.read_line(&mut empty).await?;

        let mut buffer = vec![0u8; content_length];
        use futures::AsyncReadExt;
        stdout.read_exact(&mut buffer).await?;

        Ok(String::from_utf8(buffer)?)
    }

    /// Write a JSON-RPC message to stdin with Content-Length framing.
    async fn write_message_to_stdin(&self, message: &str) -> Result<()> {
        let header = format!("Content-Length: {}\r\n\r\n", message.len());
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(message.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

#[async_trait]
impl LspTransport for StdioTransport {
    async fn send_request(&self, request: String) -> Result<String> {
        let mut json: serde_json::Value = serde_json::from_str(&request)?;

        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        json["id"] = serde_json::json!(request_id);

        let request_with_id = serde_json::to_string(&json)?;

        let (response_tx, response_rx) = unbounded();
        self.pending_requests.lock().insert(request_id, response_tx);

        self.write_message_to_stdin(&request_with_id).await?;

        let response = smol::future::or(
            async {
                response_rx
                    .recv()
                    .await
                    .map_err(|_| anyhow::anyhow!("Channel closed"))
            },
            async {
                smol::Timer::after(std::time::Duration::from_secs(30)).await;
                Err(anyhow::anyhow!("Request timeout"))
            },
        )
        .await?;

        Ok(response)
    }

    async fn send_notification(&self, notification: String) -> Result<()> {
        self.write_message_to_stdin(&notification).await
    }

    fn subscribe_notifications(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        Box::pin(self.notification_rx.clone())
    }

    async fn shutdown(&self) -> Result<()> {
        let shutdown_request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "shutdown",
        });

        let _ = self
            .send_request(serde_json::to_string(&shutdown_request)?)
            .await;

        let exit_notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
        });

        let _ = self
            .send_notification(serde_json::to_string(&exit_notification)?)
            .await;

        if let Some(mut process) = self.process.lock().take() {
            let _ = process.kill();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_trait_is_object_safe() {
        // Compile-time check that LspTransport can be used as a trait object
        let _: Option<Box<dyn LspTransport>> = None;
    }
}
