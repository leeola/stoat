//! Status model for the owned Claude subshell.
//!
//! Stoat spawns `claude` as an owned PTY subshell and learns what it is
//! doing through a stream of hook callbacks (see the per-session IPC server
//! that feeds [`AgentStatus::apply`]). This module turns that event stream
//! into a small piece of state the render process owns and reads on paint,
//! and projects it into a status badge.

use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};

/// Lifecycle phase of the owned Claude subshell, derived from its hook
/// event stream.
///
/// The phases are not a strict linear progression: a session bounces between
/// [`Working`](Self::Working) and [`Idle`](Self::Idle) as it runs tools, and
/// may reach [`AwaitingInput`](Self::AwaitingInput) at any point when it needs
/// the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhase {
    /// Session started but no tool activity has been reported yet.
    Starting,
    /// A tool is executing. Its name is carried in the status and surfaced
    /// in the badge label.
    Working,
    /// Between tools, or stopped after a turn. The agent is alive but not
    /// actively running a tool.
    Idle,
    /// The agent raised a notification and is waiting on the user.
    AwaitingInput,
    /// The session ended cleanly via a session-end hook.
    Ended,
}

/// A status event reported by the owned Claude subshell's hooks.
///
/// Each variant corresponds to one hook in Claude's hook set. The IPC server
/// decodes a socket message into one of these and feeds it to
/// [`AgentStatus::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentHookEvent {
    /// `session-start`: a new agent session began.
    SessionStart,
    /// `pre-tool-use`: the agent is about to run `tool`.
    PreToolUse { tool: String },
    /// `post-tool-use`: the most recent tool finished.
    PostToolUse,
    /// `notification`: the agent needs the user's attention.
    Notification,
    /// `stop`: the agent finished a turn and is idle.
    Stop,
    /// `session-end`: the agent session ended cleanly.
    SessionEnd,
}

/// Status of the owned Claude subshell for one workspace session.
///
/// Owned by the render process (stored on the workspace) and read on paint,
/// off the agent's IPC path.
///
/// Liveness is tracked separately from [`AgentPhase`]. A crash, where the
/// process exits without a clean `session-end` hook, then surfaces distinctly
/// from a normal [`AgentPhase::Ended`] finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    phase: AgentPhase,
    current_tool: Option<String>,
    alive: bool,
}

impl AgentStatus {
    pub fn new() -> Self {
        Self {
            phase: AgentPhase::Starting,
            current_tool: None,
            alive: true,
        }
    }

    /// Advance the status by one hook event.
    pub fn apply(&mut self, event: AgentHookEvent) {
        match event {
            AgentHookEvent::SessionStart => {
                self.phase = AgentPhase::Starting;
                self.current_tool = None;
                self.alive = true;
            },
            AgentHookEvent::PreToolUse { tool } => {
                self.phase = AgentPhase::Working;
                self.current_tool = Some(tool);
            },
            AgentHookEvent::PostToolUse | AgentHookEvent::Stop => {
                self.phase = AgentPhase::Idle;
                self.current_tool = None;
            },
            AgentHookEvent::Notification => {
                self.phase = AgentPhase::AwaitingInput;
            },
            AgentHookEvent::SessionEnd => {
                self.phase = AgentPhase::Ended;
                self.current_tool = None;
                self.alive = false;
            },
        }
    }

    /// Record that the agent process exited without a clean session-end,
    /// e.g. a crash the owning session detected via the PTY. Liveness drops
    /// while the phase is preserved, so [`Self::badge`] surfaces an error
    /// rather than treating it as a completed session.
    pub fn mark_exited(&mut self) {
        self.alive = false;
    }

    /// Project the status into a status badge, or [`None`] when the session
    /// has ended cleanly and no longer warrants an overlay.
    pub fn badge(&self) -> Option<Badge> {
        let label = match (self.alive, self.phase) {
            (_, AgentPhase::Ended) => return None,
            (false, _) => "claude: exited".to_string(),
            (true, AgentPhase::Starting) => "claude: starting".to_string(),
            (true, AgentPhase::Working) => match &self.current_tool {
                Some(tool) => format!("claude: {tool}"),
                None => "claude: working".to_string(),
            },
            (true, AgentPhase::Idle) => "claude: idle".to_string(),
            (true, AgentPhase::AwaitingInput) => "claude: awaiting input".to_string(),
        };

        let state = if self.alive {
            BadgeState::Active
        } else {
            BadgeState::Error
        };

        Some(Badge {
            source: BadgeSource::Agent,
            anchor: Anchor::BottomRight,
            state,
            label,
            detail: None,
        })
    }
}

impl Default for AgentStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_transitions() {
        let mut s = AgentStatus::new();
        assert_eq!(s.phase, AgentPhase::Starting);
        assert!(s.alive);

        s.apply(AgentHookEvent::PreToolUse {
            tool: "Read".into(),
        });
        assert_eq!(s.phase, AgentPhase::Working);
        assert_eq!(s.current_tool.as_deref(), Some("Read"));

        s.apply(AgentHookEvent::PostToolUse);
        assert_eq!(s.phase, AgentPhase::Idle);
        assert_eq!(s.current_tool, None);

        s.apply(AgentHookEvent::Notification);
        assert_eq!(s.phase, AgentPhase::AwaitingInput);

        s.apply(AgentHookEvent::Stop);
        assert_eq!(s.phase, AgentPhase::Idle);

        s.apply(AgentHookEvent::SessionEnd);
        assert_eq!(s.phase, AgentPhase::Ended);
        assert!(!s.alive);
    }

    #[test]
    fn badge_tracks_active_phase() {
        let mut s = AgentStatus::new();
        s.apply(AgentHookEvent::PreToolUse {
            tool: "Bash".into(),
        });

        let badge = s.badge().expect("active session has a badge");
        assert_eq!(badge.source, BadgeSource::Agent);
        assert_eq!(badge.state, BadgeState::Active);
        assert_eq!(badge.label, "claude: Bash");
        assert_eq!(badge.anchor, Anchor::BottomRight);
    }

    #[test]
    fn ended_session_has_no_badge() {
        let mut s = AgentStatus::new();
        s.apply(AgentHookEvent::SessionEnd);
        assert!(s.badge().is_none());
    }

    #[test]
    fn unexpected_exit_surfaces_error() {
        let mut s = AgentStatus::new();
        s.apply(AgentHookEvent::PreToolUse {
            tool: "Bash".into(),
        });
        s.mark_exited();

        let badge = s.badge().expect("crashed session still surfaces");
        assert_eq!(badge.state, BadgeState::Error);
        assert_eq!(badge.label, "claude: exited");
    }
}
