//! Per-session PTY state for a pane running a terminal shell or an agent.
//!
//! Bundles the [`TermScreen`] screen emulator with the [`TerminalSession`]
//! whose PTY output feeds it. The workspace owns a collection of these so it
//! can host several sessions at once, and a pane view such as
//! [`View::Agent`](crate::pane::View::Agent) names one by its [`TermId`].

use crate::{host::terminal::TerminalSession, term_screen::TermScreen};
use futures::FutureExt;
use slotmap::new_key_type;
use std::sync::Arc;

new_key_type! {
    /// Workspace-scoped key for a [`TermSession`] in the workspace's term
    /// collection.
    pub struct TermId;
}

/// A live term session pairing its screen emulator with the PTY session that
/// drives it.
///
/// The [`TerminalSession`] is held as an [`Arc`] so a background reader can
/// pull PTY output into [`Self::term`] while the app loop still writes input
/// to the same session.
pub struct TermSession {
    pub term: TermScreen,
    pub session: Arc<dyn TerminalSession>,
}

impl TermSession {
    /// Resize the emulator and its PTY to `rows` by `cols` so the child reflows
    /// to the hosting pane.
    ///
    /// A no-op when the emulator already matches, which keeps per-frame layout
    /// from issuing a redundant PTY resize (and the SIGWINCH redraw storm it
    /// would trigger in the child) on every frame.
    pub fn fit(&mut self, rows: u16, cols: u16) {
        if self.term.rows() == rows as usize && self.term.cols() == cols as usize {
            return;
        }

        let replies = self.term.resize(rows, cols);
        if let Err(err) = self.session.resize(rows, cols) {
            tracing::warn!(target: "stoat::agent", %err, "failed to resize agent pty");
        }
        if !replies.is_empty() {
            let _ = self.session.write(&replies).now_or_never();
        }
    }
}
