//! Claude Code host interface: traits for session I/O, the session
//! manager ([`ClaudeCodeHost`]), and the notification channel the app
//! consumes to wire newly-created sessions into its state.
//!
//! Data types are split into submodules:
//! - [`types`]: shared shape types (tool classification, plans, usage).
//! - [`events`]: session/task/hook lifecycle event enums.
//! - [`message`]: the [`AgentMessage`] union.
//! - [`hooks`]: hook-callback interface.
//! - [`permission`]: permission-callback interface and outcome types.

mod events;
mod hooks;
mod message;
mod permission;
mod types;

use async_trait::async_trait;
pub use events::{HookLifecycleEvent, SessionStateEvent, TaskEvent};
pub use hooks::{HookCallback, HookDecision, HookEvent, HookKind, HookResponse};
pub use message::AgentMessage;
pub use permission::{
    PermissionBehavior, PermissionCallback, PermissionDestination, PermissionResult,
    PermissionRule, PermissionScope, PermissionSuggestion, ToolPermissionContext,
};
use slotmap::{new_key_type, SlotMap};
use std::{io, sync::Arc};
pub use types::{
    ModeInfo, ModelInfo, PlanEntry, PlanEntryStatus, TerminalMeta, TokenUsage, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolKind,
};

/// Per-session I/O handle for an active Claude Code conversation.
#[async_trait]
pub trait ClaudeCodeSession: Send + Sync {
    async fn send(&self, content: &str) -> io::Result<()>;
    async fn recv(&self) -> Option<AgentMessage>;
    fn is_alive(&self) -> bool;
    async fn shutdown(&self) -> io::Result<()>;

    /// Interrupt the current turn via the CLI's control protocol. The
    /// default implementation returns an error so existing fake
    /// sessions (which don't model control traffic) don't have to
    /// implement it. Real sessions override.
    async fn interrupt(&self) -> io::Result<()> {
        Err(io::Error::other(
            "interrupt not supported by this ClaudeCodeSession implementation",
        ))
    }

    /// Request a model switch mid-session. Default `Err`.
    async fn set_model(&self, _model_id: &str) -> io::Result<()> {
        Err(io::Error::other(
            "set_model not supported by this ClaudeCodeSession implementation",
        ))
    }

    /// Request a permission-mode switch mid-session. Default `Err`.
    async fn set_permission_mode(&self, _mode: &str) -> io::Result<()> {
        Err(io::Error::other(
            "set_permission_mode not supported by this ClaudeCodeSession implementation",
        ))
    }
}

/// Session manager that creates new [`ClaudeCodeSession`] instances.
///
/// Production uses a launcher that spawns Claude CLI subprocesses.
/// Tests use a fake that returns pre-configured sessions.
#[async_trait]
pub trait ClaudeCodeHost: Send + Sync {
    async fn new_session(&self) -> io::Result<Box<dyn ClaudeCodeSession>>;

    /// Resume a prior session by id. Default implementation forwards
    /// to [`Self::new_session`] so hosts that don't persist sessions
    /// still satisfy the trait.
    async fn resume_session(&self, _session_id: &str) -> io::Result<Box<dyn ClaudeCodeSession>> {
        self.new_session().await
    }

    /// Load a prior session and replay its history. Default
    /// implementation forwards to [`Self::new_session`].
    async fn load_session(&self, _session_id: &str) -> io::Result<Box<dyn ClaudeCodeSession>> {
        self.new_session().await
    }

    /// Fork an existing session under a new id. Default forwards to
    /// [`Self::new_session`] (i.e. treats fork as "new session, no
    /// shared history").
    async fn fork_session(
        &self,
        _parent_session_id: &str,
    ) -> io::Result<Box<dyn ClaudeCodeSession>> {
        self.new_session().await
    }

    /// List sessions known to this host. Default: empty list.
    async fn list_sessions(&self) -> io::Result<Vec<ClaudeSessionSummary>> {
        Ok(Vec::new())
    }
}

/// Summary of a session surfaced by [`ClaudeCodeHost::list_sessions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSessionSummary {
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at: String,
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
    use crate::host::fake::{FakeClaudeCode, FakeClaudeCodeHost};
    use async_trait::async_trait;

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

    struct MinimalSession;
    #[async_trait]
    impl ClaudeCodeSession for MinimalSession {
        async fn send(&self, _content: &str) -> io::Result<()> {
            Ok(())
        }
        async fn recv(&self) -> Option<AgentMessage> {
            None
        }
        fn is_alive(&self) -> bool {
            true
        }
        async fn shutdown(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn claude_code_session_defaults_return_err() {
        let s = MinimalSession;
        assert!(s.interrupt().await.is_err());
        assert!(s.set_model("opus").await.is_err());
        assert!(s.set_permission_mode("plan").await.is_err());
    }

    #[tokio::test]
    async fn claude_code_host_default_fallbacks_forward_to_new_session() {
        let host = FakeClaudeCodeHost::new();
        host.push_session(FakeClaudeCode::new());
        host.push_session(FakeClaudeCode::new());
        host.push_session(FakeClaudeCode::new());
        let r = host.resume_session("id").await;
        assert!(r.is_ok());
        let l = host.load_session("id").await;
        assert!(l.is_ok());
        let f = host.fork_session("parent").await;
        assert!(f.is_ok());
    }

    #[tokio::test]
    async fn claude_code_host_list_sessions_default_is_empty() {
        let host = FakeClaudeCodeHost::new();
        let out = host.list_sessions().await.unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn hook_kind_round_trips_known_names() {
        let names = [
            "PreToolUse",
            "PostToolUse",
            "UserPromptSubmit",
            "Stop",
            "SubagentStop",
            "SessionStart",
            "SessionEnd",
            "Notification",
            "PreCompact",
        ];
        for name in names {
            let kind = HookKind::from_name(name);
            assert_eq!(kind.as_name(), name);
        }
        let unknown = HookKind::from_name("FutureHook");
        assert_eq!(unknown.as_name(), "FutureHook");
        assert!(matches!(unknown, HookKind::Unknown(_)));
    }

    #[test]
    fn hook_response_helpers_round_trip() {
        let cont = HookResponse::r#continue();
        assert!(cont.r#continue);
        assert!(cont.decision.is_none());

        let blocked = HookResponse::block("because");
        assert!(!blocked.r#continue);
        match blocked.decision.as_ref().unwrap() {
            HookDecision::Block { reason } => assert_eq!(reason, "because"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn permission_result_convenience_constructors() {
        let a = PermissionResult::allow();
        assert!(matches!(
            a,
            PermissionResult::Allow {
                scope: PermissionScope::Once,
                ..
            }
        ));
        let a2 = PermissionResult::allow_with_scope(PermissionScope::Always);
        assert!(matches!(
            a2,
            PermissionResult::Allow {
                scope: PermissionScope::Always,
                ..
            }
        ));
        let d = PermissionResult::deny("no");
        match d {
            PermissionResult::Deny { message, interrupt } => {
                assert_eq!(message, "no");
                assert!(!interrupt);
            },
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            PermissionResult::cancel(),
            PermissionResult::Cancel
        ));
    }

    #[test]
    fn tool_permission_context_bare_has_sensible_defaults() {
        let ctx = ToolPermissionContext::bare();
        assert!(ctx.suggestions_json.is_none());
        assert!(ctx.tool_use_id.is_none());
        assert!(ctx.tool_title.is_empty());
        assert!(matches!(ctx.tool_kind, ToolKind::Other));
        assert!(ctx.tool_content.is_empty());
        assert!(ctx.tool_locations.is_empty());
    }
}
