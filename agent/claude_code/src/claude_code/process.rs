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

const DEFAULT_INIT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitRaceOutcome {
    Initialized,
    EarlyExit,
    Timeout,
    StdoutHandlerDied,
}

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
        }
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

    /// Spawn the subprocess and wait for the first `system(init)` message,
    /// an early process exit, or the init timeout - whichever comes first.
    /// A [`Process`] is only returned when init is observed.
    async fn launch(self, use_resume: bool) -> Result<Process, SessionError> {
        let session_id = self
            .session_id
            .as_ref()
            .ok_or(SessionError::SessionIdMissing)?
            .clone();
        let init_timeout = self.init_timeout.unwrap_or(DEFAULT_INIT_TIMEOUT);

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

        let (init_tx, init_rx) = oneshot::channel::<()>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (stdin_handle, stdout_handle, stderr_handle) = Self::setup_handlers(HandlerDeps {
            stdin,
            stdout,
            stderr,
            stdin_rx,
            stdout_tx,
            tx_log: tx_log.clone(),
            rx_log: rx_log.clone(),
            init_tx: Some(init_tx),
            shutdown_rx,
        });

        let mut process = Process {
            child,
            stdin_handle,
            stdout_handle,
            stderr_handle,
            shutdown_tx: Some(shutdown_tx),
            tx_log,
            rx_log,
        };

        let outcome = tokio::select! {
            init = init_rx => match init {
                Ok(()) => InitRaceOutcome::Initialized,
                Err(_) => InitRaceOutcome::StdoutHandlerDied,
            },
            _ = process.child.wait() => InitRaceOutcome::EarlyExit,
            _ = tokio::time::sleep(init_timeout) => InitRaceOutcome::Timeout,
        };

        if outcome == InitRaceOutcome::Initialized {
            return Ok(process);
        }

        match process.close().await {
            Ok(recovered) => {
                let stderr_tail = recovered.last_stderr;
                if use_resume && stderr_tail.contains("No conversation found with session ID") {
                    return Err(SessionError::SessionNotFound { session_id });
                }
                if !use_resume && stderr_tail.contains("is already in use") {
                    return Err(SessionError::SessionAlreadyExists { session_id });
                }
                let context = match outcome {
                    InitRaceOutcome::Timeout => format!(
                        "Timed out waiting for init after {}s (stderr: {stderr_tail})",
                        init_timeout.as_secs()
                    ),
                    InitRaceOutcome::EarlyExit => {
                        format!("Process exited before init (stderr: {stderr_tail})")
                    },
                    InitRaceOutcome::StdoutHandlerDied => {
                        format!("stdout handler terminated before init (stderr: {stderr_tail})")
                    },
                    InitRaceOutcome::Initialized => unreachable!(),
                };
                Err(SessionError::SpawnFailed {
                    source: std::io::Error::other(context),
                })
            },
            Err(e) => Err(SessionError::ChannelRecoveryFailed { source: e }),
        }
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
                PermissionMode::Default => "default",
                PermissionMode::AcceptEdits => "acceptEdits",
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
        let args = builder_with_session_id()
            .permission_mode(PermissionMode::AcceptEdits)
            .build_args(false);
        let idx = index_of(&args, "--permission-mode").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"acceptEdits".to_string()));

        let args = builder_with_session_id()
            .permission_mode(PermissionMode::BypassPermissions)
            .build_args(false);
        let idx = index_of(&args, "--permission-mode").expect("flag missing");
        assert_eq!(args.get(idx + 1), Some(&"bypassPermissions".to_string()));
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
    async fn launch_early_exit_returns_error_within_short_time() {
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
            .init_timeout(Duration::from_secs(10))
            .new_session()
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected error from fast-exit binary");
        assert!(
            elapsed < Duration::from_secs(2),
            "race should resolve fast on early exit, took {elapsed:?}",
        );
    }

    #[tokio::test]
    async fn launch_respects_init_timeout_with_slow_binary() {
        // `sleep 60` never emits init; the race should abort on timeout far
        // sooner than the 60s sleep completes.
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
            .session_id("sess-timeout-test")
            .binary(sleep_bin)
            .init_timeout(Duration::from_millis(300))
            .new_session()
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected timeout error from slow binary");
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout branch should fire in ~300ms, took {elapsed:?}",
        );
        match result {
            Err(SessionError::SpawnFailed { source }) => {
                let msg = source.to_string();
                assert!(
                    msg.contains("Timed out") || msg.contains("terminated"),
                    "expected timeout error, got: {msg}"
                );
            },
            Err(other) => panic!("expected SpawnFailed, got {other:?}"),
            Ok(_) => panic!("expected error, got success"),
        }
    }
}
