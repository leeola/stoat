use async_trait::async_trait;
use slotmap::{new_key_type, SlotMap};
use std::{io, sync::Arc};

/// Host-provided callback for interactive tool-permission prompts.
///
/// When a [`ClaudeCodeSession`] is built with a registered callback, the
/// underlying wrapper asks the `claude` CLI to route permission prompts
/// over the control protocol (`--permission-prompt-tool-name stdio`).
/// Each incoming `can_use_tool` control request is forwarded here; the
/// returned [`PermissionResult`] becomes the control response.
///
/// JSON payloads are passed as `&str` so this trait (and the `stoat`
/// crate) stays free of a `serde_json` dependency. Callbacks that need
/// structured access should parse the strings themselves.
#[async_trait]
pub trait PermissionCallback: Send + Sync {
    async fn can_use_tool(
        &self,
        tool_name: &str,
        input_json: &str,
        context: ToolPermissionContext<'_>,
    ) -> PermissionResult;
}

/// Context passed to a [`PermissionCallback::can_use_tool`] invocation.
/// Mirrors the fields in the Python SDK's `ToolPermissionContext`.
/// `suggestions_json` is the raw `permission_suggestions` array as a
/// JSON string, or `None` when absent.
#[derive(Debug, Clone, Copy)]
pub struct ToolPermissionContext<'a> {
    pub suggestions_json: Option<&'a str>,
    pub tool_use_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub blocked_path: Option<&'a str>,
}

/// Outcome of a [`PermissionCallback::can_use_tool`] invocation.
#[derive(Debug, Clone)]
pub enum PermissionResult {
    /// Permit the tool to execute. `updated_input_json` optionally
    /// replaces (as a JSON object string) the input the CLI proposed;
    /// `updated_permissions_json` optionally installs new rules for
    /// the rest of the session (as a JSON array string).
    Allow {
        updated_input_json: Option<String>,
        updated_permissions_json: Option<String>,
    },
    /// Block the tool invocation. `message` is surfaced to Claude; if
    /// `interrupt` is true, the agent run is aborted entirely.
    Deny { message: String, interrupt: bool },
}

impl PermissionResult {
    pub fn allow() -> Self {
        PermissionResult::Allow {
            updated_input_json: None,
            updated_permissions_json: None,
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        PermissionResult::Deny {
            message: message.into(),
            interrupt: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AgentMessage {
    Init {
        session_id: String,
        model: String,
        tools: Vec<String>,
    },
    Text {
        text: String,
    },
    /// Incremental text delta surfaced when the session is configured
    /// with `--include-partial-messages`. A normal `Text` message always
    /// follows once the stream block completes, so consumers can choose
    /// to display deltas live or ignore them and wait for the finalized
    /// `Text` message.
    PartialText {
        text: String,
    },
    Thinking {
        text: String,
        signature: String,
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
    ServerToolUse {
        id: String,
        name: String,
        input: String,
    },
    ServerToolResult {
        id: String,
        content: String,
    },
    Result {
        cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
    },
    Error {
        message: String,
    },
    /// Content block with an unrecognized `type` tag. Carries the raw
    /// JSON so consumers can surface or persist it without this crate
    /// needing to understand the schema.
    Unknown {
        raw: String,
    },
}

/// Per-session I/O handle for an active Claude Code conversation.
#[async_trait]
pub trait ClaudeCodeSession: Send + Sync {
    async fn send(&self, content: &str) -> io::Result<()>;
    async fn recv(&self) -> Option<AgentMessage>;
    fn is_alive(&self) -> bool;
    async fn shutdown(&self) -> io::Result<()>;
}

/// Session manager that creates new [`ClaudeCodeSession`] instances.
///
/// Production uses a launcher that spawns Claude CLI subprocesses.
/// Tests use a fake that returns pre-configured sessions.
#[async_trait]
pub trait ClaudeCodeHost: Send + Sync {
    async fn new_session(&self) -> io::Result<Box<dyn ClaudeCodeSession>>;
}

new_key_type! {
    pub struct ClaudeSessionId;
}

pub enum ClaudeNotification {
    CreateRequested {
        session_id: ClaudeSessionId,
    },
    SessionReady {
        session_id: ClaudeSessionId,
        session: Box<dyn ClaudeCodeSession>,
    },
    SessionError {
        session_id: ClaudeSessionId,
        error: String,
    },
    Message {
        session_id: ClaudeSessionId,
        message: AgentMessage,
    },
}

/// Owns zero or more active Claude Code sessions.
pub struct ClaudeCodeSessions {
    sessions: SlotMap<ClaudeSessionId, Option<Arc<dyn ClaudeCodeSession>>>,
}

impl ClaudeCodeSessions {
    pub fn new() -> Self {
        Self {
            sessions: SlotMap::with_key(),
        }
    }

    pub fn reserve_slot(&mut self) -> ClaudeSessionId {
        self.sessions.insert(None)
    }

    pub fn fill_slot(&mut self, id: ClaudeSessionId, session: Arc<dyn ClaudeCodeSession>) {
        if let Some(slot) = self.sessions.get_mut(id) {
            *slot = Some(session);
        }
    }

    pub fn get(&self, id: ClaudeSessionId) -> Option<&Arc<dyn ClaudeCodeSession>> {
        self.sessions.get(id).and_then(|s| s.as_ref())
    }

    pub fn remove(&mut self, id: ClaudeSessionId) -> Option<Arc<dyn ClaudeCodeSession>> {
        self.sessions.remove(id).flatten()
    }

    pub fn ids(&self) -> impl Iterator<Item = ClaudeSessionId> + '_ {
        self.sessions.keys()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

impl Default for ClaudeCodeSessions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeClaudeCode;

    #[test]
    fn reserve_fill_get_remove() {
        let mut sessions = ClaudeCodeSessions::new();

        let id_a = sessions.reserve_slot();
        let id_b = sessions.reserve_slot();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.get(id_a).is_none(), "unfilled slot returns None");

        let fake_a: Arc<dyn ClaudeCodeSession> = Arc::new(FakeClaudeCode::new());
        let fake_b: Arc<dyn ClaudeCodeSession> = Arc::new(FakeClaudeCode::new());
        sessions.fill_slot(id_a, fake_a);
        sessions.fill_slot(id_b, fake_b);

        assert!(sessions.get(id_a).is_some());
        assert!(sessions.get(id_b).is_some());

        let removed = sessions.remove(id_a).expect("session present");
        assert!(removed.is_alive());
        assert!(sessions.get(id_a).is_none());
        assert_eq!(sessions.len(), 1);

        let remaining: Vec<_> = sessions.ids().collect();
        assert_eq!(remaining, vec![id_b]);
    }
}
