//! Declarative configuration types consumed by the Claude test harness:
//! session seeds, turn-completion result parameters, and streaming options.

use crate::host::ClaudeSessionSummary;

/// Declarative description of a seeded session. All fields have sensible
/// defaults so `SessionSpec::default()` works. Convert `&str` or `String`
/// to `SessionSpec` for terse titled-only specs.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub updated_at: Option<String>,
    /// `true` dispatches [`stoat_action::OpenClaude`] to attach this
    /// session to the active workspace's chat panel. `false` (default)
    /// keeps the session in the slotmap without UI attachment.
    pub visible: bool,
}

impl Default for SessionSpec {
    fn default() -> Self {
        Self {
            session_id: None,
            title: None,
            cwd: None,
            updated_at: None,
            visible: false,
        }
    }
}

impl SessionSpec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn titled(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            ..Self::default()
        }
    }

    pub fn visible(mut self) -> Self {
        self.visible = true;
        self
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_updated_at(mut self, s: impl Into<String>) -> Self {
        self.updated_at = Some(s.into());
        self
    }

    /// Build the public summary the fake host returns from
    /// [`crate::host::ClaudeCodeHost::list_sessions`]. `seq` is the
    /// 0-indexed insertion order, used to default unset ids and titles
    /// to distinct values.
    pub(super) fn to_summary(&self, seq: usize) -> ClaudeSessionSummary {
        ClaudeSessionSummary {
            session_id: self
                .session_id
                .clone()
                .unwrap_or_else(|| format!("sess-{:02}", seq + 1)),
            cwd: self.cwd.clone().unwrap_or_else(|| "/tmp/fake".into()),
            title: self
                .title
                .clone()
                .unwrap_or_else(|| format!("Session {}", seq + 1)),
            updated_at: self
                .updated_at
                .clone()
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".into()),
        }
    }
}

impl From<&str> for SessionSpec {
    fn from(title: &str) -> Self {
        Self::titled(title)
    }
}

impl From<String> for SessionSpec {
    fn from(title: String) -> Self {
        Self::titled(title)
    }
}

/// Parameters for a `Result` turn-completion message.
#[derive(Debug, Clone, Copy)]
pub struct ResultSpec {
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub num_turns: u32,
}

impl Default for ResultSpec {
    fn default() -> Self {
        Self {
            cost_usd: 0.01,
            duration_ms: 500,
            num_turns: 1,
        }
    }
}

/// Configuration for [`super::ClaudeSessionHandle::stream_message_with`].
#[derive(Debug, Clone)]
pub struct StreamOpts {
    /// Approximate byte size of each cumulative `PartialText` chunk.
    /// `None` skips streaming and emits a single `Text` block.
    pub chunk_size: Option<usize>,
    /// When `true`, emit a `Result` after the final `Text`.
    pub terminate_turn: bool,
    /// Override the default [`ResultSpec`] for the terminating result.
    pub result: Option<ResultSpec>,
}

impl Default for StreamOpts {
    fn default() -> Self {
        Self {
            chunk_size: Some(20),
            terminate_turn: true,
            result: None,
        }
    }
}
