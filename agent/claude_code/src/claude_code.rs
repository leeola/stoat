pub mod builder;
pub mod process;

pub use self::builder::ClaudeCodeBuilder;
use self::process::{Process, ProcessBuilder as ProcessBuilderInner};
use crate::messages::{MessageContent, PermissionMode, SdkMessage};
use anyhow::{Context, Result};
use std::{collections::VecDeque, sync::Mutex as StdMutex};
use stoat::host::AgentMessage;
use tokio::{
    sync::{Mutex as TokioMutex, mpsc},
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
    /// Active subprocess handle. Wrapped in a sync [`StdMutex`] because
    /// liveness checks and shutdown only need brief, non-`.await` access.
    process: StdMutex<Option<Process>>,

    /// Stream-JSON messages outbound to Claude. `Sender` is `Clone +
    /// Send + Sync`, so no wrapper is required.
    process_stdin_tx: mpsc::Sender<String>,

    /// Stream-JSON messages inbound from Claude. Wrapped in a
    /// [`TokioMutex`] so `recv().await` can be held across an `.await`
    /// point without blocking the runtime.
    pub(crate) process_stdout_rx: TokioMutex<mpsc::Receiver<SdkMessage>>,

    /// Buffered [`AgentMessage`]s produced by expanding a single wire
    /// [`SdkMessage`] into multiple host messages. Drained by the
    /// `ClaudeCodeHost::recv` adapter before pulling the next wire
    /// message.
    pub(crate) pending: StdMutex<VecDeque<AgentMessage>>,

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
            process: StdMutex::new(Some(process)),
            process_stdin_tx,
            process_stdout_rx: TokioMutex::new(process_stdout_rx),
            pending: StdMutex::new(VecDeque::new()),
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

    /// Shut the subprocess down. Inherent counterpart of
    /// [`ClaudeCodeHost::shutdown`] so the trait impl can delegate here
    /// without a name collision.
    pub async fn shutdown_inner(&self) -> Result<()> {
        info!(
            "Shutting down ClaudeCode for session: {}",
            self.managed_session_id
        );
        let maybe_process = self.process.lock().unwrap().take();
        if let Some(process) = maybe_process {
            let _ = process.close().await;
        }
        Ok(())
    }

    /// Wait for the next assistant response with a timeout
    pub async fn wait_for_response(&self, duration: Duration) -> Result<Option<String>> {
        let deadline = tokio::time::Instant::now() + duration;

        while tokio::time::Instant::now() < deadline {
            let mut rx = self.process_stdout_rx.lock().await;
            match timeout(deadline - tokio::time::Instant::now(), rx.recv()).await {
                Ok(Some(msg)) => {
                    // Release the lock before returning so other consumers
                    // can make progress.
                    drop(rx);
                    if let SdkMessage::Assistant { message, .. } = msg {
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
    pub async fn recv_any_message(&self, duration: Duration) -> Result<Option<SdkMessage>> {
        let mut rx = self.process_stdout_rx.lock().await;
        match timeout(duration, rx.recv()).await {
            Ok(Some(msg)) => Ok(Some(msg)),
            Ok(None) => {
                debug!("Channel closed while receiving message");
                Ok(None)
            },
            Err(_) => Ok(None),
        }
    }

    /// Get the current session ID
    pub fn get_session_id(&self) -> String {
        self.managed_session_id.to_string()
    }

    /// Check if the Claude process is still alive. Inherent counterpart
    /// of [`ClaudeCodeHost::is_alive`].
    pub fn is_alive_inner(&self) -> bool {
        let mut guard = self.process.lock().unwrap();
        if let Some(process) = guard.as_mut() {
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
        let current_process = self.process.lock().unwrap().take();
        if let Some(current_process) = current_process {
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
                Err(e) => {
                    anyhow::bail!("Failed to resume session with new model: {:?}", e);
                },
            };

            // Just update the process - channels and buffer task remain the same!
            // The buffer task is still reading from the same channel that's connected
            // to the new process through the recovered channels.
            *self.process.lock().unwrap() = Some(new_process);
            info!("Model switch completed successfully");
        } else {
            anyhow::bail!("No active process to switch model");
        }

        Ok(())
    }
}
