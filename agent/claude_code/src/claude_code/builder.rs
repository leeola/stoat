use crate::{
    claude_code::{
        ClaudeCode, SessionConfig,
        control::{DispatcherDeps, run_dispatcher},
        process::ProcessBuilder as ProcessBuilderInner,
    },
    messages::{PermissionMode, SettingSource},
};
use anyhow::Result;
use std::{path::PathBuf, sync::Arc};
use stoat::host::PermissionCallback;
use stoat_log::TextProtoLog;
use tokio::sync::mpsc;

#[derive(Default)]
pub struct ClaudeCodeBuilder {
    config: SessionConfig,
    managed_session_id: Option<uuid::Uuid>,
    tx_log: Option<Arc<TextProtoLog>>,
    rx_log: Option<Arc<TextProtoLog>>,
    permission_callback: Option<Arc<dyn PermissionCallback>>,
}

impl std::fmt::Debug for ClaudeCodeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeBuilder")
            .field("config", &self.config)
            .field("managed_session_id", &self.managed_session_id)
            .field("has_tx_log", &self.tx_log.is_some())
            .field("has_rx_log", &self.rx_log.is_some())
            .field(
                "has_permission_callback",
                &self.permission_callback.is_some(),
            )
            .finish()
    }
}

impl ClaudeCodeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the builder's configuration wholesale. Useful for
    /// constructing a builder from a prebuilt [`SessionConfig`] without
    /// having to forward every field individually.
    pub fn with_config(mut self, config: SessionConfig) -> Self {
        self.managed_session_id = config.session_id;
        self.config = config;
        self
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

    pub fn include_partial_messages(mut self, enabled: bool) -> Self {
        self.config.include_partial_messages = enabled;
        self
    }

    pub fn include_hook_events(mut self, enabled: bool) -> Self {
        self.config.include_hook_events = enabled;
        self
    }

    pub fn dangerously_skip_permissions(mut self, enabled: bool) -> Self {
        self.config.dangerously_skip_permissions = enabled;
        self
    }

    pub fn init_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.config.init_timeout = Some(timeout);
        self
    }

    pub fn disallowed_tools(mut self, tools: Vec<String>) -> Self {
        self.config.disallowed_tools = Some(tools);
        self
    }

    pub fn append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.config.append_system_prompt = Some(prompt.into());
        self
    }

    pub fn append_system_prompt_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.append_system_prompt_file = Some(path.into());
        self
    }

    pub fn add_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.config.add_dirs = dirs;
        self
    }

    pub fn mcp_config(mut self, config: impl Into<String>) -> Self {
        self.config.mcp_config = Some(config.into());
        self
    }

    pub fn setting_sources(mut self, sources: Vec<SettingSource>) -> Self {
        self.config.setting_sources = sources;
        self
    }

    pub fn bare(mut self, enabled: bool) -> Self {
        self.config.bare = enabled;
        self
    }

    pub fn fork_session(mut self, enabled: bool) -> Self {
        self.config.fork_session = enabled;
        self
    }

    pub fn extra_args(mut self, args: Vec<(String, Option<String>)>) -> Self {
        self.config.extra_args = args;
        self
    }

    /// Attaches byte-faithful transcript logs to the subprocess channels.
    /// `tx_log` receives every payload sent to Claude, `rx_log` every payload
    /// received from Claude, each as one JSON object per line.
    pub fn with_text_proto_logs(
        mut self,
        tx_log: Arc<TextProtoLog>,
        rx_log: Arc<TextProtoLog>,
    ) -> Self {
        self.tx_log = Some(tx_log);
        self.rx_log = Some(rx_log);
        self
    }

    /// Register a host-side callback that the CLI consults for every
    /// tool invocation not pre-approved by `--allowedTools`. Setting
    /// this causes the wrapper to pass
    /// `--permission-prompt-tool-name stdio` to the CLI so permission
    /// prompts are routed over the control protocol, and interposes a
    /// dispatcher task that translates `control_request` messages into
    /// callback invocations and writes `control_response` replies back
    /// through the stdin pipe.
    pub fn permission_callback(mut self, callback: Arc<dyn PermissionCallback>) -> Self {
        self.permission_callback = Some(callback);
        self
    }

    /// Build with automatic logic - create new if no session_id, otherwise resume
    pub async fn build(self) -> Result<ClaudeCode> {
        let existing_session_id = self.managed_session_id.or(self.config.session_id);
        match existing_session_id {
            None => self.spawn(uuid::Uuid::new_v4(), SpawnMode::New).await,
            Some(session_id) => self.spawn(session_id, SpawnMode::Resume).await,
        }
    }

    /// Only create a new session (never resume)
    pub async fn create_new(self) -> Result<ClaudeCode> {
        let session_id = self
            .managed_session_id
            .or(self.config.session_id)
            .unwrap_or_else(uuid::Uuid::new_v4);
        self.spawn(session_id, SpawnMode::New).await
    }

    /// Only resume an existing session (never create)
    pub async fn resume(self) -> Result<ClaudeCode> {
        let session_id = self
            .managed_session_id
            .or(self.config.session_id)
            .ok_or_else(|| anyhow::anyhow!("Session ID required for resume"))?;
        self.spawn(session_id, SpawnMode::Resume).await
    }

    async fn spawn(self, session_id: uuid::Uuid, mode: SpawnMode) -> Result<ClaudeCode> {
        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(32);
        // `inner_tx`/`inner_rx` is the direct pipe from the stdout
        // handler. When a permission callback is registered, we drain
        // it in the dispatcher and forward host-visible messages via
        // `outer_tx`/`outer_rx`. Otherwise we skip the dispatcher and
        // hand the receiver directly to ClaudeCode.
        let (inner_tx, inner_rx) = mpsc::channel::<crate::messages::SdkMessage>(32);

        let process_builder = self.configure_process_builder(
            ProcessBuilderInner::new(stdin_rx, inner_tx).session_id(session_id.to_string()),
        );

        let process = match mode {
            SpawnMode::New => process_builder
                .new_session()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to create new session: {:?}", e))?,
            SpawnMode::Resume => process_builder
                .resume_session()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to resume session: {:?}", e))?,
        };

        // Interpose the control dispatcher only when a permission
        // callback is registered. When no callback is set, the CLI is
        // also told not to use the control protocol
        // (`--permission-prompt-tool-name` is not passed), so no
        // control requests should ever arrive.
        let stdout_rx = if self.permission_callback.is_some() {
            let (outer_tx, outer_rx) = mpsc::channel(32);
            let deps = DispatcherDeps {
                permission_callback: self.permission_callback.clone(),
                stdin_tx: stdin_tx.clone(),
            };
            tokio::spawn(run_dispatcher(inner_rx, outer_tx, deps));
            outer_rx
        } else {
            inner_rx
        };

        Ok(ClaudeCode::from_process(
            process, stdin_tx, stdout_rx, session_id,
        ))
    }

    fn configure_process_builder(&self, mut pb: ProcessBuilderInner) -> ProcessBuilderInner {
        if let Some(model) = &self.config.model {
            pb = pb.model(model.clone());
        }
        if let Some(max_turns) = self.config.max_turns {
            pb = pb.max_turns(max_turns);
        }
        if let Some(cwd) = &self.config.cwd {
            pb = pb.cwd(cwd.clone());
        }
        if let Some(tools) = &self.config.allowed_tools {
            pb = pb.allowed_tools(tools.clone());
        }
        if let Some(tools) = &self.config.disallowed_tools {
            pb = pb.disallowed_tools(tools.clone());
        }
        if let Some(mode) = &self.config.permission_mode {
            pb = pb.permission_mode(mode.clone());
        }
        if let Some(prompt) = &self.config.append_system_prompt {
            pb = pb.append_system_prompt(prompt.clone());
        }
        if let Some(path) = &self.config.append_system_prompt_file {
            pb = pb.append_system_prompt_file(path.clone());
        }
        if !self.config.add_dirs.is_empty() {
            pb = pb.add_dirs(self.config.add_dirs.clone());
        }
        if let Some(config) = &self.config.mcp_config {
            pb = pb.mcp_config(config.clone());
        }
        if !self.config.setting_sources.is_empty() {
            pb = pb.setting_sources(self.config.setting_sources.clone());
        }
        if self.config.include_partial_messages {
            pb = pb.include_partial_messages(true);
        }
        if self.config.include_hook_events {
            pb = pb.include_hook_events(true);
        }
        if self.config.bare {
            pb = pb.bare(true);
        }
        if self.config.fork_session {
            pb = pb.fork_session(true);
        }
        if self.config.dangerously_skip_permissions {
            pb = pb.dangerously_skip_permissions(true);
        }
        if let Some(timeout) = self.config.init_timeout {
            pb = pb.init_timeout(timeout);
        }
        if !self.config.extra_args.is_empty() {
            pb = pb.extra_args(self.config.extra_args.clone());
        }
        if self.permission_callback.is_some() {
            pb = pb.permission_prompt_tool_name("stdio");
        }
        if let (Some(tx), Some(rx)) = (&self.tx_log, &self.rx_log) {
            pb = pb.text_proto_logs(tx.clone(), rx.clone());
        }
        pb
    }
}

enum SpawnMode {
    New,
    Resume,
}
