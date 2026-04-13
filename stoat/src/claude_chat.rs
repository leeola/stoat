use crate::{buffer::BufferId, editor_state::EditorId, host::ClaudeSessionId};
use std::time::Instant;

pub struct ClaudeChatState {
    pub session_id: ClaudeSessionId,
    pub input_editor_id: EditorId,
    pub input_buffer_id: BufferId,
    pub messages: Vec<ChatMessage>,
    pub streaming_text: Option<String>,
    pub scroll_offset: usize,
    /// Messages the user submitted before the session host was ready.
    /// Drained and sent when the session becomes available.
    pub pending_sends: Vec<String>,
    /// Set when the user submits a message; cleared when the turn
    /// completes (Result) or errors. Drives the activity throbber.
    pub active_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub enum ChatMessageContent {
    Text(String),
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        content: String,
    },
    Error(String),
    TurnComplete {
        cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
    },
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: ChatMessageContent,
}
