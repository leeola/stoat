//! Per-session state for an owned agent (Claude) running inside a pane.
//!
//! Bundles the [`AgentTerm`] screen emulator with the [`TerminalSession`]
//! whose PTY output feeds it. The workspace owns a collection of these so it
//! can host several agent sessions at once, and a
//! [`View::Agent`](crate::pane::View::Agent) names one by its [`AgentId`].

use crate::{agent_term::AgentTerm, host::terminal::TerminalSession};
use slotmap::new_key_type;
use std::sync::Arc;

new_key_type! {
    /// Workspace-scoped key for an [`AgentSession`] in the workspace's agent
    /// collection.
    pub struct AgentId;
}

/// A live agent session pairing its screen emulator with the PTY session that
/// drives it.
///
/// The [`TerminalSession`] is held as an [`Arc`] so a background reader can
/// pull PTY output into [`Self::term`] while the app loop still writes input
/// to the same session.
pub struct AgentSession {
    pub term: AgentTerm,
    pub session: Arc<dyn TerminalSession>,
}

impl AgentSession {
    /// Resize the emulator and its PTY to `rows` by `cols` so the agent reflows
    /// to the hosting pane.
    ///
    /// A no-op when the emulator already matches, which keeps per-frame layout
    /// from issuing a redundant PTY resize (and the SIGWINCH redraw storm it
    /// would trigger in the agent) on every frame.
    pub fn fit(&mut self, rows: u16, cols: u16) {
        if self.term.rows() == rows as usize && self.term.cols() == cols as usize {
            return;
        }

        self.term.resize(rows, cols);
        if let Err(err) = self.session.resize(rows, cols) {
            tracing::warn!(target: "stoat::agent", %err, "failed to resize agent pty");
        }
    }
}
