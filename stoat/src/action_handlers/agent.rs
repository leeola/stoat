use crate::{
    app::{Stoat, UpdateEffect},
    host::terminal::TerminalSession,
    pane::View,
    run::{agent_socket_path_in, spawn_claude, spawn_term_reader},
    term_screen::TermScreen,
    term_session::TermSession,
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
    // The agent's hooks call back over this workspace's socket, so bind it
    // before the spawn hands over the path. Repeat calls for a workspace
    // already served cost nothing.
    let uid = stoat.active_workspace().uid();
    if let Err(err) = stoat.serve_term_session(uid) {
        tracing::warn!(target: "stoat::agent", %err, "session hook socket bind failed");
    }

    let host = stoat.terminal_host.clone();
    let executor = stoat.executor.clone();
    let pty_tx = stoat.pty_tx.clone();
    // Resolved before the workspace borrow, since the socket directory is an
    // instance-level knob rather than a workspace one.
    let socket_path = stoat
        .agent_socket_dir
        .as_deref()
        .map(|dir| agent_socket_path_in(dir, uid));
    let ws = stoat.active_workspace_mut();
    let cwd = ws.git_root.clone();
    let diff = ws.env.diff.clone();

    // The local terminal host opens the PTY synchronously, so the spawn future
    // is ready on first poll. The run pane drives its session writes through
    // the same `now_or_never` path.
    let spawned = spawn_claude(&*host, uid, &cwd, &diff, socket_path.as_deref()).now_or_never();
    let session = match spawned {
        Some(Ok(session)) => session,
        Some(Err(err)) => {
            tracing::warn!(target: "stoat::agent", %err, "failed to spawn claude session");
            return UpdateEffect::None;
        },
        None => return UpdateEffect::None,
    };

    let session: Arc<dyn TerminalSession> = Arc::from(session);
    let agent_id = ws.terms.insert(TermSession::new(
        TermScreen::new(AGENT_ROWS, AGENT_COLS),
        session.clone(),
        TermSession::next_token(),
    ));
    let focused = ws.panes.focus();
    ws.panes.pane_mut(focused).view = View::Agent(agent_id);

    spawn_term_reader(&executor, session, agent_id, pty_tx);
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use crate::{app::Stoat, pane::View};

    /// The `EDITOR` value the agent is handed, read back from `spawns`.
    ///
    /// It resolves this test binary's own path, so it cannot be written into an
    /// expected literal. Reading it back pins the rest of the vector in full
    /// while leaving the one unpinnable value to its own assertion.
    fn editor_command(env: &[(String, String)]) -> String {
        env.iter()
            .find(|(key, _)| key == "EDITOR")
            .map(|(_, value)| value.clone())
            .expect("the agent is always handed an editor command")
    }

    #[test]
    fn agent_spawn_carries_the_session_pair() {
        let mut h = Stoat::test();
        h.stoat.set_agent_socket_dir("/state".into());

        super::super::dispatch(&mut h.stoat, &stoat_action::SpawnClaude);

        let ws = h.stoat.active_workspace();
        assert!(
            matches!(ws.panes.pane(ws.panes.focus()).view, View::Agent(_)),
            "the focused pane holds the spawned agent",
        );

        let spawns = h.fake_terminal_host().spawns();
        assert_eq!(spawns.len(), 1);
        let editor = editor_command(&spawns[0].env);
        assert_eq!(
            spawns[0].env,
            vec![
                ("STOAT_SESSION".to_string(), ws.uid.to_string()),
                (
                    "STOAT_AGENT_SOCK".to_string(),
                    format!("/state/agent-{}.sock", ws.uid),
                ),
                ("EDITOR".to_string(), editor.clone()),
                ("VISUAL".to_string(), editor.clone()),
            ],
            "the agent is told which session and socket own it",
        );
        assert!(
            editor.ends_with(" editor"),
            "the editor bridge runs this binary's editor subcommand, got {editor:?}",
        );
    }

    // The server task is enqueued but never polled, since it needs a live
    // reactor. The directory is a path nothing binds.
    #[test]
    fn an_agent_spawn_serves_its_workspaces_socket() {
        let mut h = Stoat::test();
        h.stoat
            .set_agent_socket_dir("/stoat-test-never-served".into());
        h.stoat.set_serve_agent_sockets(true);
        let uid = h.stoat.active_workspace().uid();

        super::super::dispatch(&mut h.stoat, &stoat_action::SpawnClaude);

        assert_eq!(
            h.stoat.served_agent_sockets.iter().collect::<Vec<_>>(),
            vec![&uid],
            "the agent's hooks call back over this socket, so something has to be on it",
        );
    }

    #[test]
    fn agent_spawn_omits_the_session_pair_without_a_socket_dir() {
        let mut h = Stoat::test();

        super::super::dispatch(&mut h.stoat, &stoat_action::SpawnClaude);

        let spawns = h.fake_terminal_host().spawns();
        assert_eq!(spawns.len(), 1);
        let editor = editor_command(&spawns[0].env);
        assert_eq!(
            spawns[0].env,
            vec![
                ("EDITOR".to_string(), editor.clone()),
                ("VISUAL".to_string(), editor),
            ],
            "no socket dir means no session to name, but the editor bridge stands",
        );
    }
}
