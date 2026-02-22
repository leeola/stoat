use gpui::{Context, EventEmitter};
use smol::channel;
use stoat_agent_claude_code::{ClaudeCode, PermissionMode, SdkMessage};

pub enum ChatMessage {
    User(String),
    Assistant(String),
    System(String),
    Error(String),
}

#[derive(Clone, Copy, PartialEq)]
pub enum ClaudeStatus {
    Idle,
    Connecting,
    Responding,
}

pub enum ClaudeStateEvent {
    Updated,
    CloseRequested,
}

pub struct ClaudeState {
    pub messages: Vec<ChatMessage>,
    pub input_text: String,
    pub status: ClaudeStatus,
    pub permission_mode: PermissionMode,
    stdin_tx: Option<channel::Sender<(String, PermissionMode)>>,
    initialized: bool,
}

impl ClaudeState {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            messages: Vec::new(),
            input_text: String::new(),
            status: ClaudeStatus::Connecting,
            permission_mode: PermissionMode::Default,
            stdin_tx: None,
            initialized: false,
        }
    }

    pub fn start(&mut self, workdir: String, cx: &mut Context<Self>) {
        let (stdin_tx, stdin_rx) = channel::bounded::<(String, PermissionMode)>(32);
        self.stdin_tx = Some(stdin_tx);
        let initial_mode = self.permission_mode.clone();

        let log_dir = dirs::data_local_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("stoat/logs");

        cx.spawn(async move |this, cx| {
            let claude = ClaudeCode::builder()
                .cwd(&workdir)
                .log_dir(&log_dir)
                .permission_mode(initial_mode)
                .build()
                .await;

            let mut claude = match claude {
                Ok(c) => c,
                Err(e) => {
                    this.update(cx, |state, cx| {
                        state.status = ClaudeStatus::Idle;
                        state
                            .messages
                            .push(ChatMessage::Error(format!("Failed to start: {e}")));
                        cx.emit(ClaudeStateEvent::Updated);
                        cx.notify();
                    })
                    .ok();
                    return;
                },
            };

            if let Some(p) = claude.log_file() {
                tracing::info!("Claude Code log: {}", p.display());
            }

            this.update(cx, |state, cx| {
                state.status = ClaudeStatus::Idle;
                cx.emit(ClaudeStateEvent::Updated);
                cx.notify();
            })
            .ok();

            loop {
                match claude
                    .recv_any_message(std::time::Duration::from_millis(100))
                    .await
                {
                    Ok(Some(msg)) => {
                        let is_terminal = msg.is_terminal();
                        this.update(cx, |state, cx| {
                            Self::process_message(state, msg);
                            if is_terminal {
                                state.status = ClaudeStatus::Idle;
                            }
                            cx.emit(ClaudeStateEvent::Updated);
                            cx.notify();
                        })
                        .ok();
                    },
                    Ok(None) => {},
                    Err(_) => break,
                }

                match stdin_rx.try_recv() {
                    Ok((text, desired_mode)) => {
                        let current_mode = claude.permission_mode().cloned();
                        if current_mode.as_ref() != Some(&desired_mode) {
                            if let Err(e) = claude.switch_permission_mode(desired_mode).await {
                                this.update(cx, |state, cx| {
                                    state.messages.push(ChatMessage::Error(format!(
                                        "Failed to switch permission mode: {e}"
                                    )));
                                    cx.emit(ClaudeStateEvent::Updated);
                                    cx.notify();
                                })
                                .ok();
                            }
                        }
                        this.update(cx, |state, cx| {
                            state.status = ClaudeStatus::Responding;
                            cx.emit(ClaudeStateEvent::Updated);
                            cx.notify();
                        })
                        .ok();
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

            this.update(cx, |state, cx| {
                state.status = ClaudeStatus::Idle;
                state.stdin_tx = None;
                cx.emit(ClaudeStateEvent::Updated);
                cx.notify();
            })
            .ok();

            let _ = claude.shutdown();
        })
        .detach();
    }

    fn process_message(state: &mut Self, msg: SdkMessage) {
        match &msg {
            SdkMessage::Assistant { message, .. } => {
                let text = message.get_text_content();
                if !text.is_empty() {
                    state.messages.push(ChatMessage::Assistant(text));
                }
            },
            SdkMessage::System { .. } => {
                if !state.initialized {
                    state.initialized = true;
                    state
                        .messages
                        .push(ChatMessage::System("Session started".into()));
                }
            },
            SdkMessage::Result { .. } => {},
            SdkMessage::User { .. } => {},
        }
    }

    pub fn send_message(&mut self, cx: &mut Context<Self>) {
        let text = self.input_text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input_text.clear();
        self.messages.push(ChatMessage::User(text.clone()));

        if let Some(tx) = &self.stdin_tx {
            let tx = tx.clone();
            let mode = self.permission_mode.clone();
            cx.spawn(async move |_, _| {
                let _ = tx.send((text, mode)).await;
            })
            .detach();
        }

        cx.emit(ClaudeStateEvent::Updated);
        cx.notify();
    }

    pub fn cycle_permission_mode(&mut self, cx: &mut Context<Self>) {
        self.permission_mode = match self.permission_mode {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Default,
            PermissionMode::BypassPermissions => PermissionMode::Default,
        };
        cx.emit(ClaudeStateEvent::Updated);
        cx.notify();
    }

    pub fn permission_mode_label(&self) -> &'static str {
        match self.permission_mode {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Plan => "plan",
            PermissionMode::BypassPermissions => "bypass",
        }
    }

    pub fn request_close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ClaudeStateEvent::CloseRequested);
    }
}

impl EventEmitter<ClaudeStateEvent> for ClaudeState {}
