use crate::{
    claude_code::{ClaudeCode, SessionConfig, process::ProcessBuilder as ProcessBuilderInner},
    messages::PermissionMode,
};
use anyhow::Result;

#[derive(Debug, Default)]
pub struct ClaudeCodeBuilder {
    config: SessionConfig,
    managed_session_id: Option<uuid::Uuid>,
}

impl ClaudeCodeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_config(config: SessionConfig) -> Self {
        let managed_session_id = config.session_id;
        Self {
            config,
            managed_session_id,
        }
    }

    pub fn max_turns(mut self, max_turns: u32) -> Self {
        self.config.max_turns = Some(max_turns);
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.config.cwd = Some(cwd.into());
        self
    }

    pub fn allowed_tools(mut self, tools: Vec<String>) -> Self {
        self.config.allowed_tools = Some(tools);
        self
    }

    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.config.permission_mode = Some(mode);
        self
    }

    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        let id_str = session_id.into();
        if let Ok(uuid) = uuid::Uuid::parse_str(&id_str) {
            self.config.session_id = Some(uuid);
            self.managed_session_id = Some(uuid);
        }
        self
    }

    pub fn session_id_uuid(mut self, session_id: uuid::Uuid) -> Self {
        self.config.session_id = Some(session_id);
        self.managed_session_id = Some(session_id);
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.config.model = Some(model.into());
        self
    }

    pub async fn build(self) -> Result<ClaudeCode> {
        let existing_session_id = self.managed_session_id.or(self.config.session_id);

        match existing_session_id {
            None => {
                let session_id = uuid::Uuid::new_v4();
                let (stdin_tx, stdin_rx) = async_channel::bounded::<String>(32);
                let (stdout_tx, stdout_rx) = async_channel::bounded(32);

                let process_builder = ProcessBuilderInner::new(stdin_rx, stdout_tx)
                    .session_id(session_id.to_string());
                let process_builder = self.config.apply_to(process_builder);

                let process = match process_builder.new_session().await {
                    Ok(process) => process,
                    Err((_channels, e)) => {
                        return Err(anyhow::anyhow!("Failed to create new session: {e:?}"));
                    },
                };

                Ok(ClaudeCode::from_process(
                    process,
                    stdin_tx,
                    stdout_rx,
                    self.config,
                    session_id,
                ))
            },
            Some(session_id) => {
                let (stdin_tx, stdin_rx) = async_channel::bounded::<String>(32);
                let (stdout_tx, stdout_rx) = async_channel::bounded(32);

                let process_builder = ProcessBuilderInner::new(stdin_rx, stdout_tx)
                    .session_id(session_id.to_string());
                let process_builder = self.config.apply_to(process_builder);

                let process = match process_builder.resume_session().await {
                    Ok(process) => process,
                    Err((_channels, e)) => {
                        return Err(anyhow::anyhow!("Failed to resume session: {e:?}"));
                    },
                };

                Ok(ClaudeCode::from_process(
                    process,
                    stdin_tx,
                    stdout_rx,
                    self.config,
                    session_id,
                ))
            },
        }
    }

    pub async fn create_new(self) -> Result<ClaudeCode> {
        let session_id = self
            .managed_session_id
            .or(self.config.session_id)
            .unwrap_or_else(uuid::Uuid::new_v4);

        let (stdin_tx, stdin_rx) = async_channel::bounded::<String>(32);
        let (stdout_tx, stdout_rx) = async_channel::bounded(32);

        let process_builder =
            ProcessBuilderInner::new(stdin_rx, stdout_tx).session_id(session_id.to_string());
        let process_builder = self.config.apply_to(process_builder);

        let process = match process_builder.new_session().await {
            Ok(process) => process,
            Err((_channels, e)) => {
                return Err(anyhow::anyhow!("Failed to create new session: {e:?}"));
            },
        };

        Ok(ClaudeCode::from_process(
            process,
            stdin_tx,
            stdout_rx,
            self.config,
            session_id,
        ))
    }

    pub async fn resume(self) -> Result<ClaudeCode> {
        let session_id = self
            .managed_session_id
            .or(self.config.session_id)
            .ok_or_else(|| anyhow::anyhow!("Session ID required for resume"))?;

        let (stdin_tx, stdin_rx) = async_channel::bounded::<String>(32);
        let (stdout_tx, stdout_rx) = async_channel::bounded(32);

        let process_builder =
            ProcessBuilderInner::new(stdin_rx, stdout_tx).session_id(session_id.to_string());
        let process_builder = self.config.apply_to(process_builder);

        let process = match process_builder.resume_session().await {
            Ok(process) => process,
            Err((_channels, e)) => {
                return Err(anyhow::anyhow!("Failed to resume session: {e:?}"));
            },
        };

        Ok(ClaudeCode::from_process(
            process,
            stdin_tx,
            stdout_rx,
            self.config,
            session_id,
        ))
    }
}
