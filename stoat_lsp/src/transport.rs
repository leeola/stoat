//! LSP transport layer abstractions.
//!
//! The [`LspTransport`] trait enables dependency injection for testing,
//! allowing both real process-based communication ([`StdioTransport`])
//! and mock servers ([`MockLspServer`](crate::test::MockLspServer)) to be used
//! interchangeably.

use anyhow::Result;
use async_trait::async_trait;
use futures::{
    io::{BufReader, BufWriter},
    AsyncBufReadExt, AsyncWriteExt, Stream,
};
use smol::process::{Child, Command, Stdio};
use std::{path::PathBuf, pin::Pin};

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
    process: Child,
    stdin: BufWriter<smol::process::ChildStdin>,
    stdout: BufReader<smol::process::ChildStdout>,
}

impl StdioTransport {
    /// Spawn a new LSP server process.
    ///
    /// # Arguments
    ///
    /// * `command` - Path to the LSP server executable
    /// * `args` - Command-line arguments
    ///
    /// # Errors
    ///
    /// Returns error if process fails to spawn.
    pub fn spawn(command: PathBuf, args: Vec<String>) -> Result<Self> {
        let mut process = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = BufWriter::new(
            process
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture stdin"))?,
        );

        let stdout = BufReader::new(
            process
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout"))?,
        );

        Ok(Self {
            process,
            stdin,
            stdout,
        })
    }

    /// Write a JSON-RPC message with Content-Length framing.
    async fn write_message(&mut self, message: &str) -> Result<()> {
        let header = format!("Content-Length: {}\r\n\r\n", message.len());
        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(message.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read a JSON-RPC message, stripping Content-Length framing.
    async fn read_message(&mut self) -> Result<String> {
        let mut header = String::new();
        self.stdout.read_line(&mut header).await?;

        if !header.starts_with("Content-Length: ") {
            anyhow::bail!("Invalid message header: {}", header);
        }

        let content_length: usize = header["Content-Length: ".len()..]
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid Content-Length: {}", e))?;

        // Read empty line
        let mut empty = String::new();
        self.stdout.read_line(&mut empty).await?;

        // Read message body
        let mut buffer = vec![0u8; content_length];
        use futures::AsyncReadExt;
        self.stdout.read_exact(&mut buffer).await?;

        Ok(String::from_utf8(buffer)?)
    }
}

#[async_trait]
impl LspTransport for StdioTransport {
    async fn send_request(&self, _request: String) -> Result<String> {
        // FIXME: Implement request/response correlation with request IDs
        todo!("StdioTransport request/response handling")
    }

    async fn send_notification(&self, _notification: String) -> Result<()> {
        // FIXME: Implement notification sending
        todo!("StdioTransport notification sending")
    }

    fn subscribe_notifications(&self) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        // FIXME: Implement notification stream from stdout
        todo!("StdioTransport notification stream")
    }

    async fn shutdown(&self) -> Result<()> {
        // FIXME: Send shutdown request and kill process
        todo!("StdioTransport shutdown")
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
