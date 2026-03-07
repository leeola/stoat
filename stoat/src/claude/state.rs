use crate::claude::provider::{ClaudeProvider, ClaudeSessionConfig};
use gpui::{Context, EventEmitter};
use smol::channel;
use std::sync::Arc;
use stoat_agent_claude_code::{PermissionMode, SdkMessage};

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
    pub status: ClaudeStatus,
    pub permission_mode: PermissionMode,
    stdin_tx: Option<channel::Sender<(String, PermissionMode)>>,
    initialized: bool,
}

impl ClaudeState {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            messages: Vec::new(),
            status: ClaudeStatus::Connecting,
            permission_mode: PermissionMode::Default,
            stdin_tx: None,
            initialized: false,
        }
    }

    pub fn start(
        &mut self,
        workdir: String,
        provider: Arc<dyn ClaudeProvider>,
        cx: &mut Context<Self>,
    ) {
        let initial_mode = self.permission_mode.clone();

        let log_dir = dirs::data_local_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("stoat/logs");

        let config = ClaudeSessionConfig {
            workdir,
            permission_mode: initial_mode,
            log_dir: Some(log_dir),
        };

        let session = match provider.create_session(config) {
            Ok(s) => s,
            Err(e) => {
                self.status = ClaudeStatus::Idle;
                self.messages
                    .push(ChatMessage::Error(format!("Failed to start: {e}")));
                cx.emit(ClaudeStateEvent::Updated);
                cx.notify();
                return;
            },
        };

        self.stdin_tx = Some(session.stdin_tx);
        let stdout_rx = session.stdout_rx;

        cx.spawn(async move |this, cx| {
            this.update(cx, |state, cx| {
                state.status = ClaudeStatus::Idle;
                cx.emit(ClaudeStateEvent::Updated);
                cx.notify();
            })
            .ok();

            while let Ok(msg) = stdout_rx.recv().await {
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
            }

            this.update(cx, |state, cx| {
                state.status = ClaudeStatus::Idle;
                state.stdin_tx = None;
                cx.emit(ClaudeStateEvent::Updated);
                cx.notify();
            })
            .ok();
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

    pub fn send_message(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
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
