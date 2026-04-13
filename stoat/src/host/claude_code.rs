use async_trait::async_trait;
use slotmap::{new_key_type, SlotMap};
use std::{io, sync::Arc};

/// Host-provided callback for interactive tool-permission prompts.
///
/// When a [`ClaudeCodeHost`] is built with a registered callback, the
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

#[async_trait]
pub trait ClaudeCodeHost: Send + Sync {
    async fn send(&self, content: &str) -> io::Result<()>;
    async fn recv(&self) -> Option<AgentMessage>;
    fn is_alive(&self) -> bool;
    async fn shutdown(&self) -> io::Result<()>;
}

/// Spawns new [`ClaudeCodeHost`] instances on demand. Implemented outside of
/// stoat (by the `stoat_agent_claude_code` crate) so stoat can create sessions
/// without importing the concrete process-backed type.
#[async_trait]
pub trait ClaudeCodeFactory: Send + Sync {
    async fn create(&self) -> io::Result<Arc<dyn ClaudeCodeHost>>;
}

new_key_type! {
    pub struct ClaudeSessionId;
}

/// Owns the lifecycle of zero or more active Claude Code sessions.
///
/// A `ClaudeCodeSessions` with no factory cannot spawn new sessions;
/// [`Self::create_session`] returns an error until [`Self::set_factory`] is
/// called. Tests and bare `Stoat` instances start without a factory.
pub struct ClaudeCodeSessions {
    factory: Option<Arc<dyn ClaudeCodeFactory>>,
    sessions: SlotMap<ClaudeSessionId, Arc<dyn ClaudeCodeHost>>,
}

impl ClaudeCodeSessions {
    pub fn new() -> Self {
        Self {
            factory: None,
            sessions: SlotMap::with_key(),
        }
    }

    pub fn set_factory(&mut self, factory: Arc<dyn ClaudeCodeFactory>) {
        self.factory = Some(factory);
    }

    pub fn has_factory(&self) -> bool {
        self.factory.is_some()
    }

    pub async fn create_session(&mut self) -> io::Result<ClaudeSessionId> {
        let factory = self
            .factory
            .as_ref()
            .ok_or_else(|| io::Error::other("no claude code factory registered"))?;
        let host = factory.create().await?;
        Ok(self.sessions.insert(host))
    }

    pub fn get(&self, id: ClaudeSessionId) -> Option<&Arc<dyn ClaudeCodeHost>> {
        self.sessions.get(id)
    }

    pub fn remove(&mut self, id: ClaudeSessionId) -> Option<Arc<dyn ClaudeCodeHost>> {
        self.sessions.remove(id)
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

    pub fn insert_host(&mut self, host: Arc<dyn ClaudeCodeHost>) -> ClaudeSessionId {
        self.sessions.insert(host)
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

    struct FakeFactory;

    #[async_trait]
    impl ClaudeCodeFactory for FakeFactory {
        async fn create(&self) -> io::Result<Arc<dyn ClaudeCodeHost>> {
            Ok(Arc::new(FakeClaudeCode::new()))
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn create_without_factory_errors() {
        rt().block_on(async {
            let mut sessions = ClaudeCodeSessions::new();
            let err = sessions.create_session().await.unwrap_err();
            assert_eq!(err.to_string(), "no claude code factory registered");
            assert!(sessions.is_empty());
        });
    }

    #[test]
    fn create_get_remove_roundtrip() {
        rt().block_on(async {
            let mut sessions = ClaudeCodeSessions::new();
            sessions.set_factory(Arc::new(FakeFactory));

            let id_a = sessions.create_session().await.unwrap();
            let id_b = sessions.create_session().await.unwrap();
            assert_eq!(sessions.len(), 2);
            assert_ne!(id_a, id_b);

            assert!(sessions.get(id_a).is_some());
            assert!(sessions.get(id_b).is_some());

            let removed = sessions.remove(id_a).expect("session present");
            assert!(removed.is_alive());
            assert!(sessions.get(id_a).is_none());
            assert_eq!(sessions.len(), 1);

            let remaining: Vec<_> = sessions.ids().collect();
            assert_eq!(remaining, vec![id_b]);
        });
    }
}
