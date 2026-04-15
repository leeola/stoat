pub mod builder;
mod control;
pub mod process;

pub use self::builder::ClaudeCodeBuilder;
use self::process::Process;
use crate::messages::{PermissionMode, SdkMessage, SettingSource};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use stoat::host::AgentMessage;
use tokio::sync::{Mutex as TokioMutex, mpsc};
use tracing::info;

/// Programmatic MCP server configuration, written to a temp file that
/// the CLI reads via `--mcp-config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerConfig {
    /// Subprocess server speaking MCP over stdio.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// HTTP-based MCP server.
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
    /// Server-Sent Events based MCP server.
    Sse {
        url: String,
        headers: HashMap<String, String>,
    },
}

#[derive(Debug, Clone)]
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

    // ---- Phase 6 additions ----
    /// Environment variables to merge into the child's env on spawn.
    pub env: HashMap<String, String>,
    /// Full system prompt override (matches `--system-prompt`).
    pub system_prompt: Option<String>,
    /// System-prompt file override (matches `--system-prompt-file`).
    pub system_prompt_file: Option<PathBuf>,
    /// Extended-thinking token budget. Emitted as `MAX_THINKING_TOKENS`
    /// env var (the CLI reads it that way).
    pub max_thinking_tokens: Option<u32>,
    /// Path to the CLI binary. `None` uses `claude` from `$PATH`.
    pub binary_path: Option<PathBuf>,
    /// Programmatic MCP server definitions. Serialised to a temp JSON
    /// file the CLI reads via `--mcp-config`; the tempfile lifetime is
    /// bound to the spawned `Process` so it drops on teardown.
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Value for `--settings` (file path or literal JSON).
    pub settings_arg: Option<String>,
    /// Agents JSON for `--agents`. Serialised verbatim.
    pub agents: Option<serde_json::Value>,
    /// Explicit tools list for `--tools`. Distinct from
    /// `allowed_tools`: `tools` replaces the default preset entirely.
    pub tools: Option<Vec<String>>,
    /// Plugin directories for `--plugin-dir` (repeated per entry).
    pub plugin_dirs: Vec<PathBuf>,
    /// If true, emits `--strict-mcp-config`.
    pub strict_mcp_config: bool,
    /// Value for `--fallback-model`.
    pub fallback_model: Option<String>,
    /// When false, emits `--no-session-persistence`. Default true.
    pub session_persistence: bool,
    /// Value for `--effort` (low / medium / high / max).
    pub effort: Option<String>,
    /// Emits `--allow-dangerously-skip-permissions` when the CLI
    /// gates bypass mode behind this long-form flag.
    pub allow_dangerously_skip_permissions: bool,
}

impl Default for SessionConfig {
    /// Defaults tuned for full CC SDK feature surface:
    /// - partial-message streaming on (`include_partial_messages`);
    /// - all three setting sources merged (user / project / local);
    /// - `AskUserQuestion` tool disallowed (hosts render their own permission UI);
    /// - `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1` so the CLI emits `session_state_changed`,
    ///   `compact_boundary`, and friends;
    /// - `replay-user-messages` passed through so the host can dedupe echoed user frames by
    ///   `message_uuid`.
    ///
    /// Callers that want the minimal / "old" defaults can construct
    /// `SessionConfig` field-by-field and zero these knobs explicitly.
    fn default() -> Self {
        let mut env = HashMap::new();
        env.insert(
            "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS".to_string(),
            "1".to_string(),
        );
        Self {
            max_turns: None,
            cwd: None,
            allowed_tools: None,
            disallowed_tools: Some(vec!["AskUserQuestion".to_string()]),
            permission_mode: None,
            session_id: None,
            model: None,
            append_system_prompt: None,
            append_system_prompt_file: None,
            add_dirs: Vec::new(),
            mcp_config: None,
            setting_sources: vec![
                SettingSource::User,
                SettingSource::Project,
                SettingSource::Local,
            ],
            include_partial_messages: true,
            include_hook_events: false,
            bare: false,
            fork_session: false,
            dangerously_skip_permissions: false,
            init_timeout: None,
            extra_args: vec![("replay-user-messages".to_string(), None)],
            env,
            system_prompt: None,
            system_prompt_file: None,
            max_thinking_tokens: None,
            binary_path: None,
            mcp_servers: HashMap::new(),
            settings_arg: None,
            agents: None,
            tools: None,
            plugin_dirs: Vec::new(),
            strict_mcp_config: false,
            fallback_model: None,
            session_persistence: true,
            effort: None,
            allow_dangerously_skip_permissions: false,
        }
    }
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
    /// `ClaudeCodeSession::recv` adapter before pulling the next wire
    /// message. `Arc` so the default hook wrapper (registered via the
    /// control dispatcher) can synthesise additional messages without
    /// passing through the SDK stream.
    pub(crate) pending: Arc<StdMutex<VecDeque<AgentMessage>>>,

    /// Adapter-level session state threaded through `recv()` so the
    /// classifier can correlate tool_result with prior tool_use and
    /// usage totals accumulate across turns.
    pub(crate) adapter_state: StdMutex<crate::host_adapter::AdapterState>,

    /// Correlator for outgoing `control_request` frames. Shared with
    /// the dispatcher (when installed) so inbound `control_response`
    /// frames can complete the oneshot registered here when the
    /// corresponding request was sent.
    pub(crate) control_waiters: crate::claude_code::control::ControlWaiters,

    /// Prompt-state: tracks UUIDs we stamped on outbound user messages
    /// so the adapter can drop the CLI's echoes when
    /// `replay-user-messages` is enabled.
    pub(crate) prompt_state: Arc<StdMutex<PromptState>>,

    managed_session_id: uuid::Uuid,
}

/// Concurrent-prompt bookkeeping. Lives on [`ClaudeCode`] and is also
/// handed to the adapter so [`host_adapter::extract_tool_results`] can
/// drop user-message echoes stamped by our own outbound frames.
#[derive(Debug, Default)]
pub(crate) struct PromptState {
    /// UUIDs stamped on our outbound user messages. When
    /// `replay-user-messages` is active the CLI echoes these back;
    /// matches drop silently.
    pub own_uuids: std::collections::HashSet<uuid::Uuid>,
    /// Whether a prompt is currently being processed. Future work:
    /// queue subsequent prompts and hand off when the current turn
    /// completes.
    pub running: bool,
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

    /// Create ClaudeCode from a Process instance with communication
    /// channels. `pending` is shared with the dispatcher's default
    /// hook wrapper (if installed) so tool-response hooks can inject
    /// synthesised `AgentMessage`s into the host's recv stream.
    /// `control_waiters` is also shared with the dispatcher so
    /// outgoing control_request frames can await their response.
    /// `prompt_state` is shared with the adapter so it can drop CLI
    /// echoes of our own user messages.
    pub(crate) fn from_process(
        process: Process,
        process_stdin_tx: mpsc::Sender<String>,
        process_stdout_rx: mpsc::Receiver<SdkMessage>,
        session_id: uuid::Uuid,
        pending: Arc<StdMutex<VecDeque<AgentMessage>>>,
        control_waiters: crate::claude_code::control::ControlWaiters,
        prompt_state: Arc<StdMutex<PromptState>>,
    ) -> Self {
        info!("ClaudeCode instance created for session: {}", session_id);

        let mut adapter_state = crate::host_adapter::AdapterState::default();
        adapter_state.prompt_state = Some(prompt_state.clone());

        Self {
            process: StdMutex::new(Some(process)),
            process_stdin_tx,
            process_stdout_rx: TokioMutex::new(process_stdout_rx),
            pending,
            adapter_state: StdMutex::new(adapter_state),
            control_waiters,
            prompt_state,
            managed_session_id: session_id,
        }
    }

    pub async fn send_message(&self, content: &str) -> Result<()> {
        // Stamp a unique UUID on the outbound user frame so the
        // adapter can identify and drop the CLI's echo when
        // `replay-user-messages` is enabled.
        let message_uuid = uuid::Uuid::new_v4();
        {
            let mut state = self
                .prompt_state
                .lock()
                .expect("prompt_state mutex poisoned");
            state.own_uuids.insert(message_uuid);
        }

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
            },
            "message_uuid": message_uuid.to_string(),
            "parent_tool_use_id": null,
        });

        let message = serde_json::to_string(&user_msg)?;
        self.process_stdin_tx
            .send(message)
            .await
            .context("Failed to send message to Claude Code")?;
        Ok(())
    }

    /// Shut the subprocess down. Inherent counterpart of
    /// [`ClaudeCodeSession::shutdown`] so the trait impl can delegate here
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

    /// Current token accumulation across all completed turns. Updated
    /// by the adapter as `Assistant` and `Result` frames arrive.
    pub fn accumulated_usage(&self) -> stoat::host::TokenUsage {
        self.adapter_state
            .lock()
            .expect("ClaudeCode adapter_state mutex poisoned")
            .accumulated_usage
            .clone()
    }

    /// Stable fingerprint of the session's (cwd, mcp_servers) config:
    /// sha256 of canonical JSON `{ cwd, mcp_servers: [sorted by name] }`.
    /// Used by hosts to detect whether a new session would differ
    /// meaningfully from a resumed one.
    pub fn session_fingerprint(cwd: &str, mcp_servers: &[&str]) -> String {
        use sha2::{Digest, Sha256};
        let mut sorted = mcp_servers.to_vec();
        sorted.sort();
        let canonical = serde_json::json!({
            "cwd": cwd,
            "mcp_servers": sorted,
        })
        .to_string();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Request the CLI switch to a different model mid-session via the
    /// control protocol. Awaits the CLI's ack.
    pub async fn set_model(&self, model_id: &str) -> Result<()> {
        self.send_control_request_awaited(serde_json::json!({
            "subtype": "set_model",
            "model": model_id,
        }))
        .await
        .map(|_| ())
    }

    /// Request the CLI switch to a different permission mode
    /// mid-session via the control protocol. Awaits the CLI's ack.
    pub async fn set_permission_mode(&self, mode: &str) -> Result<()> {
        self.send_control_request_awaited(serde_json::json!({
            "subtype": "set_permission_mode",
            "mode": mode,
        }))
        .await
        .map(|_| ())
    }

    /// Interrupt the current turn. Awaits the CLI's control_response
    /// ack; returns `Ok(())` on success and an error carrying the
    /// CLI's error message on failure.
    pub async fn interrupt(&self) -> Result<()> {
        self.send_control_request_awaited(serde_json::json!({ "subtype": "interrupt" }))
            .await
            .map(|_| ())
    }

    /// Fire-and-forget variant of [`send_control_request_awaited`] for
    /// callers that don't need the ack. The request is written to
    /// stdin but no correlator entry is registered.
    pub async fn send_control_request(&self, request: serde_json::Value) -> Result<()> {
        let request_id = format!("stoat_{}", uuid::Uuid::new_v4());
        self.write_control_request(request_id, request).await
    }

    /// Send a `control_request` and await the matching
    /// `control_response`. Returns the CLI's success-body JSON or an
    /// error carrying the CLI's error message.
    pub async fn send_control_request_awaited(
        &self,
        request: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request_id = format!("stoat_{}", uuid::Uuid::new_v4());
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut waiters = self
                .control_waiters
                .lock()
                .expect("control_waiters mutex poisoned");
            waiters.insert(request_id.clone(), tx);
        }
        self.write_control_request(request_id.clone(), request)
            .await?;
        match rx.await {
            Ok(control::ControlAck::Success(body)) => Ok(body),
            Ok(control::ControlAck::Error(msg)) => {
                Err(anyhow::anyhow!("control_request failed: {msg}"))
            },
            Err(_) => {
                // Waiter was dropped without a response; clean up and
                // bail.
                let mut waiters = self
                    .control_waiters
                    .lock()
                    .expect("control_waiters mutex poisoned");
                waiters.remove(&request_id);
                Err(anyhow::anyhow!(
                    "control_response channel closed before acknowledgement"
                ))
            },
        }
    }

    async fn write_control_request(
        &self,
        request_id: String,
        request: serde_json::Value,
    ) -> Result<()> {
        let frame = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        let line = serde_json::to_string(&frame)?;
        self.process_stdin_tx
            .send(line)
            .await
            .context("Failed to send control request")?;
        Ok(())
    }

    /// Check if the Claude process is still alive. Inherent counterpart
    /// of [`ClaudeCodeSession::is_alive`].
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
