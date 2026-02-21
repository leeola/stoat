use crate::messages::{PermissionMode, SdkMessage};
use async_channel::{Receiver, Sender};
use snafu::Snafu;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    thread::JoinHandle,
    time::Duration,
};
use tracing::{debug, error, trace};

pub struct ProcessChannels {
    pub stdin_rx: Receiver<String>,
    pub stdout_tx: Sender<SdkMessage>,
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
    stdin_handle: JoinHandle<Receiver<String>>,
    stdout_handle: JoinHandle<Sender<SdkMessage>>,
    stderr_handle: JoinHandle<String>,
}

pub struct RecoveredChannels {
    pub stdin_rx: Receiver<String>,
    pub stdout_tx: Sender<SdkMessage>,
    pub last_stderr: String,
}

pub struct ProcessBuilder {
    stdin_rx: Receiver<String>,
    stdout_tx: Sender<SdkMessage>,

    session_id: Option<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    cwd: Option<String>,
    allowed_tools: Option<Vec<String>>,
    permission_mode: Option<PermissionMode>,
}

impl ProcessBuilder {
    pub fn new(stdin_rx: Receiver<String>, stdout_tx: Sender<SdkMessage>) -> Self {
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

    pub async fn build(self) -> Result<Process, (ProcessChannels, BuildError)> {
        if self.session_id.is_none() {
            let mut builder = self;
            builder.session_id = Some(uuid::Uuid::new_v4().to_string());
            match builder.new_session().await {
                Ok(process) => Ok(process),
                Err((channels, e)) => Err((channels, BuildError::NewSession { source: e })),
            }
        } else {
            match self.resume_session().await {
                Ok(process) => Ok(process),
                Err((channels, e)) => Err((channels, BuildError::ResumeSession { source: e })),
            }
        }
    }

    pub async fn new_session(self) -> Result<Process, (ProcessChannels, NewSessionError)> {
        let session_id = match &self.session_id {
            Some(id) => id.clone(),
            None => {
                return Err((
                    ProcessChannels {
                        stdin_rx: self.stdin_rx,
                        stdout_tx: self.stdout_tx,
                    },
                    NewSessionError::SessionIdMissing,
                ));
            },
        };

        let args = self.build_args(false);
        let mut child = match self.spawn_child(args) {
            Ok(child) => child,
            Err(e) => {
                return Err((
                    ProcessChannels {
                        stdin_rx: self.stdin_rx,
                        stdout_tx: self.stdout_tx,
                    },
                    NewSessionError::SpawnFailed { source: e },
                ));
            },
        };

        let stdin_rx = self.stdin_rx;
        let stdout_tx = self.stdout_tx;

        let (stdin, stdout, stderr) = match Self::extract_stdio(&mut child) {
            Ok(stdio) => stdio,
            Err(e) => {
                let _ = child.kill();
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

        let mut process = Process {
            child,
            stdin_handle,
            stdout_handle,
            stderr_handle,
        };

        async_io::Timer::after(Duration::from_millis(100)).await;

        if process.check_early_exit().is_some() {
            match process.close() {
                Ok(recovered) => {
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
                    let (_, dummy_stdin_rx) = async_channel::bounded(1);
                    let (dummy_stdout_tx, _) = async_channel::bounded(1);
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

    pub async fn resume_session(self) -> Result<Process, (ProcessChannels, ResumeSessionError)> {
        let session_id = match &self.session_id {
            Some(id) => id.clone(),
            None => {
                return Err((
                    ProcessChannels {
                        stdin_rx: self.stdin_rx,
                        stdout_tx: self.stdout_tx,
                    },
                    ResumeSessionError::ResumeSessionIdMissing,
                ));
            },
        };

        let args = self.build_args(true);
        let mut child = match self.spawn_child(args) {
            Ok(child) => child,
            Err(e) => {
                return Err((
                    ProcessChannels {
                        stdin_rx: self.stdin_rx,
                        stdout_tx: self.stdout_tx,
                    },
                    ResumeSessionError::ResumeSpawnFailed { source: e },
                ));
            },
        };

        let stdin_rx = self.stdin_rx;
        let stdout_tx = self.stdout_tx;

        let (stdin, stdout, stderr) = match Self::extract_stdio(&mut child) {
            Ok(stdio) => stdio,
            Err(_) => {
                let _ = child.kill();
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

        let mut process = Process {
            child,
            stdin_handle,
            stdout_handle,
            stderr_handle,
        };

        async_io::Timer::after(Duration::from_millis(100)).await;

        if process.check_early_exit().is_some() {
            match process.close() {
                Ok(recovered) => {
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
                        let (_, dummy_stdin_rx) = async_channel::bounded(1);
                        let (dummy_stdout_tx, _) = async_channel::bounded(1);
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
                    let (_, dummy_stdin_rx) = async_channel::bounded(1);
                    let (dummy_stdout_tx, _) = async_channel::bounded(1);
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

    fn build_args(&self, use_resume: bool) -> Vec<String> {
        let mut args = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];

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

    fn spawn_child(&self, args: Vec<String>) -> Result<Child, std::io::Error> {
        let mut cmd = Command::new("claude");
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        debug!("Spawning Claude Code process with args: {:?}", args);
        cmd.spawn()
    }

    fn extract_stdio(
        child: &mut Child,
    ) -> Result<(ChildStdin, ChildStdout, ChildStderr), NewSessionError> {
        let stdin = child.stdin.take().ok_or(NewSessionError::StdioFailed)?;
        let stdout = child.stdout.take().ok_or(NewSessionError::StdioFailed)?;
        let stderr = child.stderr.take().ok_or(NewSessionError::StdioFailed)?;
        Ok((stdin, stdout, stderr))
    }

    fn setup_handlers(
        stdin: ChildStdin,
        stdout: ChildStdout,
        stderr: ChildStderr,
        stdin_rx: Receiver<String>,
        stdout_tx: Sender<SdkMessage>,
    ) -> (
        JoinHandle<Receiver<String>>,
        JoinHandle<Sender<SdkMessage>>,
        JoinHandle<String>,
    ) {
        let stdin_handle = Self::spawn_stdin_handler(stdin, stdin_rx);
        let stdout_handle = Self::spawn_stdout_handler(stdout, stdout_tx);
        let stderr_handle = Self::spawn_stderr_handler(stderr);

        (stdin_handle, stdout_handle, stderr_handle)
    }

    fn spawn_stdin_handler(
        mut stdin: ChildStdin,
        stdin_rx: Receiver<String>,
    ) -> JoinHandle<Receiver<String>> {
        std::thread::spawn(move || {
            while let Ok(message) = stdin_rx.recv_blocking() {
                if write!(stdin, "{}\n", message).is_err() {
                    break;
                }
                if stdin.flush().is_err() {
                    break;
                }
            }
            stdin_rx
        })
    }

    fn spawn_stdout_handler(
        stdout: ChildStdout,
        stdout_tx: Sender<SdkMessage>,
    ) -> JoinHandle<Sender<SdkMessage>> {
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            match serde_json::from_str::<SdkMessage>(trimmed) {
                                Ok(message) => {
                                    trace!("Received message: {:?}", message);
                                    if stdout_tx.send_blocking(message).is_err() {
                                        error!("Failed to send message to channel");
                                        break;
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
                        break;
                    },
                }
            }
            stdout_tx
        })
    }

    fn spawn_stderr_handler(stderr: ChildStderr) -> JoinHandle<String> {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            let mut last_error = String::new();
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            error!("Claude Code stderr: {}", trimmed);
                            last_error = trimmed.to_string();
                        }
                    },
                    Err(e) => {
                        error!("Failed to read from stderr: {}", e);
                        break;
                    },
                }
            }
            last_error
        })
    }
}

impl Process {
    pub fn builder(stdin_rx: Receiver<String>, stdout_tx: Sender<SdkMessage>) -> ProcessBuilder {
        ProcessBuilder::new(stdin_rx, stdout_tx)
    }

    fn check_early_exit(&mut self) -> Option<String> {
        if let Ok(Some(_status)) = self.child.try_wait() {
            std::thread::sleep(Duration::from_millis(200));
            Some("Process exited early".to_string())
        } else {
            None
        }
    }

    pub fn close(mut self) -> Result<RecoveredChannels, CloseError> {
        let _ = self.child.kill();
        let _ = self.child.wait();

        let stdin_rx = self
            .stdin_handle
            .join()
            .map_err(|_| CloseError::HandlerPanicked {
                task: "stdin".to_string(),
            })?;
        let stdout_tx = self
            .stdout_handle
            .join()
            .map_err(|_| CloseError::HandlerPanicked {
                task: "stdout".to_string(),
            })?;
        let last_stderr = self.stderr_handle.join().unwrap_or_default();

        if !last_stderr.is_empty() {
            debug!("Process closed with last stderr: {}", last_stderr);
        }

        Ok(RecoveredChannels {
            stdin_rx,
            stdout_tx,
            last_stderr,
        })
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}
