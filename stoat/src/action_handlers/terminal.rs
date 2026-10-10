use crate::{
    app::{Stoat, UpdateEffect},
    file_finder::OpenIntent,
    host::terminal::TerminalSession,
    pane::{DockVisibility, PaneId, View},
    run::{agent_socket_path_in, spawn_term_reader, spawn_terminal, TermSpawnEnv},
    term_screen::TermScreen,
    term_session::{TermId, TermLocation, TermSession},
    workspace::Workspace,
};
use futures::FutureExt;
use std::sync::Arc;
use stoat_action::{SplitNewDown, SplitNewRight};

/// Dimensions the terminal PTY opens at before the render/resize pass fits it
/// to the focused pane.
const TERM_ROWS: u16 = 24;
const TERM_COLS: u16 = 80;

/// Open a subshell in the focused pane, or return to the one already hidden
/// behind it.
///
/// Opening a file in front of a terminal leaves the shell live and recorded in
/// [`crate::pane::Pane::prev_view`], so this action and that open are two
/// halves of a round trip over one shell rather than a way to accumulate them.
/// A record naming a session that has since died falls through to a fresh
/// spawn, as does one naming a session another pane or dock already shows,
/// since two views over one PTY fight for its input.
///
/// A fresh spawn points the focused pane at the new session and records the
/// view it replaced, which is what restores that view if the terminal later
/// exits in the last split pane. A spawn failure leaves the pane unchanged.
///
/// A shell that comes back is put in normal mode, so its keys reach it
/// whatever chord it was left in.
pub(super) fn open_terminal_pane(stoat: &mut Stoat) -> UpdateEffect {
    let covered = {
        let ws = stoat.active_workspace();
        covered_terminal(ws, ws.panes.focus())
    };
    if let Some(term_id) = covered {
        show_in_focused_pane(stoat, term_id);
        return UpdateEffect::Redraw;
    }

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
            UpdateEffect::Redraw
        },
        _ => UpdateEffect::None,
    }
}

/// Show `term_id` in the focused pane, with the view it covers recorded behind
/// it, and put the pane in normal mode, where its keys reach the shell.
fn show_in_focused_pane(stoat: &mut Stoat, term_id: TermId) {
    {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let pane = ws.panes.pane_mut(focused);
        pane.prev_view = Some(std::mem::replace(&mut pane.view, View::Terminal(term_id)));
    }
    stoat.transition_mode("normal".to_string());
}

/// The live shell `pane` covers, ready to be shown again.
///
/// `None` unless the pane's record names a terminal, the workspace still holds
/// that session, and no view in any tab, window, or dock already shows it.
pub(super) fn covered_terminal(ws: &Workspace, pane: PaneId) -> Option<TermId> {
    let Some(View::Terminal(term_id)) = ws.panes.pane(pane).prev_view else {
        return None;
    };
    (ws.terms.contains_key(term_id) && !ws.term_shown(term_id)).then_some(term_id)
}

/// Bring terminal `term_id` to focus, at the view that shows it, or in the
/// focused pane when it is covered.
///
/// A shown terminal takes focus where it is, across tabs, and the split intent
/// has no effect, since two views over one PTY fight for its input. A hidden
/// dock that shows it opens. For a covered terminal, the split intent applies
/// first. If the session exited, the status line reports it.
pub(super) fn show_terminal(
    stoat: &mut Stoat,
    term_id: TermId,
    intent: OpenIntent,
) -> UpdateEffect {
    if !stoat.active_workspace().terms.contains_key(term_id) {
        stoat.set_status("terminal exited");
        return UpdateEffect::Redraw;
    }

    // A terminal pane rests in normal mode, where its keys go to the child, so
    // the focus move sets no mode.
    if let Some(at) = stoat.active_workspace().term_location(term_id) {
        if let TermLocation::Dock(id) = at
            && let Some(dock) = stoat.active_workspace_mut().docks.get_mut(id)
            && dock.visibility == DockVisibility::Hidden
        {
            dock.visibility = DockVisibility::Open {
                width: dock.default_width,
            };
        }
        stoat.focus_location(at);
        return UpdateEffect::Redraw;
    }

    match intent {
        OpenIntent::Replace => {},
        OpenIntent::HSplit => {
            super::dispatch(stoat, &SplitNewDown);
        },
        OpenIntent::VSplit => {
            super::dispatch(stoat, &SplitNewRight);
        },
    }
    show_in_focused_pane(stoat, term_id);
    UpdateEffect::Redraw
}

/// Respawn a fresh shell for every persisted terminal pane and dock whose
/// backing session did not survive, then repoint the view at it.
///
/// The panes of every tab count, a parked one included, so a tab shows a live
/// shell when the reader next switches to it.
///
/// Terminal panes ride `PaneTree` serde as [`View::Terminal`], but the session
/// is a live OS resource that is not persisted, so the id is dead after a
/// restore or a workspace copy. Each dead pane and dock gets its own fresh
/// shell. Runtime state (history, running processes) is intentionally lost, and
/// a spawn failure leaves a `Terminal (closed)` label in place.
pub(crate) fn respawn_terminal_panes(stoat: &mut Stoat) {
    let dead_panes = {
        let ws = stoat.active_workspace();
        let dead = |view: &View| matches!(view, View::Terminal(t) if !ws.terms.contains_key(*t));
        ws.pane_trees()
            .enumerate()
            .flat_map(|(tree, panes)| {
                panes
                    .split_pane_ids()
                    .into_iter()
                    .filter(move |&id| dead(&panes.pane(id).view))
                    .map(move |id| (tree, id))
            })
            .collect::<Vec<(usize, PaneId)>>()
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

    // `pane_trees_mut` walks the trees in the order `pane_trees` does, so the
    // index collected above names the same tree here.
    for (tree, pane_id) in dead_panes {
        let view = spawn_terminal_view(stoat);
        if let Some(panes) = stoat.active_workspace_mut().pane_trees_mut().nth(tree) {
            panes.pane_mut(pane_id).view = view;
        }
    }
    for dock_id in dead_docks {
        let view = spawn_terminal_view(stoat);
        if let Some(dock) = stoat.active_workspace_mut().docks.get_mut(dock_id) {
            dock.view = view;
        }
    }
}

/// Spawn a fresh terminal session and return a [`View::Terminal`] naming it,
/// or [`View::Label`] when the spawn fails.
///
/// Shared by the terminal action and the restore-time respawn. Resolves the
/// program and arguments from the `terminal.shell` / `terminal.args` settings
/// (falling back to `$SHELL`, then `/bin/sh`), stores the session alongside a
/// fresh screen emulator, and starts its reader.
pub(super) fn spawn_terminal_view(stoat: &mut Stoat) -> View {
    let (program, args) = resolve_shell(
        stoat.settings.terminal_shell.as_deref(),
        stoat.settings.terminal_args.as_deref(),
        stoat.env_host().var("SHELL"),
    );

    // The shell's environment names this workspace's socket, so bind it before
    // handing over the path. Repeat calls for a workspace already served cost
    // nothing.
    let uid = stoat.active_workspace().uid();
    if let Err(err) = stoat.serve_term_session(uid) {
        tracing::warn!(target: "stoat::terminal", %err, "session hook socket bind failed");
    }

    let host = stoat.terminal_host.clone();
    let executor = stoat.executor.clone();
    let pty_tx = stoat.pty_tx.clone();
    let socket_dir = stoat.agent_socket_dir.clone();
    let workspace = stoat.active_workspace;
    let ws = stoat.active_workspace_mut();
    let cwd = ws.git_root.clone();
    let diff = ws.env.diff.clone();

    // Minted before the spawn because the child's environment names it, and the
    // session it belongs to does not exist until the insert below.
    let token = TermSession::next_token();
    let session_env = socket_dir.map(|dir| TermSpawnEnv {
        uid,
        socket_path: agent_socket_path_in(&dir, uid),
        token,
    });

    // The local terminal host opens the PTY synchronously, so the spawn future
    // is ready on first poll, matching the claude pane's spawn path.
    let spawned = spawn_terminal(&*host, &cwd, &program, &args, &diff, session_env).now_or_never();
    let session = match spawned {
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
        token,
    ));
    spawn_term_reader(&executor, session, workspace, term_id, pty_tx);
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

    /// Open a terminal in the focused pane, then cover it the way an
    /// open-in-term request does. The buffer sits in front and the shell is
    /// recorded behind it.
    fn hide_a_live_terminal(h: &mut crate::test_harness::TestHarness) -> TermId {
        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let ws = h.stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let pane = ws.panes.pane_mut(focused);
        let View::Terminal(term_id) = pane.view else {
            panic!("the terminal action should leave a terminal in the focused pane");
        };
        let covering = pane
            .prev_view
            .take()
            .expect("the view the terminal replaced");
        pane.prev_view = Some(std::mem::replace(&mut pane.view, covering));
        term_id
    }

    fn focused_view_terminal(h: &crate::test_harness::TestHarness) -> TermId {
        let ws = h.stoat.active_workspace();
        let View::Terminal(term_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("focused pane should hold a terminal view");
        };
        term_id
    }

    #[test]
    fn showing_a_covered_terminal_puts_it_in_the_focused_pane() {
        let mut h = Stoat::test();
        let hidden = hide_a_live_terminal(&mut h);

        show_terminal(&mut h.stoat, hidden, OpenIntent::Replace);

        assert_eq!(
            (
                focused_view_terminal(&h),
                h.stoat.focused_mode().to_string(),
                h.stoat.active_workspace().panes.split_pane_ids().len(),
            ),
            (hidden, "normal".to_string(), 1),
        );
    }

    #[test]
    fn showing_a_covered_terminal_with_a_split_intent_opens_a_pane_for_it() {
        let mut h = Stoat::test();
        let hidden = hide_a_live_terminal(&mut h);

        show_terminal(&mut h.stoat, hidden, OpenIntent::VSplit);

        assert_eq!(
            (
                focused_view_terminal(&h),
                h.stoat.active_workspace().panes.split_pane_ids().len(),
            ),
            (hidden, 2),
        );
    }

    /// Two views over one PTY fight for its input, so a shown terminal takes
    /// focus where it is and a split intent adds nothing.
    #[test]
    fn showing_a_shown_terminal_focuses_its_pane_and_adds_no_view() {
        let mut h = Stoat::test();
        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let terminal_pane = h.stoat.active_workspace().panes.focus();
        let term_id = focused_view_terminal(&h);
        super::super::dispatch(&mut h.stoat, &SplitNewRight);

        show_terminal(&mut h.stoat, term_id, OpenIntent::VSplit);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.panes.focus(), ws.panes.split_pane_ids().len()),
            (terminal_pane, 2),
        );
    }

    #[test]
    fn showing_a_terminal_in_a_hidden_dock_opens_the_dock_and_focuses_it() {
        let mut h = Stoat::test();
        let hidden = hide_a_live_terminal(&mut h);
        show_in_a_dock(&mut h.stoat, hidden);

        show_terminal(&mut h.stoat, hidden, OpenIntent::Replace);

        let ws = h.stoat.active_workspace();
        let (id, dock) = ws.docks.iter().next().expect("the dock");
        assert_eq!(
            (ws.focus, dock.visibility),
            (
                crate::pane::FocusTarget::Dock(id),
                DockVisibility::Open { width: 30 },
            ),
        );
    }

    #[test]
    fn the_terminal_action_returns_to_the_shell_it_hid() {
        let mut h = Stoat::test();
        let hidden = hide_a_live_terminal(&mut h);

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        assert_eq!(
            focused_view_terminal(&h),
            hidden,
            "the shell behind the buffer comes back",
        );
        assert_eq!(
            h.fake_terminal_host().spawns().len(),
            1,
            "returning to a live shell starts no second one",
        );
        assert!(
            matches!(
                h.stoat
                    .active_workspace()
                    .panes
                    .pane(h.stoat.active_workspace().panes.focus())
                    .prev_view,
                Some(View::Editor(_)),
            ),
            "the buffer it covered is what the next toggle returns to",
        );
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn quit_on_a_pane_that_covers_a_shell_returns_to_the_shell() {
        returns_to_the_covered_shell(&stoat_action::Quit);
    }

    #[test]
    fn a_buffer_close_on_a_pane_that_covers_a_shell_returns_to_the_shell() {
        returns_to_the_covered_shell(&stoat_action::CloseBuffer);
    }

    /// Dispatch `action` on a pane that covers a live shell, and assert the
    /// shell comes back in that pane with no record left and no second spawn.
    fn returns_to_the_covered_shell(action: &dyn stoat_action::Action) {
        let mut h = Stoat::test();
        let hidden = hide_a_live_terminal(&mut h);

        let effect = super::super::dispatch(&mut h.stoat, action);

        let ws = h.stoat.active_workspace();
        let no_record = ws.panes.pane(ws.panes.focus()).prev_view.is_none();
        assert_eq!(
            (
                effect,
                focused_view_terminal(&h),
                no_record,
                h.stoat.focused_mode(),
                h.fake_terminal_host().spawns().len(),
            ),
            (UpdateEffect::Redraw, hidden, true, "normal", 1),
        );
    }

    #[test]
    fn closing_a_pane_over_a_covered_shell_ends_the_shell() {
        assert_eq!(
            close_over_a_covered_shell(&stoat_action::ClosePane, |_, _| {}),
            (false, true),
        );
    }

    #[test]
    fn closing_a_pane_keeps_a_covered_shell_that_a_dock_shows() {
        assert_eq!(
            close_over_a_covered_shell(&stoat_action::ClosePane, show_in_a_dock),
            (true, false),
        );
    }

    #[test]
    fn closing_a_tab_over_a_covered_shell_ends_the_shell() {
        let away_and_back = |stoat: &mut Stoat, _| {
            super::super::dispatch(stoat, &stoat_action::NewTab);
            super::super::dispatch(stoat, &stoat_action::PrevTab);
        };
        assert_eq!(
            close_over_a_covered_shell(&stoat_action::CloseTab, away_and_back),
            (false, true),
        );
    }

    /// Cover a live shell in a second pane, run `before`, dispatch `close`,
    /// and report whether the session lives on and whether its child was
    /// killed.
    fn close_over_a_covered_shell(
        close: &dyn stoat_action::Action,
        before: impl FnOnce(&mut Stoat, TermId),
    ) -> (bool, bool) {
        let mut h = Stoat::test();
        h.stoat
            .active_workspace_mut()
            .panes
            .split(crate::pane::Axis::Vertical);
        let hidden = hide_a_live_terminal(&mut h);
        before(&mut h.stoat, hidden);

        super::super::dispatch(&mut h.stoat, close);
        h.settle();

        (
            h.stoat.active_workspace().terms.contains_key(hidden),
            h.fake_terminal().was_killed(),
        )
    }

    #[test]
    fn a_dead_recorded_shell_spawns_a_fresh_one() {
        let mut h = Stoat::test();
        let hidden = hide_a_live_terminal(&mut h);
        h.stoat.active_workspace_mut().terms.remove(hidden);

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        assert_ne!(
            focused_view_terminal(&h),
            hidden,
            "a record outliving its session is no shell to return to",
        );
        assert_eq!(h.fake_terminal_host().spawns().len(), 2);
    }

    /// Hide a shell, then have `show_elsewhere` put it on another surface, and
    /// assert the action spawns rather than returning to it.
    fn refuses_a_shell_shown_elsewhere(show_elsewhere: impl FnOnce(&mut Stoat, TermId)) {
        let mut h = Stoat::test();
        let hidden = hide_a_live_terminal(&mut h);
        let covered = h.stoat.active_workspace().panes.focus();

        show_elsewhere(&mut h.stoat, hidden);
        h.stoat.active_workspace_mut().panes.set_focus(covered);

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        assert_ne!(
            focused_view_terminal(&h),
            hidden,
            "one PTY driven from two surfaces fights over its input",
        );
        assert_eq!(h.fake_terminal_host().spawns().len(), 2);
    }

    #[test]
    fn a_shell_another_pane_shows_spawns_a_fresh_one() {
        refuses_a_shell_shown_elsewhere(|stoat, hidden| {
            let ws = stoat.active_workspace_mut();
            let side = ws.panes.split(crate::pane::Axis::Vertical);
            ws.panes.pane_mut(side).view = View::Terminal(hidden);
        });
    }

    #[test]
    fn a_shell_a_dock_shows_spawns_a_fresh_one() {
        refuses_a_shell_shown_elsewhere(show_in_a_dock);
    }

    #[test]
    fn a_shell_another_tab_shows_spawns_a_fresh_one() {
        refuses_a_shell_shown_elsewhere(|stoat, hidden| {
            super::super::dispatch(stoat, &stoat_action::NewTab);
            let ws = stoat.active_workspace_mut();
            let pane = ws.panes.focus();
            ws.panes.pane_mut(pane).view = View::Terminal(hidden);
            super::super::dispatch(stoat, &stoat_action::PrevTab);
        });
    }

    #[test]
    fn a_shell_a_window_shows_spawns_a_fresh_one() {
        refuses_a_shell_shown_elsewhere(|stoat, hidden| {
            let ws = stoat.active_workspace_mut();
            let side = ws.panes.split(crate::pane::Axis::Vertical);
            ws.panes.pane_mut(side).view = View::Terminal(hidden);
            assert!(
                ws.panes.detach(side, 1),
                "the side pane leaves for a window"
            );
        });
    }

    /// Show `term` in a hidden dock, a surface that keeps the session alive
    /// with no pane on it.
    fn show_in_a_dock(stoat: &mut Stoat, term: TermId) {
        use crate::pane::{DockPanel, DockSide, DockVisibility};

        stoat.active_workspace_mut().docks.insert(DockPanel {
            view: View::Terminal(term),
            side: DockSide::Right,
            visibility: DockVisibility::Hidden,
            default_width: 30,
            area: Default::default(),
        });
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
    fn terminal_spawn_carries_the_session_triple() {
        let mut h = Stoat::test();
        h.stoat.set_agent_socket_dir("/state".into());

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let ws = h.stoat.active_workspace();
        let View::Terminal(term_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("focused pane should hold a terminal view");
        };
        let expected = vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("STOAT_SESSION".to_string(), ws.uid.to_string()),
            (
                "STOAT_AGENT_SOCK".to_string(),
                format!("/state/agent-{}.sock", ws.uid),
            ),
            (
                "STOAT_TERM_ID".to_string(),
                ws.terms[term_id].token.to_string(),
            ),
        ];

        let spawns = h.fake_terminal_host().spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(
            spawns[0].env, expected,
            "the shell is told which instance, socket, and terminal pane owns it",
        );
    }

    // The server task is enqueued but never polled, since it needs a live
    // reactor. The directory is a path nothing binds.
    #[test]
    fn a_terminal_spawn_serves_its_workspaces_socket() {
        let mut h = Stoat::test();
        h.stoat
            .set_agent_socket_dir("/stoat-test-never-served".into());
        h.stoat.set_serve_agent_sockets(true);
        let uid = h.stoat.active_workspace().uid();

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        assert_eq!(
            h.stoat.agent_servers.keys().collect::<Vec<_>>(),
            vec![&uid],
            "the shell's env names this socket, so something has to be on it",
        );
    }

    #[test]
    fn terminal_spawn_omits_the_session_triple_without_a_socket_dir() {
        let mut h = Stoat::test();

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let spawns = h.fake_terminal_host().spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(
            spawns[0].env,
            vec![("TERM".to_string(), "xterm-256color".to_string())],
            "no socket dir means no parent instance to name",
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

    #[test]
    fn respawn_reaches_a_terminal_in_a_parked_tab() {
        use crate::term_session::TermId;

        let mut h = Stoat::test();
        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        h.stoat.terminal_host = Arc::new(crate::host::FakeTerminalHost::new(fake));
        h.allow_host_swap();

        // The terminal sits in tab 0, which parks once a second tab opens.
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let pane = ws.panes.focus();
        ws.panes.pane_mut(pane).view = View::Terminal(TermId::default());
        ws.new_tab(&executor);

        respawn_terminal_panes(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        let parked = ws.tabs[0].parked.as_ref().expect("tab 0 parks");
        let View::Terminal(id) = parked.pane(pane).view else {
            panic!("the parked terminal pane stays a terminal");
        };
        assert_eq!(
            (
                id != TermId::default(),
                ws.terms.contains_key(id),
                ws.terms.len()
            ),
            (true, true, 1),
            "the parked pane names a fresh live session, the only one",
        );
    }
}
