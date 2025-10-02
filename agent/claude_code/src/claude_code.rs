pub mod builder;
pub mod process;

pub use self::builder::ClaudeCodeBuilder;
use self::process::{Process, ProcessBuilder as ProcessBuilderInner};
use crate::messages::{MessageContent, PermissionMode, SdkMessage};
use anyhow::{Context, Result};
use tokio::{
    sync::mpsc,
    time::{Duration, timeout},
};
use tracing::{debug, info};

#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    pub max_turns: Option<u32>,
    pub cwd: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub permission_mode: Option<PermissionMode>,
    pub session_id: Option<uuid::Uuid>,
    pub model: Option<String>,
}

pub struct ClaudeCode {
    process: Option<Process>,
    // Channels for communicating with Process
    process_stdin_tx: mpsc::Sender<String>,
    process_stdout_rx: mpsc::Receiver<SdkMessage>,
    current_config: SessionConfig,
    managed_session_id: uuid::Uuid,
}

impl ClaudeCode {
    /// Create a new builder for ClaudeCode
    pub fn builder() -> ClaudeCodeBuilder {
        ClaudeCodeBuilder::new()
    }

    /// Create a new ClaudeCode with the given configuration
    pub async fn new(config: SessionConfig) -> Result<Self> {
        let mut builder = ClaudeCodeBuilder::new();

        if let Some(model) = config.model {
            builder = builder.model(model);
        }
        if let Some(session_id) = config.session_id {
            builder = builder.session_id(session_id.to_string());
        }
        if let Some(max_turns) = config.max_turns {
            builder = builder.max_turns(max_turns);
        }
        if let Some(cwd) = config.cwd {
            builder = builder.cwd(cwd);
        }
        if let Some(tools) = config.allowed_tools {
            builder = builder.allowed_tools(tools);
        }
        if let Some(mode) = config.permission_mode {
            builder = builder.permission_mode(mode);
        }

        builder.build().await
    }

    /// Create ClaudeCode from a Process instance with communication channels
    pub(crate) fn from_process(
        process: Process,
        process_stdin_tx: mpsc::Sender<String>,
        process_stdout_rx: mpsc::Receiver<SdkMessage>,
        config: SessionConfig,
        session_id: uuid::Uuid,
    ) -> Self {
        info!("ClaudeCode instance created for session: {}", session_id);

        Self {
            process: Some(process),
            process_stdin_tx,
            process_stdout_rx,
            current_config: config,
            managed_session_id: session_id,
        }
    }

    pub async fn send_message(&self, content: &str) -> Result<()> {
        // Create user message in stream-json format
        let user_msg = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": content
                    }
                ]
            }
        });

        let message = serde_json::to_string(&user_msg)?;
        self.process_stdin_tx
            .send(message)
            .await
            .context("Failed to send message to Claude Code")?;
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<()> {
        info!(
            "Shutting down ClaudeCode for session: {}",
            self.managed_session_id
        );
        if let Some(process) = self.process.take() {
            // Close process and discard recovered channels
            let _ = process.close().await;
        }
        Ok(())
    }

    /// Wait for the next assistant response with a timeout
    pub async fn wait_for_response(&mut self, duration: Duration) -> Result<Option<String>> {
        let deadline = tokio::time::Instant::now() + duration;

        while tokio::time::Instant::now() < deadline {
            match timeout(
                deadline - tokio::time::Instant::now(),
                self.process_stdout_rx.recv(),
            )
            .await
            {
                Ok(Some(msg)) => {
                    // Check if it's an assistant message
                    if let SdkMessage::Assistant { message, .. } = msg {
                        // Extract text content for convenience
                        let content = message
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                MessageContent::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        return Ok(Some(content));
                    }
                    // Continue for non-assistant messages
                    continue;
                },
                Ok(None) => {
                    debug!("Channel closed while waiting for response");
                    return Ok(None);
                },
                Err(_) => {
                    debug!("Timeout waiting for assistant response");
                    return Ok(None);
                },
            }
        }

        Ok(None)
    }

    /// Receive any type of message with a timeout
    pub async fn recv_any_message(&mut self, duration: Duration) -> Result<Option<SdkMessage>> {
        match timeout(duration, self.process_stdout_rx.recv()).await {
            Ok(Some(msg)) => Ok(Some(msg)),
            Ok(None) => {
                debug!("Channel closed while receiving message");
                Ok(None)
            },
            Err(_) => {
                // Timeout is not an error, just no message available
                Ok(None)
            },
        }
    }

    /// Get the current session ID
    pub fn get_session_id(&self) -> String {
        self.managed_session_id.to_string()
    }

    /// Check if the Claude process is still alive
    pub async fn is_alive(&mut self) -> bool {
        // Check both process and channel status
        if let Some(process) = self.process.as_mut() {
            process.is_alive() && !self.process_stdin_tx.is_closed()
        } else {
            false
        }
    }

    /// Switch to a different model at runtime
    pub async fn switch_model(&mut self, model: impl Into<String>) -> Result<()> {
        let model_str = model.into();
        info!(
            "Switching model from {:?} to {}",
            self.current_config.model, model_str
        );

        // Update config with new model
        self.current_config.model = Some(model_str.clone());

        // Take the current process
        if let Some(current_process) = self.process.take() {
            // Close current process and recover channels
            let recovered = current_process
                .close()
                .await
                .context("Failed to close current process")?;

            // Wait a bit for the process to fully terminate
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Build new process with the RECOVERED channels
            // This reuses the same channels - ClaudeCode's stdin_tx and stdout_rx stay connected!
            let mut process_builder =
                ProcessBuilderInner::new(recovered.stdin_rx, recovered.stdout_tx)
                    .session_id(self.managed_session_id.to_string())
                    .model(model_str);

            if let Some(max_turns) = self.current_config.max_turns {
                process_builder = process_builder.max_turns(max_turns);
            }
            if let Some(cwd) = &self.current_config.cwd {
                process_builder = process_builder.cwd(cwd.clone());
            }
            if let Some(tools) = &self.current_config.allowed_tools {
                process_builder = process_builder.allowed_tools(tools.clone());
            }
            if let Some(mode) = &self.current_config.permission_mode {
                process_builder = process_builder.permission_mode(mode.clone());
            }

            let new_process = match process_builder.resume_session().await {
                Ok(process) => process,
                Err((_channels, e)) => {
                    anyhow::bail!("Failed to resume session with new model: {e:?}");
                },
            };

            // Just update the process - channels and buffer task remain the same!
            // The buffer task is still reading from the same channel that's connected
            // to the new process through the recovered channels.
            self.process = Some(new_process);
            info!("Model switch completed successfully");
        } else {
            anyhow::bail!("No active process to switch model");
        }

        Ok(())
    }
}
