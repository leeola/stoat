//! ACP-shaped agent host interface: a session manager
//! ([`AgentConnection`]) that spawns per-conversation I/O handles
//! ([`AgentSession`]).
//!
//! Mirrors [`crate::host::claude_code::ClaudeCodeHost`] /
//! [`crate::host::claude_code::ClaudeCodeSession`], which these supersede
//! for the ACP transport. The Claude traits stay until the stream-json
//! stack is retired; the heavy ACP implementation lives in a separate
//! crate and routes its agent output back through the shared
//! [`AgentMessage`] event.

use crate::host::claude_code::AgentMessage;
use async_trait::async_trait;
use std::io;

/// Which mid-session controls an [`AgentSession`] supports, reflecting the
/// agent's advertised ACP capabilities. All `false` by default, so a
/// session that models none still satisfies the trait. Callers gate UI on
/// these before invoking the matching method, which otherwise returns an
/// unsupported-operation error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentSessionCapabilities {
    pub set_mode: bool,
    pub set_config_option: bool,
    pub select_model: bool,
}

/// Per-session I/O handle for an active agent conversation. Supersedes
/// [`crate::host::claude_code::ClaudeCodeSession`].
#[async_trait]
pub trait AgentSession: Send + Sync {
    /// Send a user prompt, starting or continuing a turn. The turn's
    /// output streams back through [`Self::recv`]; this resolves once the
    /// prompt has been delivered, not when the turn completes.
    async fn prompt(&self, content: &str) -> io::Result<()>;

    /// Cancel the in-flight turn.
    async fn cancel(&self) -> io::Result<()>;

    /// Receive the next streamed agent event, or `None` once the session
    /// has ended.
    async fn recv(&self) -> Option<AgentMessage>;

    fn is_alive(&self) -> bool;

    async fn shutdown(&self) -> io::Result<()>;

    /// Controls this session supports. Default: none. A capable
    /// implementation overrides this alongside the gated methods below.
    fn capabilities(&self) -> AgentSessionCapabilities {
        AgentSessionCapabilities::default()
    }

    /// Switch the agent's session mode. Default `Err`; gated on
    /// [`AgentSessionCapabilities::set_mode`].
    async fn set_mode(&self, _mode_id: &str) -> io::Result<()> {
        Err(unsupported("set_mode"))
    }

    /// Set a session config option to one of its allowed values. Default
    /// `Err`; gated on [`AgentSessionCapabilities::set_config_option`].
    async fn set_config_option(&self, _config_id: &str, _value_id: &str) -> io::Result<()> {
        Err(unsupported("set_config_option"))
    }

    /// Switch the agent's model mid-session. Default `Err`; gated on
    /// [`AgentSessionCapabilities::select_model`].
    async fn select_model(&self, _model_id: &str) -> io::Result<()> {
        Err(unsupported("select_model"))
    }
}

/// Session manager that spawns [`AgentSession`] handles. Supersedes
/// [`crate::host::claude_code::ClaudeCodeHost`]. Production resolves and
/// launches an ACP agent over stdio JSON-RPC; tests use an in-memory
/// fake.
#[async_trait]
pub trait AgentConnection: Send + Sync {
    /// Start a new conversation, returning its session handle.
    async fn new_session(&self) -> io::Result<Box<dyn AgentSession>>;

    /// Resume a prior session by id. Default forwards to
    /// [`Self::new_session`] for connections that do not persist sessions.
    async fn resume_session(&self, _session_id: &str) -> io::Result<Box<dyn AgentSession>> {
        self.new_session().await
    }

    /// Load a prior session and replay its history. Default forwards to
    /// [`Self::new_session`].
    async fn load_session(&self, _session_id: &str) -> io::Result<Box<dyn AgentSession>> {
        self.new_session().await
    }
}

fn unsupported(method: &str) -> io::Error {
    io::Error::other(format!(
        "{method} is not supported by this AgentSession implementation"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MinimalSession;

    #[async_trait]
    impl AgentSession for MinimalSession {
        async fn prompt(&self, _content: &str) -> io::Result<()> {
            Ok(())
        }
        async fn cancel(&self) -> io::Result<()> {
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

    struct MinimalConnection;

    #[async_trait]
    impl AgentConnection for MinimalConnection {
        async fn new_session(&self) -> io::Result<Box<dyn AgentSession>> {
            Ok(Box::new(MinimalSession))
        }
    }

    #[tokio::test]
    async fn capability_defaults_are_unsupported() {
        let session = MinimalSession;
        let caps = session.capabilities();
        assert!(!caps.set_mode && !caps.set_config_option && !caps.select_model);
        assert!(session.set_mode("plan").await.is_err());
        assert!(session
            .set_config_option("verbosity", "high")
            .await
            .is_err());
        assert!(session.select_model("opus").await.is_err());
    }

    #[tokio::test]
    async fn connection_lifecycle_defaults_forward_to_new_session() {
        let conn = MinimalConnection;
        assert!(conn.new_session().await.is_ok());
        assert!(conn.resume_session("id").await.is_ok());
        assert!(conn.load_session("id").await.is_ok());
    }
}
