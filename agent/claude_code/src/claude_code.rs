pub mod builder;
mod control;
pub mod process;

pub use self::builder::ClaudeCodeBuilder;
use self::process::Process;
use crate::messages::{PermissionMode, SdkMessage, SettingSource};
use anyhow::{Context, Result};
use std::{collections::VecDeque, path::PathBuf, sync::Mutex as StdMutex, time::Duration};
use stoat::host::AgentMessage;
use tokio::sync::{Mutex as TokioMutex, mpsc};
use tracing::info;

#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    pub max_turns: Option<u32>,
    pub cwd: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub permission_mode: Option<PermissionMode>,
    pub session_id: Option<uuid::Uuid>,
    pub model: Option<String>,
    pub append_system_prompt: Option<String>,
    pub append_system_prompt_file: Option<PathBuf>,
    pub add_dirs: Vec<PathBuf>,
    pub mcp_config: Option<String>,
    pub setting_sources: Vec<SettingSource>,
    pub include_partial_messages: bool,
    pub include_hook_events: bool,
    pub bare: bool,
    pub fork_session: bool,
    pub dangerously_skip_permissions: bool,
    pub init_timeout: Option<Duration>,
    pub extra_args: Vec<(String, Option<String>)>,
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

    managed_session_id: uuid::Uuid,
}

impl ClaudeCode {
    /// Create a new builder for ClaudeCode
    pub fn builder() -> ClaudeCodeBuilder {
        ClaudeCodeBuilder::new()
    }

    /// Create a new ClaudeCode with the given configuration
    pub async fn new(config: SessionConfig) -> Result<Self> {
        ClaudeCodeBuilder::new().with_config(config).build().await
    }

    /// Create ClaudeCode from a Process instance with communication channels
    pub(crate) fn from_process(
        process: Process,
        process_stdin_tx: mpsc::Sender<String>,
        process_stdout_rx: mpsc::Receiver<SdkMessage>,
        session_id: uuid::Uuid,
    ) -> Self {
        info!("ClaudeCode instance created for session: {}", session_id);

        Self {
            process: StdMutex::new(Some(process)),
            process_stdin_tx,
            process_stdout_rx: TokioMutex::new(process_stdout_rx),
            pending: StdMutex::new(VecDeque::new()),
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
    pub(crate) async fn shutdown_inner(&self) -> Result<()> {
        info!(
            "Shutting down ClaudeCode for session: {}",
            self.managed_session_id
        );
        let maybe_process = self
            .process
            .lock()
            .expect("ClaudeCode process mutex poisoned")
            .take();
        if let Some(process) = maybe_process {
            let _ = process.close().await;
        }
        Ok(())
    }

    /// Get the current session ID.
    pub fn get_session_id(&self) -> String {
        self.managed_session_id.to_string()
    }

    /// Check if the Claude process is still alive. Inherent counterpart
    /// of [`ClaudeCodeHost::is_alive`].
    pub(crate) fn is_alive_inner(&self) -> bool {
        let mut guard = self
            .process
            .lock()
            .expect("ClaudeCode process mutex poisoned");
        if let Some(process) = guard.as_mut() {
            process.is_alive() && !self.process_stdin_tx.is_closed()
        } else {
            false
        }
    }
}
