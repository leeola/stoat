use parking_lot::Mutex;
use smol::channel;
use std::{any::Any, collections::VecDeque, path::PathBuf, sync::Arc};
use stoat_agent_claude_code::{PermissionMode, SdkMessage};

pub struct ClaudeSessionConfig {
    pub workdir: String,
    pub permission_mode: PermissionMode,
    pub log_dir: Option<PathBuf>,
}

pub type StdinMessage = (String, PermissionMode);

pub trait ClaudeProvider: Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn create_session(&self, config: ClaudeSessionConfig) -> Result<ClaudeSession, anyhow::Error>;
}

pub struct ClaudeSession {
    pub stdin_tx: channel::Sender<StdinMessage>,
    pub stdout_rx: channel::Receiver<SdkMessage>,
}

// -- Real implementation --

pub struct RealClaudeProvider;

impl ClaudeProvider for RealClaudeProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn create_session(&self, config: ClaudeSessionConfig) -> Result<ClaudeSession, anyhow::Error> {
        let (stdin_tx, stdin_rx) = channel::bounded::<StdinMessage>(32);
        let (stdout_tx, stdout_rx) = channel::bounded::<SdkMessage>(256);

        let workdir = config.workdir;
        let log_dir = config.log_dir;
        let initial_mode = config.permission_mode;

        std::thread::spawn(move || {
            smol::block_on(async {
                use stoat_agent_claude_code::ClaudeCode;

                let mut builder = ClaudeCode::builder().cwd(&workdir);
                if let Some(ref dir) = log_dir {
                    builder = builder.log_dir(dir);
                }
                let claude = builder.permission_mode(initial_mode).build().await;

                let mut claude = match claude {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("Failed to start Claude Code: {e}");
                        return;
                    },
                };

                if let Some(p) = claude.log_file() {
                    tracing::info!("Claude Code log: {}", p.display());
                }

                loop {
                    match claude
                        .recv_any_message(std::time::Duration::from_millis(100))
                        .await
                    {
                        Ok(Some(msg)) => {
                            if stdout_tx.send(msg).await.is_err() {
                                break;
                            }
                        },
                        Ok(None) => {},
                        Err(_) => break,
                    }

                    match stdin_rx.try_recv() {
                        Ok((text, desired_mode)) => {
                            let current_mode = claude.permission_mode().cloned();
                            if current_mode.as_ref() != Some(&desired_mode) {
                                let _ = claude.switch_permission_mode(desired_mode).await;
                            }
                            if claude.send_message(&text).await.is_err() {
                                break;
                            }
                        },
                        Err(channel::TryRecvError::Empty) => {},
                        Err(channel::TryRecvError::Closed) => break,
                    }

                    if !claude.is_alive() {
                        break;
                    }
                }

                let _ = claude.shutdown();
            });
        });

        Ok(ClaudeSession {
            stdin_tx,
            stdout_rx,
        })
    }
}

// -- Fake implementation --

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
pub struct FakeClaudeProvider {
    state: Arc<Mutex<FakeClaudeState>>,
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
struct FakeClaudeState {
    replay_queue: VecDeque<SdkMessage>,
    sent_messages: Vec<String>,
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
impl FakeClaudeProvider {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeClaudeState {
                replay_queue: VecDeque::new(),
                sent_messages: Vec::new(),
            })),
        }
    }

    pub fn queue_message(&self, msg: SdkMessage) {
        self.state.lock().replay_queue.push_back(msg);
    }

    pub fn queue_response(&self, text: &str) {
        use stoat_agent_claude_code::AssistantMessage;
        self.queue_message(SdkMessage::Assistant {
            message: AssistantMessage::from_text(text),
            session_id: "fake".to_string(),
        });
    }

    pub fn queue_result(&self) {
        use stoat_agent_claude_code::ResultSubtype;
        self.queue_message(SdkMessage::Result {
            subtype: ResultSubtype::Success,
            duration_ms: 0,
            duration_api_ms: 0,
            is_error: false,
            num_turns: 1,
            result: None,
            session_id: "fake".to_string(),
            total_cost_usd: 0.0,
        });
    }

    pub fn sent_messages(&self) -> Vec<String> {
        self.state.lock().sent_messages.clone()
    }
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
impl ClaudeProvider for FakeClaudeProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn create_session(&self, _config: ClaudeSessionConfig) -> Result<ClaudeSession, anyhow::Error> {
        let (stdin_tx, stdin_rx) = channel::bounded::<StdinMessage>(32);
        let (stdout_tx, stdout_rx) = channel::bounded::<SdkMessage>(256);

        let state = self.state.clone();

        // Drain replay queue into stdout channel, record stdin into sent_messages
        std::thread::spawn(move || {
            smol::block_on(async {
                // Drain queued messages
                loop {
                    let msg = {
                        let mut s = state.lock();
                        s.replay_queue.pop_front()
                    };
                    match msg {
                        Some(m) => {
                            if stdout_tx.send(m).await.is_err() {
                                return;
                            }
                        },
                        None => break,
                    }
                }

                // Then listen for stdin and record
                while let Ok((text, _mode)) = stdin_rx.recv().await {
                    state.lock().sent_messages.push(text);
                    // Drain any newly queued messages
                    loop {
                        let msg = {
                            let mut s = state.lock();
                            s.replay_queue.pop_front()
                        };
                        match msg {
                            Some(m) => {
                                if stdout_tx.send(m).await.is_err() {
                                    return;
                                }
                            },
                            None => break,
                        }
                    }
                }
            });
        });

        Ok(ClaudeSession {
            stdin_tx,
            stdout_rx,
        })
    }
}
