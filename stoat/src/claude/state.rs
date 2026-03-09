use crate::claude::{
    history::{self, ConversationMeta, ConversationStore},
    provider::{ClaudeProvider, ClaudeSessionConfig},
};
use gpui::{Context, EventEmitter};
use serde::{Deserialize, Serialize};
use smol::channel;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use stoat_agent_claude_code::{MessageContent, PermissionMode, SdkMessage};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatMessage {
    User {
        text: String,
        session_id: String,
    },
    Assistant {
        blocks: Vec<AssistantBlock>,
        session_id: String,
    },
    System {
        text: String,
        model: Option<String>,
    },
    Error {
        text: String,
    },
    Result {
        duration_ms: u64,
        cost_usd: f64,
        num_turns: u32,
        is_error: bool,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "block_type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text { text: String },
    ToolUse { name: String, input_summary: String },
    Thinking { text: String },
    RedactedThinking,
    ServerToolUse { name: String },
    Unknown,
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
    pub model: Option<String>,
    pub primary_session_id: Option<String>,
    pub input_history: VecDeque<String>,
    pub history_index: Option<usize>,
    pub conversation_id: String,
    pub conversation_title: String,
    pub conversation_store: ConversationStore,
    stdin_tx: Option<channel::Sender<(String, PermissionMode)>>,
    initialized: bool,
}

const INPUT_HISTORY_MAX: usize = 50;

impl ClaudeState {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            messages: Vec::new(),
            status: ClaudeStatus::Connecting,
            permission_mode: PermissionMode::Default,
            model: None,
            primary_session_id: None,
            input_history: VecDeque::new(),
            history_index: None,
            conversation_id: history::new_conversation_id(),
            conversation_title: String::new(),
            conversation_store: ConversationStore::new(ConversationStore::default_dir()),
            stdin_tx: None,
            initialized: false,
        }
    }

    pub fn start(
        &mut self,
        workdir: String,
        provider: Arc<dyn ClaudeProvider>,
        session_slug: Option<String>,
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
            session_slug,
        };

        cx.spawn(async move |this, cx| {
            let session = match provider.create_session(config).await {
                Ok(s) => s,
                Err(e) => {
                    this.update(cx, |state, cx| {
                        state.status = ClaudeStatus::Idle;
                        state.messages.push(ChatMessage::Error {
                            text: format!("Failed to start: {e}"),
                        });
                        cx.emit(ClaudeStateEvent::Updated);
                        cx.notify();
                    })
                    .ok();
                    return;
                },
            };

            this.update(cx, |state, cx| {
                state.stdin_tx = Some(session.stdin_tx);
                state.status = ClaudeStatus::Idle;
                cx.emit(ClaudeStateEvent::Updated);
                cx.notify();
            })
            .ok();

            let stdout_rx = session.stdout_rx;

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
        match msg {
            SdkMessage::Assistant {
                message,
                session_id,
            } => {
                let blocks: Vec<AssistantBlock> = message
                    .content
                    .into_iter()
                    .filter_map(|c| match c {
                        MessageContent::Text { text } if !text.is_empty() => {
                            Some(AssistantBlock::Text { text })
                        },
                        MessageContent::ToolUse { name, input, .. } => {
                            Some(AssistantBlock::ToolUse {
                                input_summary: summarize_tool_input(&name, &input),
                                name,
                            })
                        },
                        MessageContent::Thinking { thinking } if !thinking.is_empty() => {
                            Some(AssistantBlock::Thinking { text: thinking })
                        },
                        MessageContent::RedactedThinking => Some(AssistantBlock::RedactedThinking),
                        MessageContent::ServerToolUse { name, .. } => {
                            Some(AssistantBlock::ServerToolUse { name })
                        },
                        MessageContent::Unknown { .. } => Some(AssistantBlock::Unknown),
                        _ => None,
                    })
                    .collect();
                if !blocks.is_empty() {
                    state
                        .messages
                        .push(ChatMessage::Assistant { blocks, session_id });
                }
            },
            SdkMessage::System {
                model, session_id, ..
            } => {
                if !state.initialized {
                    state.initialized = true;
                    state.primary_session_id = Some(session_id);
                    state.model = Some(model.clone());
                    state.messages.push(ChatMessage::System {
                        text: "Session started".into(),
                        model: Some(model),
                    });
                }
            },
            SdkMessage::Result {
                duration_ms,
                total_cost_usd,
                num_turns,
                is_error,
                ..
            } => {
                state.messages.push(ChatMessage::Result {
                    duration_ms,
                    cost_usd: total_cost_usd,
                    num_turns,
                    is_error,
                });
                state.auto_save();
            },
            SdkMessage::User { .. } => {},
        }
    }

    pub fn send_message(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        // Add to input history (dedup: remove previous occurrence)
        if let Some(pos) = self.input_history.iter().position(|h| h == &text) {
            self.input_history.remove(pos);
        }
        self.input_history.push_back(text.clone());
        if self.input_history.len() > INPUT_HISTORY_MAX {
            self.input_history.pop_front();
        }
        self.history_index = None;

        if self.conversation_title.is_empty() {
            self.conversation_title = history::auto_title(&text);
        }

        let session_id = self.primary_session_id.clone().unwrap_or_default();
        self.messages.push(ChatMessage::User {
            text: text.clone(),
            session_id,
        });
        self.status = ClaudeStatus::Responding;

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

    pub fn history_up(&mut self) -> Option<String> {
        if self.input_history.is_empty() {
            return None;
        }
        let idx = match self.history_index {
            Some(0) => return self.input_history.front().cloned(),
            Some(i) => i - 1,
            None => self.input_history.len() - 1,
        };
        self.history_index = Some(idx);
        self.input_history.get(idx).cloned()
    }

    pub fn history_down(&mut self) -> Option<String> {
        let idx = match self.history_index {
            Some(i) => i + 1,
            None => return None,
        };
        if idx >= self.input_history.len() {
            self.history_index = None;
            return None;
        }
        self.history_index = Some(idx);
        self.input_history.get(idx).cloned()
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
            PermissionMode::Default => "read-only",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Plan => "plan-only",
            PermissionMode::BypassPermissions => "full-access",
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.status != ClaudeStatus::Responding {
            return;
        }
        self.stdin_tx = None;
        self.status = ClaudeStatus::Idle;
        cx.emit(ClaudeStateEvent::Updated);
        cx.notify();
    }

    pub fn request_close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ClaudeStateEvent::CloseRequested);
    }

    fn auto_save(&self) {
        if self.messages.is_empty() {
            return;
        }
        let now = chrono::Utc::now();
        let meta = ConversationMeta {
            id: self.conversation_id.clone(),
            session_id: self.primary_session_id.clone().unwrap_or_default(),
            title: self.conversation_title.clone(),
            created_at: now,
            updated_at: now,
            message_count: self.messages.len(),
            forked_from: None,
        };
        if let Err(e) = self.conversation_store.save(&self.messages, &meta) {
            tracing::warn!("Failed to auto-save conversation: {e}");
        }
    }

    pub fn fork_at(&mut self, message_index: usize) -> Option<String> {
        self.auto_save();
        match self
            .conversation_store
            .fork(&self.conversation_id, message_index)
        {
            Ok(new_meta) => Some(new_meta.id),
            Err(e) => {
                tracing::warn!("Failed to fork conversation: {e}");
                None
            },
        }
    }

    pub fn load_conversation(&mut self, id: &str, cx: &mut Context<Self>) {
        match self.conversation_store.load(id) {
            Ok((meta, messages)) => {
                self.messages = messages;
                self.conversation_id = meta.id;
                self.conversation_title = meta.title;
                cx.emit(ClaudeStateEvent::Updated);
                cx.notify();
            },
            Err(e) => {
                self.messages.push(ChatMessage::Error {
                    text: format!("Failed to load conversation: {e}"),
                });
                cx.emit(ClaudeStateEvent::Updated);
                cx.notify();
            },
        }
    }

    pub fn new_conversation(&mut self, cx: &mut Context<Self>) {
        self.auto_save();
        self.messages.clear();
        self.conversation_id = history::new_conversation_id();
        self.conversation_title.clear();
        self.initialized = false;
        cx.emit(ClaudeStateEvent::Updated);
        cx.notify();
    }
}

fn summarize_tool_input(name: &str, input: &HashMap<String, serde_json::Value>) -> String {
    let try_keys = ["command", "file_path", "path", "query", "pattern"];
    for key in &try_keys {
        if let Some(val) = input.get(*key) {
            if let Some(s) = val.as_str() {
                return s.to_string();
            }
        }
    }
    name.to_string()
}

impl EventEmitter<ClaudeStateEvent> for ClaudeState {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_history(items: &[&str]) -> ClaudeState {
        let mut state = ClaudeState {
            messages: Vec::new(),
            status: ClaudeStatus::Idle,
            permission_mode: PermissionMode::Default,
            model: None,
            primary_session_id: None,
            input_history: items.iter().map(|s| s.to_string()).collect(),
            history_index: None,
            conversation_id: "test".into(),
            conversation_title: String::new(),
            conversation_store: ConversationStore::new(
                tempfile::tempdir().expect("tempdir").into_path(),
            ),
            stdin_tx: None,
            initialized: false,
        };
        state
    }

    #[test]
    fn history_navigation() {
        let mut s = make_history(&["first", "second", "third"]);

        assert_eq!(s.history_up(), Some("third".into()));
        assert_eq!(s.history_up(), Some("second".into()));
        assert_eq!(s.history_up(), Some("first".into()));
        assert_eq!(s.history_up(), Some("first".into()));

        assert_eq!(s.history_down(), Some("second".into()));
        assert_eq!(s.history_down(), Some("third".into()));
        assert_eq!(s.history_down(), None);
    }

    #[test]
    fn history_empty() {
        let mut s = make_history(&[]);
        assert_eq!(s.history_up(), None);
        assert_eq!(s.history_down(), None);
    }

    #[test]
    fn history_dedup() {
        let mut s = make_history(&["first", "second"]);
        s.input_history.push_back("first".into());
        if let Some(pos) = s.input_history.iter().position(|h| h == "first") {
            if pos < s.input_history.len() - 1 {
                s.input_history.remove(pos);
            }
        }
        let items: Vec<_> = s.input_history.iter().cloned().collect();
        assert_eq!(items, vec!["second", "first"]);
    }

    #[test]
    fn summarize_tool_input_command() {
        let mut input = HashMap::new();
        input.insert(
            "command".to_string(),
            serde_json::Value::String("ls -la".to_string()),
        );
        assert_eq!(summarize_tool_input("Bash", &input), "ls -la");
    }

    #[test]
    fn summarize_tool_input_path() {
        let mut input = HashMap::new();
        input.insert(
            "file_path".to_string(),
            serde_json::Value::String("/foo/bar.rs".to_string()),
        );
        assert_eq!(summarize_tool_input("Read", &input), "/foo/bar.rs");
    }

    #[test]
    fn summarize_tool_input_fallback() {
        let input = HashMap::new();
        assert_eq!(summarize_tool_input("Unknown", &input), "Unknown");
    }

    #[test]
    fn auto_title_from_message() {
        assert_eq!(
            history::auto_title("help me fix this bug"),
            "help me fix this bug"
        );
        let long = "a".repeat(100);
        assert_eq!(history::auto_title(&long).len(), 50);
    }
}
