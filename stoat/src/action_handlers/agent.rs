use crate::{
    agent_session::AgentSession,
    agent_term::AgentTerm,
    app::{Stoat, UpdateEffect},
    host::terminal::TerminalSession,
    pane::View,
    run::{spawn_agent_reader, spawn_claude},
};
use futures::FutureExt;
use std::sync::Arc;

/// Dimensions the owned Claude PTY is opened at. The render/resize sibling
/// later fits both the PTY and the emulator to the focused pane.
const AGENT_ROWS: u16 = 24;
const AGENT_COLS: u16 = 80;

/// Launch a Claude agent session into the focused pane.
///
/// Spawns the subshell through the terminal host, stores it alongside a fresh
/// screen emulator in the workspace's agent collection, and points the focused
/// pane at the new [`View::Agent`]. A spawn failure leaves the pane unchanged.
pub(super) fn spawn_claude_pane(stoat: &mut Stoat) -> UpdateEffect {
    let host = stoat.terminal_host.clone();
    let executor = stoat.executor.clone();
    let pty_tx = stoat.pty_tx.clone();
    let ws = stoat.active_workspace_mut();
    let uid = ws.uid;
    let cwd = ws.git_root.clone();

    // The local terminal host opens the PTY synchronously, so the spawn future
    // is ready on first poll. The run pane drives its session writes through
    // the same `now_or_never` path.
    let session = match spawn_claude(&*host, uid, &cwd).now_or_never() {
        Some(Ok(session)) => session,
        Some(Err(err)) => {
            tracing::warn!(target: "stoat::agent", %err, "failed to spawn claude session");
            return UpdateEffect::None;
        },
        None => return UpdateEffect::None,
    };

    let session: Arc<dyn TerminalSession> = Arc::from(session);
    let agent_id = ws.agents.insert(AgentSession {
        term: AgentTerm::new(AGENT_ROWS, AGENT_COLS),
        session: session.clone(),
    });
    let focused = ws.panes.focus();
    ws.panes.pane_mut(focused).view = View::Agent(agent_id);

    spawn_agent_reader(&executor, session, agent_id, pty_tx);
    UpdateEffect::Redraw
}
