use crate::{
    app::{Stoat, UpdateEffect},
    host::terminal::TerminalSession,
    pane::View,
    run::{spawn_term_reader, spawn_terminal},
    term_screen::TermScreen,
    term_session::TermSession,
};
use futures::FutureExt;
use std::sync::Arc;

/// Dimensions the terminal PTY opens at before the render/resize pass fits it
/// to the focused pane.
const TERM_ROWS: u16 = 24;
const TERM_COLS: u16 = 80;

/// Open a subshell in the focused pane.
///
/// Spawns a fresh terminal session and points the focused pane at it,
/// recording the view it replaced in [`crate::pane::Pane::prev_view`] so it
/// can be restored if the terminal later exits in the last split pane. A spawn
/// failure leaves the focused pane unchanged.
///
/// The new pane enters insert mode so typing reaches the shell immediately.
/// The focus-arrival hook in [`Stoat::update`] covers the same transition when
/// the action is dispatched through the event loop, but the direct call here
/// also readies a terminal opened off that seam.
pub(super) fn open_terminal_pane(stoat: &mut Stoat) -> UpdateEffect {
    match spawn_terminal_view(stoat) {
        view @ View::Terminal(_) => {
            {
                let ws = stoat.active_workspace_mut();
                let focused = ws.panes.focus();
                let prev = ws.panes.pane(focused).view.clone();
                let pane = ws.panes.pane_mut(focused);
                pane.prev_view = Some(prev);
                pane.view = view;
            }
            stoat.transition_mode("insert".to_string());
            UpdateEffect::Redraw
        },
        _ => UpdateEffect::None,
    }
}

/// Respawn a fresh shell for every persisted terminal pane and dock whose
/// backing session did not survive, then repoint the view at it.
///
/// Terminal panes ride `PaneTree` serde as [`View::Terminal`], but the session
/// is a live OS resource that is not persisted, so the id is dead after a
/// restore or a workspace copy. Each dead pane and dock gets its own fresh
/// shell. Runtime state (history, running processes) is intentionally lost, and
/// a spawn failure leaves a `Terminal (closed)` label in place.
///
/// A focused terminal pane enters insert mode after the respawn, so a restore
/// or copy that lands focus on a terminal is typing-ready like any other focus
/// arrival ([`Stoat::auto_insert_focused_terminal`] covers the input-driven
/// paths).
pub(crate) fn respawn_terminal_panes(stoat: &mut Stoat) {
    let dead_panes = {
        let ws = stoat.active_workspace();
        ws.panes
            .split_pane_ids()
            .into_iter()
            .filter(|&id| {
                matches!(ws.panes.pane(id).view, View::Terminal(t) if !ws.terms.contains_key(t))
            })
            .collect::<Vec<_>>()
    };
    let dead_docks = {
        let ws = stoat.active_workspace();
        ws.docks
            .iter()
            .filter_map(|(id, dock)| {
                matches!(dock.view, View::Terminal(t) if !ws.terms.contains_key(t)).then_some(id)
            })
            .collect::<Vec<_>>()
    };

    for pane_id in dead_panes {
        let view = spawn_terminal_view(stoat);
        stoat.active_workspace_mut().panes.pane_mut(pane_id).view = view;
    }
    for dock_id in dead_docks {
        let view = spawn_terminal_view(stoat);
        if let Some(dock) = stoat.active_workspace_mut().docks.get_mut(dock_id) {
            dock.view = view;
        }
    }

    if stoat.focused_shell_term_id().is_some() && stoat.focused_mode() != "insert" {
        stoat.transition_mode("insert".to_string());
    }
}

/// Spawn a fresh terminal session and return a [`View::Terminal`] naming it,
/// or [`View::Label`] when the spawn fails.
///
/// Shared by the terminal action and the restore-time respawn. Resolves the
/// program and arguments from the `terminal.shell` / `terminal.args` settings
/// (falling back to `$SHELL`, then `/bin/sh`), stores the session alongside a
/// fresh screen emulator, and starts its reader.
fn spawn_terminal_view(stoat: &mut Stoat) -> View {
    let (program, args) = resolve_shell(
        stoat.settings.terminal_shell.as_deref(),
        stoat.settings.terminal_args.as_deref(),
        stoat.env_host().var("SHELL"),
    );

    let host = stoat.terminal_host.clone();
    let executor = stoat.executor.clone();
    let pty_tx = stoat.pty_tx.clone();
    let ws = stoat.active_workspace_mut();
    let cwd = ws.git_root.clone();
    let diff = ws.env.diff.clone();

    // The local terminal host opens the PTY synchronously, so the spawn future
    // is ready on first poll, matching the claude pane's spawn path.
    let session = match spawn_terminal(&*host, &cwd, &program, &args, &diff).now_or_never() {
        Some(Ok(session)) => session,
        Some(Err(err)) => {
            tracing::warn!(target: "stoat::terminal", %err, "failed to spawn terminal session");
            return View::Label("Terminal (closed)".into());
        },
        None => return View::Label("Terminal (closed)".into()),
    };

    let session: Arc<dyn TerminalSession> = Arc::from(session);
    let term_id = ws.terms.insert(TermSession::new(
        TermScreen::new(TERM_ROWS, TERM_COLS),
        session.clone(),
    ));
    spawn_term_reader(&executor, session, term_id, pty_tx);
    View::Terminal(term_id)
}

/// Resolve the shell program and arguments for a new terminal pane.
///
/// The program is the `terminal.shell` setting when set, otherwise the
/// `$SHELL` environment value, otherwise `/bin/sh`. Arguments come only from
/// the `terminal.args` setting, so a program resolved from the environment or
/// the `/bin/sh` fallback launches with none.
fn resolve_shell(
    settings_shell: Option<&str>,
    settings_args: Option<&[String]>,
    env_shell: Option<String>,
) -> (String, Vec<String>) {
    let program = settings_shell
        .map(str::to_owned)
        .or(env_shell)
        .unwrap_or_else(|| "/bin/sh".to_owned());
    let args = settings_args.map(|a| a.to_vec()).unwrap_or_default();
    (program, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolve_shell_precedence() {
        assert_eq!(
            resolve_shell(
                Some("/bin/zsh"),
                Some(&args(&["-l"])),
                Some("/bin/bash".into())
            ),
            ("/bin/zsh".to_string(), args(&["-l"])),
            "settings win over env",
        );
        assert_eq!(
            resolve_shell(None, None, Some("/bin/bash".into())),
            ("/bin/bash".to_string(), vec![]),
            "env shell used when unset, with no default args",
        );
        assert_eq!(
            resolve_shell(None, None, None),
            ("/bin/sh".to_string(), vec![]),
            "final fallback is /bin/sh",
        );
        assert_eq!(
            resolve_shell(None, Some(&args(&["-x"])), Some("/bin/bash".into())),
            ("/bin/bash".to_string(), args(&["-x"])),
            "args come from settings independent of program source",
        );
    }

    #[test]
    fn terminal_action_opens_terminal_pane() {
        let mut h = Stoat::test();
        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        h.stoat.terminal_host = Arc::new(crate::host::FakeTerminalHost::new(fake));
        h.allow_host_swap();

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let View::Terminal(term_id) = ws.panes.pane(focused).view else {
            panic!("focused pane should hold a terminal view");
        };
        assert!(
            ws.terms.contains_key(term_id),
            "spawned terminal session is stored",
        );
    }

    #[test]
    fn terminal_spawn_resolves_shell_from_env() {
        let mut h = Stoat::test();
        h.fake_env().set("SHELL", "/bin/fakesh");

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let spawns = h.fake_terminal_host().spawns();
        assert_eq!(spawns.len(), 1, "the terminal action spawns one session");
        assert_eq!(
            spawns[0].program, "/bin/fakesh",
            "the program resolves from $SHELL through EnvHost, not the real environment",
        );
        assert!(
            spawns[0].args.is_empty(),
            "an env-resolved shell launches with no args",
        );
    }

    #[test]
    fn terminal_shell_setting_beats_env() {
        let mut h = Stoat::test();
        h.fake_env().set("SHELL", "/bin/fakesh");
        h.stoat.settings.terminal_shell = Some("/bin/zsh".to_owned());
        h.stoat.settings.terminal_args = Some(args(&["-l"]));

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let spawns = h.fake_terminal_host().spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(
            spawns[0].program, "/bin/zsh",
            "the terminal.shell setting overrides the seeded $SHELL",
        );
        assert_eq!(
            spawns[0].args,
            args(&["-l"]),
            "terminal.args accompany the setting-resolved shell",
        );
    }

    #[test]
    fn respawn_replaces_dead_terminal_with_live_session() {
        use crate::term_session::TermId;

        let mut h = Stoat::test();
        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        h.stoat.terminal_host = Arc::new(crate::host::FakeTerminalHost::new(fake));
        h.allow_host_swap();

        // A restored terminal pane names a session id that no longer exists.
        let ws = h.stoat.active_workspace_mut();
        let pane = ws.panes.focus();
        let dead_id = TermId::default();
        ws.panes.pane_mut(pane).view = View::Terminal(dead_id);

        respawn_terminal_panes(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        let View::Terminal(new_id) = ws.panes.pane(pane).view else {
            panic!("dead terminal pane should be respawned as a terminal");
        };
        assert_ne!(new_id, dead_id, "respawned with a fresh session id");
        assert!(ws.terms.contains_key(new_id), "fresh session is stored");
    }
}
