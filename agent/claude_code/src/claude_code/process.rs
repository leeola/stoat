use crate::messages::{PermissionMode, SdkMessage};
use snafu::{ResultExt, Snafu};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::mpsc,
    task::JoinHandle,
};
use tracing::{debug, error, trace};

/// Channels needed for process communication that can be recovered on error
pub struct ProcessChannels {
    pub stdin_rx: mpsc::Receiver<String>,
    pub stdout_tx: mpsc::Sender<SdkMessage>,
}

#[derive(Debug, Snafu)]
pub enum NewSessionError {
    #[snafu(display("Session ID {} is already in use", session_id))]
    SessionAlreadyExists { session_id: String },

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
pub enum ResumeSessionError {
    #[snafu(display("No conversation found with session ID: {}", session_id))]
    SessionNotFound { session_id: String },

    #[snafu(display("Failed to recover channels from failed process"))]
    ResumeChannelRecoveryFailed { source: CloseError },

    #[snafu(display("Failed to spawn Claude process"))]
    ResumeSpawnFailed { source: std::io::Error },

    #[snafu(display("Failed to get process stdio"))]
    ResumeStdioFailed,

    #[snafu(display("session_id is required but not provided"))]
    ResumeSessionIdMissing,
}

#[derive(Debug, Snafu)]
pub enum BuildError {
    #[snafu(display("Failed to create new session"))]
    NewSession { source: NewSessionError },

    #[snafu(display("Failed to resume session"))]
    ResumeSession { source: ResumeSessionError },
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
    permission_mode: Option<PermissionMode>,
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
            permission_mode: None,
        }
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

    /// Default build method - simplified to only generate session_id if missing
    pub async fn build(self) -> Result<Process, (ProcessChannels, BuildError)> {
        // This method should only be used as a fallback when session_id is missing
        // ClaudeCode should normally call new_session() or resume_session() directly
        if self.session_id.is_none() {
            // No session ID provided - generate one and create new
            let mut builder = self;
            builder.session_id = Some(uuid::Uuid::new_v4().to_string());
            match builder.new_session().await {
                Ok(process) => Ok(process),
                Err((channels, e)) => Err((channels, BuildError::NewSession { source: e })),
            }
        } else {
            // If session_id is present, caller should use resume_session() or new_session()
            // directly Default to resume for backward compatibility
            match self.resume_session().await {
                Ok(process) => Ok(process),
                Err((channels, e)) => Err((channels, BuildError::ResumeSession { source: e })),
            }
        }
    }

    /// Create a new session (fails if session already exists)
    pub async fn new_session(self) -> Result<Process, (ProcessChannels, NewSessionError)> {
        // Validate session_id is present
        let session_id = match &self.session_id {
            Some(id) => id.clone(),
            None => {
                // Return channels with error since we own them
                return Err((
                    ProcessChannels {
                        stdin_rx: self.stdin_rx,
                        stdout_tx: self.stdout_tx,
                    },
                    NewSessionError::SessionIdMissing,
                ));
            },
        };

        // Build args and spawn child before taking ownership of channels
        let args = self.build_args(false);
        let mut child = match self.spawn_child(args).await {
            Ok(child) => child,
            Err(e) => {
                // Take ownership of channels to return them
                return Err((
                    ProcessChannels {
                        stdin_rx: self.stdin_rx,
                        stdout_tx: self.stdout_tx,
                    },
                    NewSessionError::SpawnFailed { source: e },
                ));
            },
        };

        // Now take ownership of channels
        let stdin_rx = self.stdin_rx;
        let stdout_tx = self.stdout_tx;

        let (stdin, stdout, stderr) = match Self::extract_stdio(&mut child) {
            Ok(stdio) => stdio,
            Err(e) => {
                let _ = child.kill().await;
                return Err((
                    ProcessChannels {
                        stdin_rx,
                        stdout_tx,
                    },
                    e,
                ));
            },
        };

        let (stdin_handle, stdout_handle, stderr_handle) =
            Self::setup_handlers(stdin, stdout, stderr, stdin_rx, stdout_tx);

        // Check for early exit/errors
        let mut process = Process {
            child,
            stdin_handle,
            stdout_handle,
            stderr_handle,
        };

        // Wait briefly to see if process fails to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        if let Some(_error_msg) = process.check_early_exit().await {
            // Process exited early, close it to get the actual stderr
            match process.close().await {
                Ok(recovered) => {
                    // Check the actual stderr message
                    if recovered.last_stderr.contains("is already in use") {
                        return Err((
                            ProcessChannels {
                                stdin_rx: recovered.stdin_rx,
                                stdout_tx: recovered.stdout_tx,
                            },
                            NewSessionError::SessionAlreadyExists {
                                session_id: session_id.clone(),
                            },
                        ));
                    } else {
                        // Some other error occurred - still return channels
                        return Err((
                            ProcessChannels {
                                stdin_rx: recovered.stdin_rx,
                                stdout_tx: recovered.stdout_tx,
                            },
                            NewSessionError::SpawnFailed {
                                source: std::io::Error::other(format!(
                                    "Process failed: {}",
                                    recovered.last_stderr
                                )),
                            },
                        ));
                    }
                },
                Err(e) => {
                    // Can't recover channels, return a different error
                    // We need to create dummy channels since we must return them
                    let (dummy_stdin_tx, dummy_stdin_rx) = mpsc::channel(1);
                    let (dummy_stdout_tx, _) = mpsc::channel(1);
                    drop(dummy_stdin_tx); // Close the channel
                    return Err((
                        ProcessChannels {
                            stdin_rx: dummy_stdin_rx,
                            stdout_tx: dummy_stdout_tx,
                        },
                        NewSessionError::ChannelRecoveryFailed { source: e },
                    ));
                },
            }
        }

        Ok(process)
    }

    /// Resume an existing session (fails if session doesn't exist)  
    pub async fn resume_session(self) -> Result<Process, (ProcessChannels, ResumeSessionError)> {
        // Validate session_id is present
        let session_id = match &self.session_id {
            Some(id) => id.clone(),
            None => {
                // Return channels with error since we own them
                return Err((
                    ProcessChannels {
                        stdin_rx: self.stdin_rx,
                        stdout_tx: self.stdout_tx,
                    },
                    ResumeSessionError::ResumeSessionIdMissing,
                ));
            },
        };

        // Build args and spawn child before taking ownership of channels
        let args = self.build_args(true);
        let mut child = match self.spawn_child(args).await {
            Ok(child) => child,
            Err(e) => {
                // Take ownership of channels to return them
                return Err((
                    ProcessChannels {
                        stdin_rx: self.stdin_rx,
                        stdout_tx: self.stdout_tx,
                    },
                    ResumeSessionError::ResumeSpawnFailed { source: e },
                ));
            },
        };

        // Now take ownership of channels
        let stdin_rx = self.stdin_rx;
        let stdout_tx = self.stdout_tx;

        let (stdin, stdout, stderr) = match Self::extract_stdio(&mut child) {
            Ok(stdio) => stdio,
            Err(_) => {
                let _ = child.kill().await;
                return Err((
                    ProcessChannels {
                        stdin_rx,
                        stdout_tx,
                    },
                    ResumeSessionError::ResumeStdioFailed,
                ));
            },
        };

        let (stdin_handle, stdout_handle, stderr_handle) =
            Self::setup_handlers(stdin, stdout, stderr, stdin_rx, stdout_tx);

        // Check for early exit/errors
        let mut process = Process {
            child,
            stdin_handle,
            stdout_handle,
            stderr_handle,
        };

        // Wait briefly to see if process fails to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        if let Some(_error_msg) = process.check_early_exit().await {
            // Process exited early, close it to get the actual stderr
            match process.close().await {
                Ok(recovered) => {
                    // Check the actual stderr message
                    if recovered
                        .last_stderr
                        .contains("No conversation found with session ID")
                    {
                        return Err((
                            ProcessChannels {
                                stdin_rx: recovered.stdin_rx,
                                stdout_tx: recovered.stdout_tx,
                            },
                            ResumeSessionError::SessionNotFound {
                                session_id: session_id.clone(),
                            },
                        ));
                    } else {
                        // Some other error occurred
                        let (dummy_stdin_tx, dummy_stdin_rx) = mpsc::channel(1);
                        let (dummy_stdout_tx, _) = mpsc::channel(1);
                        drop(dummy_stdin_tx); // Close the channel
                        return Err((
                            ProcessChannels {
                                stdin_rx: dummy_stdin_rx,
                                stdout_tx: dummy_stdout_tx,
                            },
                            ResumeSessionError::ResumeChannelRecoveryFailed {
                                source: CloseError::KillFailed {
                                    source: std::io::Error::other(format!(
                                        "Process failed: {}",
                                        recovered.last_stderr
                                    )),
                                },
                            },
                        ));
                    }
                },
                Err(e) => {
                    // Can't recover channels, return a different error
                    // We need to create dummy channels since we must return them
                    let (dummy_stdin_tx, dummy_stdin_rx) = mpsc::channel(1);
                    let (dummy_stdout_tx, _) = mpsc::channel(1);
                    drop(dummy_stdin_tx); // Close the channel
                    return Err((
                        ProcessChannels {
                            stdin_rx: dummy_stdin_rx,
                            stdout_tx: dummy_stdout_tx,
                        },
                        ResumeSessionError::ResumeChannelRecoveryFailed { source: e },
                    ));
                },
            }
        }

        Ok(process)
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

        if let Some(cwd) = &self.cwd {
            args.push("--cwd".to_string());
            args.push(cwd.clone());
        }

        if let Some(tools) = &self.allowed_tools {
            for tool in tools {
                args.push("--tool".to_string());
                args.push(tool.clone());
            }
        }

        if let Some(mode) = &self.permission_mode {
            let mode_str = match mode {
                PermissionMode::Default => "default",
                PermissionMode::AcceptEdits => "accept-edits",
                PermissionMode::BypassPermissions => "bypass-permissions",
                PermissionMode::Plan => "plan",
            };
            args.push("--permission-mode".to_string());
            args.push(mode_str.to_string());
        }

        args
    }

    // Spawn the child process
    async fn spawn_child(&self, args: Vec<String>) -> Result<Child, std::io::Error> {
        let mut cmd = Command::new("claude");
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        debug!("Spawning Claude Code process with args: {:?}", args);
        cmd.spawn()
    }

    // Extract stdio handles
    fn extract_stdio(
        child: &mut Child,
    ) -> Result<(ChildStdin, ChildStdout, ChildStderr), NewSessionError> {
        let stdin = child.stdin.take().ok_or(NewSessionError::StdioFailed)?;
        let stdout = child.stdout.take().ok_or(NewSessionError::StdioFailed)?;
        let stderr = child.stderr.take().ok_or(NewSessionError::StdioFailed)?;
        Ok((stdin, stdout, stderr))
    }

    // Setup all handlers
    fn setup_handlers(
        stdin: ChildStdin,
        stdout: ChildStdout,
        stderr: ChildStderr,
        stdin_rx: mpsc::Receiver<String>,
        stdout_tx: mpsc::Sender<SdkMessage>,
    ) -> (
        JoinHandle<mpsc::Receiver<String>>,
        JoinHandle<mpsc::Sender<SdkMessage>>,
        JoinHandle<String>,
    ) {
        let stdin_handle = Self::spawn_stdin_handler(stdin, stdin_rx);
        let stdout_handle = Self::spawn_stdout_handler(stdout, stdout_tx);
        let stderr_handle = Self::spawn_stderr_handler(stderr);

        (stdin_handle, stdout_handle, stderr_handle)
    }

    // Stdin handler
    fn spawn_stdin_handler(
        stdin: ChildStdin,
        mut stdin_rx: mpsc::Receiver<String>,
    ) -> JoinHandle<mpsc::Receiver<String>> {
        tokio::spawn(async move {
            let mut stdin = stdin;
            loop {
                match stdin_rx.recv().await {
                    Some(message) => {
                        if let Err(e) = Self::write_message(&mut stdin, message).await {
                            error!("Failed to write to stdin: {}", e);
                            break stdin_rx;
                        }
                    },
                    None => {
                        debug!("stdin channel closed");
                        break stdin_rx;
                    },
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
    ) -> JoinHandle<mpsc::Sender<SdkMessage>> {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();

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
                            match serde_json::from_str::<SdkMessage>(trimmed) {
                                Ok(message) => {
                                    trace!("Received message: {:?}", message);
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

    /// Check if process exited early and get stderr if available
    async fn check_early_exit(&mut self) -> Option<String> {
        // Check if process has exited
        if let Ok(Some(_status)) = self.child.try_wait() {
            // Process exited, wait a bit more for stderr to be captured
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Process has exited - we need to get the error message
            // Since we can't peek at the JoinHandle result, we'll just indicate
            // that the process exited and let the caller handle it
            Some("Process exited early".to_string())
        } else {
            None
        }
    }

    /// Close the process and recover the channels
    pub async fn close(mut self) -> Result<RecoveredChannels, CloseError> {
        // Kill the child process
        self.child.kill().await.context(KillFailedSnafu)?;

        // Recover channels and get last stderr
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
        let last_stderr = self.stderr_handle.await.unwrap_or_else(|_| String::new());

        // Log last stderr if present
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
