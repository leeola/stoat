use gpui::{Context, EventEmitter};
use smol::channel;
use stoat_agent_claude_code::{ClaudeCode, SdkMessage};

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
    stdin_tx: Option<channel::Sender<String>>,
}

impl ClaudeState {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            messages: Vec::new(),
            input_text: String::new(),
            status: ClaudeStatus::Connecting,
            stdin_tx: None,
        }
    }

    pub fn start(&mut self, workdir: String, cx: &mut Context<Self>) {
        let (stdin_tx, stdin_rx) = channel::bounded::<String>(32);
        self.stdin_tx = Some(stdin_tx);

        let log_dir = dirs::data_local_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("stoat/logs");

        cx.spawn(async move |this, cx| {
            let claude = ClaudeCode::builder()
                .cwd(&workdir)
                .log_dir(&log_dir)
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

            let log_path_msg = claude.log_file().map(|p| format!("Log: {}", p.display()));

            this.update(cx, |state, cx| {
                state.status = ClaudeStatus::Idle;
                if let Some(msg) = log_path_msg {
                    state.messages.push(ChatMessage::System(msg));
                }
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
                    Ok(text) => {
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
                state
                    .messages
                    .push(ChatMessage::System("Session started".into()));
            },
            SdkMessage::Result { result, .. } => {
                if let Some(text) = result {
                    if !text.is_empty() {
                        state.messages.push(ChatMessage::Assistant(text.clone()));
                    }
                }
            },
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
            cx.spawn(async move |_, _| {
                let _ = tx.send(text).await;
            })
            .detach();
        }

        cx.emit(ClaudeStateEvent::Updated);
        cx.notify();
    }

    pub fn request_close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ClaudeStateEvent::CloseRequested);
    }
}

impl EventEmitter<ClaudeStateEvent> for ClaudeState {}
