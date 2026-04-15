use crate::messages::{PermissionMode, SdkMessage, SettingSource, SystemSubtype};
use snafu::{ResultExt, Snafu};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use stoat_log::TextProtoLog;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{debug, error, trace};

/// Bundle of dependencies handed to [`ProcessBuilder::setup_handlers`].
/// Grouped into a struct so the function does not exceed Clippy's
/// `too_many_arguments` threshold and to keep call sites readable.
struct HandlerDeps {
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    stdin_rx: mpsc::Receiver<String>,
    stdout_tx: mpsc::Sender<SdkMessage>,
    tx_log: Option<Arc<TextProtoLog>>,
    rx_log: Option<Arc<TextProtoLog>>,
    init_tx: Option<oneshot::Sender<()>>,
    shutdown_rx: oneshot::Receiver<()>,
}

/// Errors produced when starting or resuming a Claude subprocess session.
///
/// Returned by [`ProcessBuilder::new_session`], [`ProcessBuilder::resume_session`],
/// and [`ProcessBuilder::build`]. Variants cover both creation and resumption
/// because the underlying failure modes (spawn, stdio, channel recovery) are
/// symmetric; only [`SessionAlreadyExists`] and [`SessionNotFound`] are
/// specific to one direction.
#[derive(Debug, Snafu)]
pub enum SessionError {
    #[snafu(display("Session ID {session_id} is already in use"))]
    SessionAlreadyExists { session_id: String },

    #[snafu(display("No conversation found with session ID: {session_id}"))]
    SessionNotFound { session_id: String },

    #[snafu(display("Failed to recover channels from failed process"))]
    ChannelRecoveryFailed { source: CloseError },

    #[snafu(display("Failed to spawn Claude process"))]
    SpawnFailed { source: std::io::Error },

    #[snafu(display("Failed to get process stdio"))]
    StdioFailed,

    #[snafu(display("session_id is required but not provided"))]
    SessionIdMissing,
}

#[derive(Debug, Snafu)]
pub enum CloseError {
    #[snafu(display("Failed to kill process"))]
    KillFailed { source: std::io::Error },

    #[snafu(display("Handler task panicked: {}", task))]
    HandlerPanicked { task: String },
}

pub struct Process {
    child: Child,
    stdin_handle: JoinHandle<mpsc::Receiver<String>>,
    stdout_handle: JoinHandle<mpsc::Sender<SdkMessage>>,
    stderr_handle: JoinHandle<String>, // Returns last stderr line
    /// Oneshot sender that signals the stdin handler to stop its
    /// `select!` loop. Required because the handler reads from an mpsc
    /// channel whose senders live outside of [`Process`], so
    /// `recv().await` cannot be relied on to return `None` during
    /// shutdown.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Shared clones of the transcript logs so [`Process::close`] can
    /// flush them after the handler tasks have exited.
    tx_log: Option<Arc<TextProtoLog>>,
    rx_log: Option<Arc<TextProtoLog>>,
    /// Holds an MCP `--mcp-config` tempfile for the lifetime of the
    /// subprocess. The file must exist until the CLI has read it at
    /// startup; binding the `Arc<NamedTempFile>` here guarantees the
    /// file is not deleted prematurely by drop order.
    _mcp_config_tempfile: Option<Arc<tempfile::NamedTempFile>>,
}

pub struct RecoveredChannels {
    pub stdin_rx: mpsc::Receiver<String>,
    pub stdout_tx: mpsc::Sender<SdkMessage>,
    pub last_stderr: String,
}

pub struct ProcessBuilder {
    // Required channels (passed in constructor)
    stdin_rx: mpsc::Receiver<String>,
    stdout_tx: mpsc::Sender<SdkMessage>,

    // Optional configuration
    session_id: Option<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    cwd: Option<String>,
    allowed_tools: Option<Vec<String>>,
    disallowed_tools: Option<Vec<String>>,
    permission_mode: Option<PermissionMode>,
    append_system_prompt: Option<String>,
    append_system_prompt_file: Option<PathBuf>,
    add_dirs: Vec<PathBuf>,
    mcp_config: Option<String>,
    setting_sources: Vec<SettingSource>,
    include_partial_messages: bool,
    include_hook_events: bool,
    bare: bool,
    fork_session: bool,
    dangerously_skip_permissions: bool,
    init_timeout: Option<Duration>,
    extra_args: Vec<(String, Option<String>)>,
    permission_prompt_tool_name: Option<String>,

    /// Path to the CLI binary. Defaults to `"claude"` when unset. Tests may
    /// override this with a fast-failing binary like `/usr/bin/false`.
    binary: Option<String>,

    // Optional byte-faithful transcript logs (stdin -> Claude, Claude -> stdout).
    tx_log: Option<Arc<TextProtoLog>>,
    rx_log: Option<Arc<TextProtoLog>>,

    // ---- Phase 6 additions ----
    env: std::collections::HashMap<String, String>,
    system_prompt: Option<String>,
    system_prompt_file: Option<PathBuf>,
    max_thinking_tokens: Option<u32>,
    mcp_config_tempfile: Option<Arc<tempfile::NamedTempFile>>,
    settings_arg: Option<String>,
    agents: Option<serde_json::Value>,
    tools_override: Option<Vec<String>>,
    plugin_dirs: Vec<PathBuf>,
    strict_mcp_config: bool,
    fallback_model: Option<String>,
    session_persistence: bool,
    effort: Option<String>,
    allow_dangerously_skip_permissions: bool,
}

impl ProcessBuilder {
    pub fn new(stdin_rx: mpsc::Receiver<String>, stdout_tx: mpsc::Sender<SdkMessage>) -> Self {
        Self {
            stdin_rx,
            stdout_tx,
            session_id: None,
            model: None,
            max_turns: None,
            cwd: None,
            allowed_tools: None,
            disallowed_tools: None,
            permission_mode: None,
            append_system_prompt: None,
            append_system_prompt_file: None,
            add_dirs: Vec::new(),
            mcp_config: None,
            setting_sources: Vec::new(),
            include_partial_messages: false,
            include_hook_events: false,
            bare: false,
            fork_session: false,
            dangerously_skip_permissions: false,
            init_timeout: None,
            extra_args: Vec::new(),
            permission_prompt_tool_name: None,
            binary: None,
            tx_log: None,
            rx_log: None,
            env: std::collections::HashMap::new(),
            system_prompt: None,
            system_prompt_file: None,
            max_thinking_tokens: None,
            mcp_config_tempfile: None,
            settings_arg: None,
            agents: None,
            tools_override: None,
            plugin_dirs: Vec::new(),
            strict_mcp_config: false,
            fallback_model: None,
            session_persistence: true,
            effort: None,
            allow_dangerously_skip_permissions: false,
        }
    }

    // ---- Phase 6 builder setters ----

    pub fn env_map(mut self, env: std::collections::HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn system_prompt_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.system_prompt_file = Some(path.into());
        self
    }

    pub fn max_thinking_tokens(mut self, tokens: u32) -> Self {
        self.max_thinking_tokens = Some(tokens);
        self
    }

    pub fn mcp_config_tempfile(mut self, file: Arc<tempfile::NamedTempFile>) -> Self {
        self.mcp_config_tempfile = Some(file);
        self
    }

    pub fn settings_arg(mut self, value: impl Into<String>) -> Self {
        self.settings_arg = Some(value.into());
        self
    }

    pub fn agents(mut self, value: serde_json::Value) -> Self {
        self.agents = Some(value);
        self
    }

    pub fn tools_override(mut self, tools: Vec<String>) -> Self {
        self.tools_override = Some(tools);
        self
    }

    pub fn plugin_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.plugin_dirs = dirs;
        self
    }

    pub fn strict_mcp_config(mut self, enabled: bool) -> Self {
        self.strict_mcp_config = enabled;
        self
    }

    pub fn fallback_model(mut self, model: impl Into<String>) -> Self {
        self.fallback_model = Some(model.into());
        self
    }

    pub fn session_persistence(mut self, enabled: bool) -> Self {
        self.session_persistence = enabled;
        self
    }

    pub fn effort(mut self, value: impl Into<String>) -> Self {
        self.effort = Some(value.into());
        self
    }

    pub fn allow_dangerously_skip_permissions(mut self, enabled: bool) -> Self {
        self.allow_dangerously_skip_permissions = enabled;
        self
    }

    #[cfg(not(test))]
    pub fn binary_path(mut self, path: impl Into<String>) -> Self {
        self.binary = Some(path.into());
        self
    }

    pub fn text_proto_logs(mut self, tx_log: Arc<TextProtoLog>, rx_log: Arc<TextProtoLog>) -> Self {
        self.tx_log = Some(tx_log);
        self.rx_log = Some(rx_log);
        self
    }

    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn max_turns(mut self, turns: u32) -> Self {
        self.max_turns = Some(turns);
        self
    }

    pub fn cwd(mut self, dir: impl Into<String>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn allowed_tools(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools = Some(tools);
        self
    }

    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = Some(mode);
        self
    }

    pub fn include_partial_messages(mut self, enabled: bool) -> Self {
        self.include_partial_messages = enabled;
        self
    }

    pub fn include_hook_events(mut self, enabled: bool) -> Self {
        self.include_hook_events = enabled;
        self
    }

    pub fn dangerously_skip_permissions(mut self, enabled: bool) -> Self {
        self.dangerously_skip_permissions = enabled;
        self
    }

    pub fn init_timeout(mut self, timeout: Duration) -> Self {
        self.init_timeout = Some(timeout);
        self
    }

    pub fn disallowed_tools(mut self, tools: Vec<String>) -> Self {
        self.disallowed_tools = Some(tools);
        self
    }

    pub fn append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.append_system_prompt = Some(prompt.into());
        self
    }

    pub fn append_system_prompt_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.append_system_prompt_file = Some(path.into());
        self
    }

    pub fn add_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.add_dirs = dirs;
        self
    }

    pub fn mcp_config(mut self, config: impl Into<String>) -> Self {
        self.mcp_config = Some(config.into());
        self
    }

    pub fn setting_sources(mut self, sources: Vec<SettingSource>) -> Self {
        self.setting_sources = sources;
        self
    }

    pub fn bare(mut self, enabled: bool) -> Self {
        self.bare = enabled;
        self
    }

    pub fn fork_session(mut self, enabled: bool) -> Self {
        self.fork_session = enabled;
        self
    }

    pub fn extra_args(mut self, args: Vec<(String, Option<String>)>) -> Self {
        self.extra_args = args;
        self
    }

    pub fn permission_prompt_tool_name(mut self, name: impl Into<String>) -> Self {
        self.permission_prompt_tool_name = Some(name.into());
        self
    }

    #[cfg(test)]
    fn binary(mut self, path: impl Into<String>) -> Self {
        self.binary = Some(path.into());
        self
    }

    /// Default build method - generates a session id if missing, then creates
    /// a new session, otherwise resumes the supplied session id.
    pub async fn build(self) -> Result<Process, SessionError> {
        if self.session_id.is_none() {
            let mut builder = self;
            builder.session_id = Some(uuid::Uuid::new_v4().to_string());
            builder.new_session().await
        } else {
            self.resume_session().await
        }
    }

    /// Create a new session (fails if session already exists)
    pub async fn new_session(self) -> Result<Process, SessionError> {
        self.launch(false).await
    }

    /// Resume an existing session (fails if session doesn't exist)
    pub async fn resume_session(self) -> Result<Process, SessionError> {
        self.launch(true).await
    }

    /// Spawn the subprocess and return immediately. The CLI in `--print
    /// --input-format stream-json` mode does not emit a system init
    /// message until the first user message is sent, so we cannot block
    /// on init here. The stdout handler still forwards every message
    /// (including a future init) through the normal channel.
    async fn launch(self, use_resume: bool) -> Result<Process, SessionError> {
        let _session_id = self
            .session_id
            .as_ref()
            .ok_or(SessionError::SessionIdMissing)?
            .clone();

        let args = self.build_args(use_resume);
        let mut child = self
            .spawn_child(args)
            .await
            .map_err(|e| SessionError::SpawnFailed { source: e })?;

        let stdin_rx = self.stdin_rx;
        let stdout_tx = self.stdout_tx;
        let tx_log = self.tx_log;
        let rx_log = self.rx_log;

        let (stdin, stdout, stderr) = match Self::extract_stdio(&mut child) {
            Ok(stdio) => stdio,
            Err(e) => {
                let _ = child.kill().await;
                return Err(e);
            },
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (stdin_handle, stdout_handle, stderr_handle) = Self::setup_handlers(HandlerDeps {
            stdin,
            stdout,
            stderr,
            stdin_rx,
            stdout_tx,
            tx_log: tx_log.clone(),
            rx_log: rx_log.clone(),
            init_tx: None,
            shutdown_rx,
        });

        Ok(Process {
            child,
            stdin_handle,
            stdout_handle,
            stderr_handle,
            shutdown_tx: Some(shutdown_tx),
            tx_log,
            rx_log,
            _mcp_config_tempfile: self.mcp_config_tempfile.clone(),
        })
    }

    // Build command arguments
    fn build_args(&self, use_resume: bool) -> Vec<String> {
        let mut args = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];

        // Session handling
        if use_resume {
            args.push("--resume".to_string());
        } else {
            args.push("--session-id".to_string());
        }
        args.push(
            self.session_id
                .as_ref()
                .expect("session_id should be set when building args")
                .clone(),
        );

        // Optional arguments
        if let Some(model) = &self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }

        if let Some(max_turns) = self.max_turns {
            args.push("--max-turns".to_string());
            args.push(max_turns.to_string());
        }

        if let Some(tools) = &self.allowed_tools
            && !tools.is_empty()
        {
            args.push("--allowedTools".to_string());
            args.push(tools.join(","));
        }

        if let Some(tools) = &self.disallowed_tools
            && !tools.is_empty()
        {
            args.push("--disallowedTools".to_string());
            args.push(tools.join(","));
        }

        if let Some(mode) = &self.permission_mode {
            let mode_str = match mode {
                PermissionMode::Auto => "auto",
                PermissionMode::Default => "default",
                PermissionMode::AcceptEdits => "acceptEdits",
                PermissionMode::DontAsk => "dontAsk",
                PermissionMode::BypassPermissions => "bypassPermissions",
                PermissionMode::Plan => "plan",
            };
            args.push("--permission-mode".to_string());
            args.push(mode_str.to_string());
        }

        if let Some(prompt) = &self.append_system_prompt {
            args.push("--append-system-prompt".to_string());
            args.push(prompt.clone());
        }
        if let Some(path) = &self.append_system_prompt_file {
            args.push("--append-system-prompt-file".to_string());
            args.push(path.to_string_lossy().into_owned());
        }

        for dir in &self.add_dirs {
            args.push("--add-dir".to_string());
            args.push(dir.to_string_lossy().into_owned());
        }

        if let Some(config) = &self.mcp_config {
            args.push("--mcp-config".to_string());
            args.push(config.clone());
        }

        if !self.setting_sources.is_empty() {
            let joined = self
                .setting_sources
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",");
            args.push("--setting-sources".to_string());
            args.push(joined);
        }

        if self.include_partial_messages {
            args.push("--include-partial-messages".to_string());
        }
        if self.include_hook_events {
            args.push("--include-hook-events".to_string());
        }
        if self.bare {
            args.push("--bare".to_string());
        }
        if self.fork_session {
            args.push("--fork-session".to_string());
        }
        if self.dangerously_skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        }

        if let Some(name) = &self.permission_prompt_tool_name {
            args.push("--permission-prompt-tool-name".to_string());
            args.push(name.clone());
        }

        // Phase 6 additions - emit the remaining CLI flags.
        if let Some(prompt) = &self.system_prompt {
            args.push("--system-prompt".to_string());
            args.push(prompt.clone());
        }
        if let Some(path) = &self.system_prompt_file {
            args.push("--system-prompt-file".to_string());
            args.push(path.to_string_lossy().into_owned());
        }
        if let Some(tempfile) = &self.mcp_config_tempfile {
            args.push("--mcp-config".to_string());
            args.push(tempfile.path().to_string_lossy().into_owned());
        }
        if let Some(value) = &self.settings_arg {
            args.push("--settings".to_string());
            args.push(value.clone());
        }
        if let Some(agents) = &self.agents {
            args.push("--agents".to_string());
            args.push(agents.to_string());
        }
        if let Some(tools) = &self.tools_override
            && !tools.is_empty()
        {
            args.push("--tools".to_string());
            args.push(tools.join(","));
        }
        for dir in &self.plugin_dirs {
            args.push("--plugin-dir".to_string());
            args.push(dir.to_string_lossy().into_owned());
        }
        if self.strict_mcp_config {
            args.push("--strict-mcp-config".to_string());
        }
        if let Some(model) = &self.fallback_model {
            args.push("--fallback-model".to_string());
            args.push(model.clone());
        }
        if !self.session_persistence {
            args.push("--no-session-persistence".to_string());
        }
        if let Some(effort) = &self.effort {
            args.push("--effort".to_string());
            args.push(effort.clone());
        }
        if self.allow_dangerously_skip_permissions {
            args.push("--allow-dangerously-skip-permissions".to_string());
        }

        for (flag, value) in &self.extra_args {
            args.push(flag.clone());
            if let Some(v) = value {
                args.push(v.clone());
            }
        }

        args
    }

    // Spawn the child process
    async fn spawn_child(&self, args: Vec<String>) -> Result<Child, std::io::Error> {
        let binary = self.binary.as_deref().unwrap_or("claude");
        let mut cmd = Command::new(binary);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }

        // Inject configured env vars (Phase 6).
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        // `MAX_THINKING_TOKENS` is read from env by the CLI when
        // extended thinking is active; translate the builder field.
        if let Some(tokens) = self.max_thinking_tokens {
            cmd.env("MAX_THINKING_TOKENS", tokens.to_string());
        }

        debug!("Spawning {binary} process with args: {:?}", args);
        cmd.spawn()
    }

    // Extract stdio handles
    fn extract_stdio(
        child: &mut Child,
    ) -> Result<(ChildStdin, ChildStdout, ChildStderr), SessionError> {
        let stdin = child.stdin.take().ok_or(SessionError::StdioFailed)?;
        let stdout = child.stdout.take().ok_or(SessionError::StdioFailed)?;
        let stderr = child.stderr.take().ok_or(SessionError::StdioFailed)?;
        Ok((stdin, stdout, stderr))
    }

    // Setup all handlers
    fn setup_handlers(
        deps: HandlerDeps,
    ) -> (
        JoinHandle<mpsc::Receiver<String>>,
        JoinHandle<mpsc::Sender<SdkMessage>>,
        JoinHandle<String>,
    ) {
        let stdin_handle =
            Self::spawn_stdin_handler(deps.stdin, deps.stdin_rx, deps.tx_log, deps.shutdown_rx);
        let stdout_handle =
            Self::spawn_stdout_handler(deps.stdout, deps.stdout_tx, deps.rx_log, deps.init_tx);
        let stderr_handle = Self::spawn_stderr_handler(deps.stderr);

        (stdin_handle, stdout_handle, stderr_handle)
    }

    // Stdin handler
    fn spawn_stdin_handler(
        stdin: ChildStdin,
        mut stdin_rx: mpsc::Receiver<String>,
        tx_log: Option<Arc<TextProtoLog>>,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) -> JoinHandle<mpsc::Receiver<String>> {
        tokio::spawn(async move {
            let mut stdin = stdin;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("stdin handler received shutdown signal");
                        break stdin_rx;
                    }
                    msg = stdin_rx.recv() => match msg {
                        Some(message) => {
                            if let Some(log) = &tx_log {
                                log.record(&message);
                            }
                            if let Err(e) = Self::write_message(&mut stdin, message).await {
                                error!("Failed to write to stdin: {}", e);
                                break stdin_rx;
                            }
                        }
                        None => {
                            debug!("stdin channel closed");
                            break stdin_rx;
                        }
                    }
                }
            }
        })
    }

    async fn write_message(stdin: &mut ChildStdin, message: String) -> std::io::Result<()> {
        stdin.write_all(message.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    }

    // Stdout handler
    fn spawn_stdout_handler(
        stdout: ChildStdout,
        stdout_tx: mpsc::Sender<SdkMessage>,
        rx_log: Option<Arc<TextProtoLog>>,
        init_tx: Option<oneshot::Sender<()>>,
    ) -> JoinHandle<mpsc::Sender<SdkMessage>> {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut init_tx = init_tx;

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        debug!("Claude Code stdout closed");
                        break stdout_tx;
                    },
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            if let Some(log) = &rx_log {
                                log.record(trimmed);
                            }
                            match serde_json::from_str::<SdkMessage>(trimmed) {
                                Ok(message) => {
                                    trace!("Received message: {:?}", message);
                                    if matches!(
                                        &message,
                                        SdkMessage::System {
                                            subtype: SystemSubtype::Init,
                                            ..
                                        }
                                    ) && let Some(tx) = init_tx.take()
                                    {
                                        let _ = tx.send(());
                                    }
                                    if stdout_tx.send(message).await.is_err() {
                                        error!("Failed to send message to channel");
                                        break stdout_tx;
                                    }
                                },
                                Err(e) => {
                                    error!(
                                        "Failed to parse JSON message: {} - Line: {}",
                                        e, trimmed
                                    );
                                },
                            }
                        }
                    },
                    Err(e) => {
                        error!("Failed to read from stdout: {}", e);
                        break stdout_tx;
                    },
                }
            }
        })
    }

    // Stderr handler that maintains a small buffer
    fn spawn_stderr_handler(stderr: ChildStderr) -> JoinHandle<String> {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            let mut last_error = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break last_error, // Return last error on EOF
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            error!("Claude Code stderr: {}", trimmed);
                            // Keep the last error line - any non-empty stderr is likely an error
                            last_error = trimmed.to_string();
                        }
                    },
                    Err(e) => {
                        error!("Failed to read from stderr: {}", e);
                        break last_error;
                    },
                }
            }
        })
    }
}

impl Process {
    /// Create a new builder
    pub fn builder(
        stdin_rx: mpsc::Receiver<String>,
        stdout_tx: mpsc::Sender<SdkMessage>,
    ) -> ProcessBuilder {
        ProcessBuilder::new(stdin_rx, stdout_tx)
    }

    /// Close the process and recover the channels.
    ///
    /// Kills the child and signals the stdin handler via its cooperative
    /// shutdown oneshot, then awaits each handler task to exit cleanly.
    /// The stdout and stderr handlers exit on their own once the child
    /// dies (child stdio pipes close), so they need no explicit signal.
    /// After the handlers finish, any attached transcript logs are
    /// flushed so their tail bytes land on disk.
    pub async fn close(mut self) -> Result<RecoveredChannels, CloseError> {
        self.child.kill().await.context(KillFailedSnafu)?;

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        let stdin_rx = self
            .stdin_handle
            .await
            .map_err(|_| CloseError::HandlerPanicked {
                task: "stdin".to_string(),
            })?;
        let stdout_tx = self
            .stdout_handle
            .await
            .map_err(|_| CloseError::HandlerPanicked {
                task: "stdout".to_string(),
            })?;
        let last_stderr = self.stderr_handle.await.unwrap_or_default();

        if let Some(log) = &self.tx_log {
            log.flush();
        }
        if let Some(log) = &self.rx_log {
            log.flush();
        }

        if !last_stderr.is_empty() {
            debug!("Process closed with last stderr: {}", last_stderr);
        }

        Ok(RecoveredChannels {
            stdin_rx,
            stdout_tx,
            last_stderr,
        })
    }

    /// Check if the process is still alive
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder_with_session_id() -> ProcessBuilder {
        let (_stdin_tx, stdin_rx) = mpsc::channel::<String>(1);
        let (stdout_tx, _stdout_rx) = mpsc::channel::<SdkMessage>(1);
        ProcessBuilder::new(stdin_rx, stdout_tx).session_id("sess-1")
    }

    fn index_of(args: &[String], needle: &str) -> Option<usize> {
        args.iter().position(|a| a == needle)
    }

    #[test]
    fn build_args_contains_required_baseline() {
        let args = builder_with_session_id().build_args(false);
        assert!(args.contains(&"--print".to_string()));
        assert_eq!(
            args.iter().filter(|a| *a == "--output-format").count(),
            1,
            "args: {args:?}"
        );
        assert!(
            index_of(&args, "--output-format")
                .map(|i| args.get(i + 1) == Some(&"stream-json".to_string()))
                .unwrap_or(false)
        );
        assert!(
            index_of(&args, "--input-format")
                .map(|i| args.get(i + 1) == Some(&"stream-json".to_string()))
                .unwrap_or(false)
        );
        assert!(args.contains(&"--verbose".to_string()));
        assert!(args.contains(&"--session-id".to_string()));
        assert!(args.contains(&"sess-1".to_string()));
    }

    #[test]
    fn build_args_never_emits_broken_flags() {
        let args = builder_with_session_id()
            .cwd("/tmp")
            .allowed_tools(vec!["Read".into(), "Write".into()])
            .build_args(false);
        assert!(
            !args.contains(&"--tool".to_string()),
            "'--tool' is not a real flag; args: {args:?}"
        );
        assert!(
            !args.contains(&"--cwd".to_string()),
            "'--cwd' is not a real flag; args: {args:?}"
        );
    }

    #[test]
    fn build_args_allowed_tools_single_comma_joined() {
        let args = builder_with_session_id()
            .allowed_tools(vec!["Read".into(), "Write".into(), "Bash".into()])
            .build_args(false);
        let idx = index_of(&args, "--allowedTools")
            .expect("--allowedTools flag missing in args: {args:?}");
        assert_eq!(args.get(idx + 1), Some(&"Read,Write,Bash".to_string()));
        assert_eq!(
            args.iter().filter(|a| *a == "--allowedTools").count(),
            1,
            "must emit a single --allowedTools arg, not one per tool"
        );
    }

    #[test]
    fn build_args_allowed_tools_empty_omits_flag() {
        let args = builder_with_session_id()
            .allowed_tools(vec![])
            .build_args(false);
        assert!(!args.contains(&"--allowedTools".to_string()));
    }

    #[test]
    fn build_args_permission_mode_uses_camel_case() {
        let cases = [
            (PermissionMode::Auto, "auto"),
            (PermissionMode::Default, "default"),
            (PermissionMode::AcceptEdits, "acceptEdits"),
            (PermissionMode::DontAsk, "dontAsk"),
            (PermissionMode::BypassPermissions, "bypassPermissions"),
            (PermissionMode::Plan, "plan"),
        ];
        for (mode, expected) in cases {
            let args = builder_with_session_id()
                .permission_mode(mode)
                .build_args(false);
            let idx = index_of(&args, "--permission-mode").expect("flag missing");
            assert_eq!(args.get(idx + 1), Some(&expected.to_string()));
        }
    }

    #[test]
    fn build_args_discovery_flags_off_by_default() {
        let args = builder_with_session_id().build_args(false);
        assert!(!args.contains(&"--include-partial-messages".to_string()));
        assert!(!args.contains(&"--include-hook-events".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn build_args_discovery_flags_emit_when_enabled() {
        let args = builder_with_session_id()
            .include_partial_messages(true)
            .include_hook_events(true)
            .dangerously_skip_permissions(true)
            .build_args(false);
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(args.contains(&"--include-hook-events".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn build_args_resume_uses_resume_flag() {
        let args = builder_with_session_id().build_args(true);
        assert!(args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn build_args_disallowed_tools_single_comma_joined() {
        let args = builder_with_session_id()
            .disallowed_tools(vec!["Bash".into(), "Write".into()])
            .build_args(false);
        let idx = index_of(&args, "--disallowedTools").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"Bash,Write".to_string()));
    }

    #[test]
    fn build_args_append_system_prompt_inline() {
        let args = builder_with_session_id()
            .append_system_prompt("extra instructions")
            .build_args(false);
        let idx = index_of(&args, "--append-system-prompt").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"extra instructions".to_string()));
    }

    #[test]
    fn build_args_append_system_prompt_file() {
        let args = builder_with_session_id()
            .append_system_prompt_file(PathBuf::from("/tmp/prompt.txt"))
            .build_args(false);
        let idx = index_of(&args, "--append-system-prompt-file").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"/tmp/prompt.txt".to_string()));
    }

    #[test]
    fn build_args_add_dir_repeated_per_path() {
        let args = builder_with_session_id()
            .add_dirs(vec![PathBuf::from("/a"), PathBuf::from("/b")])
            .build_args(false);
        let add_dir_indices: Vec<_> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--add-dir")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(add_dir_indices.len(), 2);
        assert_eq!(args.get(add_dir_indices[0] + 1), Some(&"/a".to_string()));
        assert_eq!(args.get(add_dir_indices[1] + 1), Some(&"/b".to_string()));
    }

    #[test]
    fn build_args_setting_sources_comma_joined() {
        let args = builder_with_session_id()
            .setting_sources(vec![SettingSource::User, SettingSource::Project])
            .build_args(false);
        let idx = index_of(&args, "--setting-sources").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"user,project".to_string()));
    }

    #[test]
    fn build_args_mcp_config_passes_blob() {
        let args = builder_with_session_id()
            .mcp_config("{\"mcpServers\":{}}")
            .build_args(false);
        let idx = index_of(&args, "--mcp-config").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"{\"mcpServers\":{}}".to_string()));
    }

    #[test]
    fn build_args_bare_fork_session_flags() {
        let args = builder_with_session_id()
            .bare(true)
            .fork_session(true)
            .build_args(false);
        assert!(args.contains(&"--bare".to_string()));
        assert!(args.contains(&"--fork-session".to_string()));
    }

    #[test]
    fn build_args_extra_args_appended_last() {
        let args = builder_with_session_id()
            .extra_args(vec![
                ("--experimental".into(), None),
                ("--custom-flag".into(), Some("value".into())),
            ])
            .build_args(false);
        assert!(args.contains(&"--experimental".to_string()));
        let idx = index_of(&args, "--custom-flag").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"value".to_string()));
    }

    #[test]
    fn build_args_permission_prompt_tool_name_emitted_when_set() {
        let args = builder_with_session_id()
            .permission_prompt_tool_name("stdio")
            .build_args(false);
        let idx = index_of(&args, "--permission-prompt-tool-name").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"stdio".to_string()));
    }

    #[test]
    fn build_args_permission_prompt_tool_name_absent_by_default() {
        let args = builder_with_session_id().build_args(false);
        assert!(!args.contains(&"--permission-prompt-tool-name".to_string()));
    }

    /// Returns the first path from `candidates` that exists, or `None`. Used
    /// to find a POSIX fast-fail binary across platforms (macOS has
    /// `/usr/bin/false`, Linux usually `/bin/false` and `/usr/bin/false`).
    fn first_existing_path(candidates: &[&str]) -> Option<&'static str> {
        for path in candidates {
            if std::path::Path::new(path).exists() {
                // Path returned from candidates; leak the &str to 'static.
                // Candidates are themselves &'static str, so just return.
                return Some(match *path {
                    "/bin/false" => "/bin/false",
                    "/usr/bin/false" => "/usr/bin/false",
                    _ => continue,
                });
            }
        }
        None
    }

    #[tokio::test]
    async fn launch_returns_immediately_even_if_process_exits_fast() {
        let Some(false_bin) = first_existing_path(&["/bin/false", "/usr/bin/false"]) else {
            eprintln!("skipping: no /bin/false on this platform");
            return;
        };

        let (_stdin_tx, stdin_rx) = mpsc::channel::<String>(1);
        let (stdout_tx, _stdout_rx) = mpsc::channel::<SdkMessage>(1);

        let start = std::time::Instant::now();
        let result = ProcessBuilder::new(stdin_rx, stdout_tx)
            .session_id("sess-race-test")
            .binary(false_bin)
            .new_session()
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "launch should succeed immediately");
        assert!(
            elapsed < Duration::from_secs(2),
            "launch should not block, took {elapsed:?}",
        );
    }

    #[tokio::test]
    async fn launch_returns_immediately_without_waiting_for_init() {
        let sleep_bin =
            match first_existing_path(&["/bin/sleep", "/usr/bin/sleep"]).or(Some("sleep")) {
                Some(p) => p,
                None => {
                    eprintln!("skipping: no sleep binary");
                    return;
                },
            };

        let (_stdin_tx, stdin_rx) = mpsc::channel::<String>(1);
        let (stdout_tx, _stdout_rx) = mpsc::channel::<SdkMessage>(1);

        let start = std::time::Instant::now();
        let result = ProcessBuilder::new(stdin_rx, stdout_tx)
            .session_id("sess-no-wait-test")
            .binary(sleep_bin)
            .new_session()
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "launch should succeed without init");
        assert!(
            elapsed < Duration::from_secs(2),
            "launch should not wait for init, took {elapsed:?}",
        );
    }
}
