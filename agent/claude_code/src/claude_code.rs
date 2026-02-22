pub mod builder;
pub mod process;

pub use self::builder::ClaudeCodeBuilder;
use self::process::{Process, ProcessBuilder as ProcessBuilderInner};
use crate::messages::{PermissionMode, SdkMessage, UserMessage};
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use async_io::Timer;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
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
    pub log_dir: Option<PathBuf>,
}

impl SessionConfig {
    pub(crate) fn apply_to(&self, mut builder: ProcessBuilderInner) -> ProcessBuilderInner {
        if let Some(model) = &self.model {
            builder = builder.model(model.clone());
        }
        if let Some(max_turns) = self.max_turns {
            builder = builder.max_turns(max_turns);
        }
        if let Some(cwd) = &self.cwd {
            builder = builder.cwd(cwd.clone());
        }
        if let Some(tools) = &self.allowed_tools {
            builder = builder.allowed_tools(tools.clone());
        }
        if let Some(mode) = &self.permission_mode {
            builder = builder.permission_mode(mode.clone());
        }
        if let Some(dir) = &self.log_dir {
            builder = builder.log_dir(dir.clone());
        }
        builder
    }
}

pub struct ClaudeCode {
    process: Option<Process>,
    process_stdin_tx: Sender<String>,
    process_stdout_rx: Receiver<SdkMessage>,
    current_config: SessionConfig,
    managed_session_id: uuid::Uuid,
    log_file: Option<PathBuf>,
}

impl ClaudeCode {
    pub fn builder() -> ClaudeCodeBuilder {
        ClaudeCodeBuilder::new()
    }

    pub async fn new(config: SessionConfig) -> Result<Self> {
        ClaudeCodeBuilder::from_config(config).build().await
    }

    pub(crate) fn from_process(
        process: Process,
        process_stdin_tx: Sender<String>,
        process_stdout_rx: Receiver<SdkMessage>,
        config: SessionConfig,
        session_id: uuid::Uuid,
    ) -> Self {
        info!("ClaudeCode instance created for session: {}", session_id);

        let log_file = config
            .log_dir
            .as_ref()
            .map(|dir| dir.join(format!("claude-{session_id}.jsonl")));

        Self {
            process: Some(process),
            process_stdin_tx,
            process_stdout_rx,
            current_config: config,
            managed_session_id: session_id,
            log_file,
        }
    }

    pub async fn send_message(&self, content: &str) -> Result<()> {
        let msg = SdkMessage::User {
            message: UserMessage::from_text(content),
            session_id: self.managed_session_id.to_string(),
        };
        let json = serde_json::to_string(&msg)?;
        self.process_stdin_tx
            .send(json)
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send message to Claude Code"))?;
        Ok(())
    }

    pub fn shutdown(mut self) -> Result<()> {
        info!(
            "Shutting down ClaudeCode for session: {}",
            self.managed_session_id
        );
        if let Some(process) = self.process.take() {
            let _ = process.close();
        }
        Ok(())
    }

    pub async fn wait_for_response(&mut self, duration: Duration) -> Result<Option<String>> {
        let deadline = Instant::now() + duration;

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let recv = self.process_stdout_rx.recv();
            let timeout = Timer::after(remaining);
            match futures_lite::future::or(async { (recv.await).ok() }, async {
                timeout.await;
                None
            })
            .await
            {
                Some(msg) => {
                    if let SdkMessage::Assistant { message, .. } = msg {
                        return Ok(Some(message.get_text_content()));
                    }
                    continue;
                },
                None => {
                    debug!("Timeout waiting for assistant response");
                    return Ok(None);
                },
            }
        }

        Ok(None)
    }

    pub async fn recv_any_message(&mut self, duration: Duration) -> Result<Option<SdkMessage>> {
        let recv = self.process_stdout_rx.recv();
        let timeout = Timer::after(duration);
        match futures_lite::future::or(async { (recv.await).ok() }, async {
            timeout.await;
            None
        })
        .await
        {
            Some(msg) => Ok(Some(msg)),
            None => Ok(None),
        }
    }

    pub fn get_session_id(&self) -> String {
        self.managed_session_id.to_string()
    }

    pub fn log_file(&self) -> Option<&Path> {
        self.log_file.as_deref()
    }

    pub fn is_alive(&mut self) -> bool {
        if let Some(process) = self.process.as_mut() {
            process.is_alive() && !self.process_stdin_tx.is_closed()
        } else {
            false
        }
    }

    pub fn permission_mode(&self) -> Option<&PermissionMode> {
        self.current_config.permission_mode.as_ref()
    }

    async fn restart_with_config(&mut self, context: &str) -> Result<()> {
        if let Some(current_process) = self.process.take() {
            let recovered = current_process
                .close()
                .context("Failed to close current process")?;

            Timer::after(Duration::from_millis(100)).await;

            let process_builder = ProcessBuilderInner::new(recovered.stdin_rx, recovered.stdout_tx)
                .session_id(self.managed_session_id.to_string());
            let process_builder = self.current_config.apply_to(process_builder);

            let new_process = match process_builder.resume_session().await {
                Ok(process) => process,
                Err((_channels, e)) => {
                    anyhow::bail!("Failed to resume session after {context}: {e:?}");
                },
            };

            self.process = Some(new_process);
            info!("{} completed successfully", context);
        } else {
            anyhow::bail!("No active process for {}", context);
        }

        Ok(())
    }

    pub async fn switch_model(&mut self, model: impl Into<String>) -> Result<()> {
        let model_str = model.into();
        info!(
            "Switching model from {:?} to {}",
            self.current_config.model, model_str
        );
        self.current_config.model = Some(model_str);
        self.restart_with_config("model switch").await
    }

    pub async fn switch_permission_mode(&mut self, mode: PermissionMode) -> Result<()> {
        info!(
            "Switching permission mode from {:?} to {:?}",
            self.current_config.permission_mode, mode
        );
        self.current_config.permission_mode = Some(mode);
        self.restart_with_config("permission mode switch").await
    }
}
