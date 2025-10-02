use crate::{
    claude_code::{ClaudeCode, SessionConfig, process::ProcessBuilder as ProcessBuilderInner},
    messages::PermissionMode,
};
use anyhow::Result;
use tokio::sync::mpsc;

#[derive(Debug, Default)]
pub struct ClaudeCodeBuilder {
    config: SessionConfig,
    managed_session_id: Option<uuid::Uuid>,
}

impl ClaudeCodeBuilder {
    pub fn new() -> Self {
        Self::default()
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
        // Parse string into UUID if possible
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

    /// Build with automatic logic - create new if no session_id, otherwise resume
    pub async fn build(self) -> Result<ClaudeCode> {
        // Check if we have an existing session ID to work with
        let existing_session_id = self.managed_session_id.or(self.config.session_id);

        match existing_session_id {
            None => {
                // No session ID provided - generate one and create new session
                let session_id = uuid::Uuid::new_v4();

                // Create channels
                let (stdin_tx, stdin_rx) = mpsc::channel::<String>(32);
                let (stdout_tx, stdout_rx) = mpsc::channel(32);

                // Build process with new session
                let mut process_builder = ProcessBuilderInner::new(stdin_rx, stdout_tx)
                    .session_id(session_id.to_string());

                // Apply configuration
                if let Some(model) = &self.config.model {
                    process_builder = process_builder.model(model.clone());
                }
                if let Some(max_turns) = self.config.max_turns {
                    process_builder = process_builder.max_turns(max_turns);
                }
                if let Some(cwd) = &self.config.cwd {
                    process_builder = process_builder.cwd(cwd.clone());
                }
                if let Some(tools) = &self.config.allowed_tools {
                    process_builder = process_builder.allowed_tools(tools.clone());
                }
                if let Some(mode) = &self.config.permission_mode {
                    process_builder = process_builder.permission_mode(mode.clone());
                }

                // Create new session
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
                // Session ID provided - resume existing session

                // Create channels
                let (stdin_tx, stdin_rx) = mpsc::channel::<String>(32);
                let (stdout_tx, stdout_rx) = mpsc::channel(32);

                // Build process to resume session
                let mut process_builder = ProcessBuilderInner::new(stdin_rx, stdout_tx)
                    .session_id(session_id.to_string());

                // Apply configuration
                if let Some(model) = &self.config.model {
                    process_builder = process_builder.model(model.clone());
                }
                if let Some(max_turns) = self.config.max_turns {
                    process_builder = process_builder.max_turns(max_turns);
                }
                if let Some(cwd) = &self.config.cwd {
                    process_builder = process_builder.cwd(cwd.clone());
                }
                if let Some(tools) = &self.config.allowed_tools {
                    process_builder = process_builder.allowed_tools(tools.clone());
                }
                if let Some(mode) = &self.config.permission_mode {
                    process_builder = process_builder.permission_mode(mode.clone());
                }

                // Resume existing session
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

    /// Only create a new session (never resume)
    pub async fn create_new(self) -> Result<ClaudeCode> {
        // Use managed_session_id if we have one, otherwise use config.session_id,
        // otherwise generate new
        let session_id = self
            .managed_session_id
            .or(self.config.session_id)
            .unwrap_or_else(uuid::Uuid::new_v4);

        // Create a single set of channels
        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(32);
        let (stdout_tx, stdout_rx) = mpsc::channel(32);

        let mut process_builder =
            ProcessBuilderInner::new(stdin_rx, stdout_tx).session_id(session_id.to_string());

        // Apply configuration
        if let Some(model) = &self.config.model {
            process_builder = process_builder.model(model.clone());
        }
        if let Some(max_turns) = self.config.max_turns {
            process_builder = process_builder.max_turns(max_turns);
        }
        if let Some(cwd) = &self.config.cwd {
            process_builder = process_builder.cwd(cwd.clone());
        }
        if let Some(tools) = &self.config.allowed_tools {
            process_builder = process_builder.allowed_tools(tools.clone());
        }
        if let Some(mode) = &self.config.permission_mode {
            process_builder = process_builder.permission_mode(mode.clone());
        }

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

    /// Only resume an existing session (never create)
    pub async fn resume(self) -> Result<ClaudeCode> {
        // For resume, we must have a session ID
        let session_id = self
            .managed_session_id
            .or(self.config.session_id)
            .ok_or_else(|| anyhow::anyhow!("Session ID required for resume"))?;

        // Create a single set of channels
        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(32);
        let (stdout_tx, stdout_rx) = mpsc::channel(32);

        let mut process_builder =
            ProcessBuilderInner::new(stdin_rx, stdout_tx).session_id(session_id.to_string());

        // Apply configuration
        if let Some(model) = &self.config.model {
            process_builder = process_builder.model(model.clone());
        }
        if let Some(max_turns) = self.config.max_turns {
            process_builder = process_builder.max_turns(max_turns);
        }
        if let Some(cwd) = &self.config.cwd {
            process_builder = process_builder.cwd(cwd.clone());
        }
        if let Some(tools) = &self.config.allowed_tools {
            process_builder = process_builder.allowed_tools(tools.clone());
        }
        if let Some(mode) = &self.config.permission_mode {
            process_builder = process_builder.permission_mode(mode.clone());
        }

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
