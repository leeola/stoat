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
use stoat::host::{HookCallback, PermissionCallback};
use stoat_log::TextProtoLog;
use tokio::sync::mpsc;

#[derive(Default)]
pub struct ClaudeCodeBuilder {
    config: SessionConfig,
    managed_session_id: Option<uuid::Uuid>,
    tx_log: Option<Arc<TextProtoLog>>,
    rx_log: Option<Arc<TextProtoLog>>,
    permission_callback: Option<Arc<dyn PermissionCallback>>,
    hook_callback: Option<Arc<dyn HookCallback>>,
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
            .field("has_hook_callback", &self.hook_callback.is_some())
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

    // ---- Phase 6 additions ----

    /// Inject an environment variable into the spawned process.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.env.insert(key.into(), value.into());
        self
    }

    /// Merge an entire env map into the spawned process.
    pub fn envs<K, V>(mut self, iter: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.config
            .env
            .extend(iter.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Full system prompt (replaces the default; emits `--system-prompt`).
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.config.system_prompt = Some(prompt.into());
        self
    }

    /// System prompt read from a file (emits `--system-prompt-file`).
    pub fn system_prompt_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.system_prompt_file = Some(path.into());
        self
    }

    /// Extended-thinking token budget. Emitted as `MAX_THINKING_TOKENS`
    /// in the child process env.
    pub fn max_thinking_tokens(mut self, tokens: u32) -> Self {
        self.config.max_thinking_tokens = Some(tokens);
        self
    }

    /// Override the CLI binary path.
    pub fn binary_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.binary_path = Some(path.into());
        self
    }

    /// Programmatic MCP server configuration. Overwrites any prior call.
    pub fn mcp_servers(
        mut self,
        servers: std::collections::HashMap<String, crate::claude_code::McpServerConfig>,
    ) -> Self {
        self.config.mcp_servers = servers;
        self
    }

    /// Add a single MCP server by name (merges with existing config).
    pub fn add_mcp_server(
        mut self,
        name: impl Into<String>,
        config: crate::claude_code::McpServerConfig,
    ) -> Self {
        self.config.mcp_servers.insert(name.into(), config);
        self
    }

    /// `--settings` argument (file path or literal JSON).
    pub fn settings_arg(mut self, value: impl Into<String>) -> Self {
        self.config.settings_arg = Some(value.into());
        self
    }

    /// `--agents` argument (inline JSON).
    pub fn agents(mut self, value: serde_json::Value) -> Self {
        self.config.agents = Some(value);
        self
    }

    /// Explicit `--tools` list (replaces the default preset).
    pub fn tools(mut self, tools: Vec<String>) -> Self {
        self.config.tools = Some(tools);
        self
    }

    /// Add a plugin directory (repeatable). Emits one `--plugin-dir`
    /// per entry.
    pub fn plugin_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.config.plugin_dirs = dirs;
        self
    }

    /// Emit `--strict-mcp-config`.
    pub fn strict_mcp_config(mut self, enabled: bool) -> Self {
        self.config.strict_mcp_config = enabled;
        self
    }

    /// Fallback model for `--fallback-model`.
    pub fn fallback_model(mut self, model: impl Into<String>) -> Self {
        self.config.fallback_model = Some(model.into());
        self
    }

    /// `false` emits `--no-session-persistence`. Default `true`.
    pub fn session_persistence(mut self, enabled: bool) -> Self {
        self.config.session_persistence = enabled;
        self
    }

    /// `--effort` value (low / medium / high / max).
    pub fn effort(mut self, value: impl Into<String>) -> Self {
        self.config.effort = Some(value.into());
        self
    }

    /// Emit `--allow-dangerously-skip-permissions` (gate that some CLI
    /// versions require alongside `--dangerously-skip-permissions`).
    pub fn allow_dangerously_skip_permissions(mut self, enabled: bool) -> Self {
        self.config.allow_dangerously_skip_permissions = enabled;
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

    /// Register a callback that handles CLI hook events. Pairs with
    /// `include_hook_events(true)` to enable the CLI's hook firing on
    /// stdout. If no callback is registered, hook control requests are
    /// answered with a no-op success body so the CLI doesn't stall.
    pub fn hook_callback(mut self, callback: Arc<dyn HookCallback>) -> Self {
        self.hook_callback = Some(callback);
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

    /// Fork an existing session into a new one. Sets `fork_session` on
    /// the CLI invocation, reuses the parent's session id as the
    /// `--resume` target, and allocates a fresh UUID for the new
    /// session. Returns the newly spawned [`ClaudeCode`].
    pub async fn fork_from(mut self, parent_session_id: uuid::Uuid) -> Result<ClaudeCode> {
        self.config.fork_session = true;
        self.config.session_id = Some(parent_session_id);
        self.managed_session_id = Some(parent_session_id);
        // The CLI itself handles the "allocate new id" semantics when
        // `--fork-session` is set and `--resume` points at the parent.
        self.spawn(parent_session_id, SpawnMode::Resume).await
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

        // Interpose the control dispatcher whenever control-protocol
        // traffic might arrive: a permission callback is registered, a
        // hook callback is registered, hook events are enabled, or any
        // other CLI-initiated control flow is active. Without the
        // dispatcher, control requests would hit the host as raw
        // `SdkMessage::ControlRequest` frames that most consumers would
        // mishandle.
        let needs_dispatcher = self.permission_callback.is_some()
            || self.hook_callback.is_some()
            || self.config.include_hook_events;
        // Create the shared state up-front so the dispatcher and the
        // ClaudeCode session can both hold handles.
        let pending = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let control_waiters: crate::claude_code::control::ControlWaiters =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let prompt_state = Arc::new(std::sync::Mutex::new(
            crate::claude_code::PromptState::default(),
        ));

        // The dispatcher must be installed when we need to route any
        // control traffic, INCLUDING outgoing control_request frames
        // that expect a response (interrupt, set_model, ...). Without
        // a dispatcher, inbound control_response frames would reach
        // host_adapter as SdkMessage::ControlResponse which it would
        // drop. We always install the dispatcher now.
        let _ = needs_dispatcher; // retain local for readability
        let (outer_tx, outer_rx) = mpsc::channel(32);
        let hook_callback: Option<Arc<dyn HookCallback>> =
            Some(Arc::new(crate::claude_code::control::DefaultHookCallback {
                pending: pending.clone(),
                inner: self.hook_callback.clone(),
            }));
        let deps = DispatcherDeps {
            permission_callback: self.permission_callback.clone(),
            hook_callback,
            stdin_tx: stdin_tx.clone(),
            control_waiters: control_waiters.clone(),
        };
        tokio::spawn(run_dispatcher(inner_rx, outer_tx, deps));
        let stdout_rx = outer_rx;

        Ok(ClaudeCode::from_process(
            process,
            stdin_tx,
            stdout_rx,
            session_id,
            pending,
            control_waiters,
            prompt_state,
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

        // ---- Phase 6 fields ----
        if !self.config.env.is_empty() {
            pb = pb.env_map(self.config.env.clone());
        }
        if let Some(prompt) = &self.config.system_prompt {
            pb = pb.system_prompt(prompt.clone());
        }
        if let Some(path) = &self.config.system_prompt_file {
            pb = pb.system_prompt_file(path.clone());
        }
        if let Some(tokens) = self.config.max_thinking_tokens {
            pb = pb.max_thinking_tokens(tokens);
        }
        if !self.config.mcp_servers.is_empty() {
            match write_mcp_config_tempfile(&self.config.mcp_servers) {
                Ok(file) => pb = pb.mcp_config_tempfile(Arc::new(file)),
                Err(err) => tracing::warn!(
                    "failed to write MCP config tempfile, skipping mcp_servers: {err}"
                ),
            }
        }
        if let Some(value) = &self.config.settings_arg {
            pb = pb.settings_arg(value.clone());
        }
        if let Some(agents) = &self.config.agents {
            pb = pb.agents(agents.clone());
        }
        if let Some(tools) = &self.config.tools {
            pb = pb.tools_override(tools.clone());
        }
        if !self.config.plugin_dirs.is_empty() {
            pb = pb.plugin_dirs(self.config.plugin_dirs.clone());
        }
        if self.config.strict_mcp_config {
            pb = pb.strict_mcp_config(true);
        }
        if let Some(model) = &self.config.fallback_model {
            pb = pb.fallback_model(model.clone());
        }
        if !self.config.session_persistence {
            pb = pb.session_persistence(false);
        }
        if let Some(effort) = &self.config.effort {
            pb = pb.effort(effort.clone());
        }
        if self.config.allow_dangerously_skip_permissions {
            pb = pb.allow_dangerously_skip_permissions(true);
        }
        #[cfg(not(test))]
        if let Some(path) = &self.config.binary_path {
            pb = pb.binary_path(path.to_string_lossy().into_owned());
        }

        pb
    }
}

/// Serialize the programmatic MCP server config to a temp file in the
/// shape the Claude CLI consumes via `--mcp-config`:
/// `{ "mcpServers": { <name>: <config> } }`. The returned
/// `NamedTempFile` must outlive the subprocess.
fn write_mcp_config_tempfile(
    servers: &std::collections::HashMap<String, crate::claude_code::McpServerConfig>,
) -> std::io::Result<tempfile::NamedTempFile> {
    use crate::claude_code::McpServerConfig;
    use std::io::Write;

    let mut map = serde_json::Map::new();
    for (name, cfg) in servers {
        let obj = match cfg {
            McpServerConfig::Stdio { command, args, env } => {
                let mut m = serde_json::Map::new();
                m.insert("type".into(), "stdio".into());
                m.insert("command".into(), command.clone().into());
                m.insert(
                    "args".into(),
                    serde_json::Value::Array(
                        args.iter()
                            .cloned()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
                if !env.is_empty() {
                    m.insert(
                        "env".into(),
                        serde_json::Value::Object(
                            env.iter()
                                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                                .collect(),
                        ),
                    );
                }
                serde_json::Value::Object(m)
            },
            McpServerConfig::Http { url, headers } | McpServerConfig::Sse { url, headers } => {
                let kind = if matches!(cfg, McpServerConfig::Http { .. }) {
                    "http"
                } else {
                    "sse"
                };
                let mut m = serde_json::Map::new();
                m.insert("type".into(), kind.into());
                m.insert("url".into(), url.clone().into());
                if !headers.is_empty() {
                    m.insert(
                        "headers".into(),
                        serde_json::Value::Object(
                            headers
                                .iter()
                                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                                .collect(),
                        ),
                    );
                }
                serde_json::Value::Object(m)
            },
        };
        map.insert(name.clone(), obj);
    }

    let wrapper = serde_json::json!({ "mcpServers": serde_json::Value::Object(map) });
    let mut file = tempfile::NamedTempFile::new()?;
    writeln!(file, "{wrapper}")?;
    file.flush()?;
    Ok(file)
}

enum SpawnMode {
    New,
    Resume,
}
