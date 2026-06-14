//! Process-level owner of live editor sessions.
//!
//! A session is a workspace plus the windows showing it. The host owns the set
//! at process scope -- rather than each window's view tree owning its workspace
//! solo -- so it persists workspaces as their windows close and on app quit,
//! binds the app IPC socket, and resolves a client's working directory to the
//! session enclosing it. Stored as a [`Global`]; install it once at startup
//! before opening windows.

use crate::workspace::Workspace;
use gpui::{AnyWindowHandle, App, Entity, Global};
use std::path::Path;
use stoat::workspace::WorkspaceUid;
use stoat_agent_claude_code::jsonrpc;
use stoat_scheduler::{Executor, Task};

/// A live editor session: one workspace and the windows presenting it.
///
/// `windows` is a set rather than a single handle so a future
/// same-session-across-monitors feature needs no shape change; today each
/// session has exactly one window.
struct Session {
    workspace: Entity<Workspace>,
    windows: Vec<AnyWindowHandle>,
}

/// The process-level registry of live sessions.
///
/// Holds every session keyed by its uid and root directory, and resolves a
/// client's working directory to the nearest enclosing session (see
/// [`Self::resolve_cwd`]) -- the routing the IPC verbs build on.
#[derive(Default)]
pub struct AppHost {
    sessions: Vec<Session>,
    /// The IPC accept loop, held so it runs for the process lifetime.
    /// `None` until [`AppHost::serve`] binds the socket, and when binding
    /// fails (e.g. another live instance already holds it).
    _ipc: Option<Task<()>>,
}

impl Global for AppHost {}

impl AppHost {
    /// Register a new single-window session for `workspace`.
    pub fn add_session(&mut self, workspace: Entity<Workspace>, window: AnyWindowHandle) {
        self.sessions.push(Session {
            workspace,
            windows: vec![window],
        });
    }

    /// Resolve the live session whose root directory is the nearest ancestor
    /// of `cwd` (cwd itself counts), returning its [`WorkspaceUid`]. `None`
    /// when no live session is rooted at any ancestor.
    ///
    /// This is the live-session counterpart to the on-disk ancestor walk that
    /// backs `--continue`: a client's working directory routes to the most
    /// specific session enclosing it. Among sessions sharing the nearest root,
    /// the highest uid wins, so the result is deterministic even when two
    /// sessions collide on a coarse-clock [`WorkspaceUid`].
    // First caller lands with the open_file IPC verb.
    #[allow(dead_code)]
    pub fn resolve_cwd(&self, cwd: &Path, cx: &App) -> Option<WorkspaceUid> {
        for ancestor in cwd.ancestors() {
            let nearest = self
                .sessions
                .iter()
                .filter_map(|session| {
                    let workspace = session.workspace.read(cx);
                    (workspace.git_root().as_path() == ancestor).then(|| workspace.uid())
                })
                .max_by_key(|uid| uid.0);
            if nearest.is_some() {
                return nearest;
            }
        }
        None
    }

    /// Save and drop every session whose windows have all closed, keyed off the
    /// set of windows still open.
    ///
    /// The host is the sole owner of a session's workspace, so it flushes the
    /// workspace to disk as the session's last window closes -- there is no
    /// per-window release observer left to do it. Sessions that still have an
    /// open window are left untouched.
    pub fn prune_closed(&mut self, open: &[AnyWindowHandle], cx: &App) {
        for session in &mut self.sessions {
            session.windows.retain(|window| open.contains(window));
        }
        self.sessions.retain(|session| {
            let keep = !session.windows.is_empty();
            if !keep {
                session.workspace.read(cx).save_state_to_default_path(cx);
            }
            keep
        });
    }

    /// Persist every live session's workspace. Runs on app quit to flush the
    /// sessions that still have an open window; ones whose windows closed
    /// earlier were already saved by [`Self::prune_closed`].
    pub fn save_all(&self, cx: &App) {
        for session in &self.sessions {
            session.workspace.read(cx).save_state_to_default_path(cx);
        }
    }

    /// Bind the process IPC socket and start accepting clients, holding the
    /// accept loop for the host's lifetime.
    ///
    /// Binding failures are logged and swallowed rather than aborting
    /// startup: a live socket held by another Stoat instance, or an
    /// unresolvable runtime directory, must not stop this window from opening.
    pub fn serve(&mut self, executor: &Executor) {
        let path = match stoat_log::app_socket_path() {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!(%err, "resolving the app socket path failed; IPC disabled");
                return;
            },
        };
        match jsonrpc::serve_unix(&path, executor) {
            Ok(task) => self._ipc = Some(task),
            Err(err) => {
                tracing::warn!(%err, ?path, "binding the app socket failed; IPC disabled")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppHost, Workspace};
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{AnyWindowHandle, Entity, TestAppContext, VisualContext};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    use stoat::{
        host::{FakeFs, FsHost, FsWatchHost},
        pane::Axis,
        workspace::persist::state_path_for,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) -> (Arc<dyn FsHost>, Arc<TestScheduler>) {
        let scheduler = Arc::new(TestScheduler::new());
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(scheduler.clone())));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(FsHostGlobal(fs.clone()));
        });
        (fs, scheduler)
    }

    /// A window whose workspace has had a pane split, so it is no longer fresh
    /// and therefore produces a state file when saved.
    fn dirty_workspace_window(
        cx: &mut TestAppContext,
        name: &str,
        anchor: &Path,
    ) -> (Entity<Workspace>, AnyWindowHandle) {
        let name = name.to_string();
        let anchor = anchor.to_path_buf();
        let (workspace, vcx) = cx.add_window_view(|_window, cx| Workspace::new(name, anchor, cx));
        let handle = vcx.window_handle();
        workspace.update(cx, |w, cx| {
            w.pane_tree()
                .clone()
                .update(cx, |tree, cx| tree.split(Axis::Vertical, cx));
        });
        (workspace, handle)
    }

    #[test]
    fn prune_saves_and_drops_sessions_whose_last_window_closed() {
        let mut cx = TestAppContext::single();
        let (fs, _scheduler) = install_globals(&mut cx);
        let repo = PathBuf::from("/repo");

        let (ws_a, win_a) = dirty_workspace_window(&mut cx, "a", &repo);
        let (ws_b, win_b) = dirty_workspace_window(&mut cx, "b", &repo);
        let path_a = {
            let uid = ws_a.read_with(&cx, |w, _| w.uid());
            state_path_for(&repo, uid, &*fs).expect("state path")
        };

        let mut host = AppHost::default();
        host.add_session(ws_a, win_a);
        host.add_session(ws_b, win_b);
        assert_eq!(host.sessions.len(), 2);
        assert!(!FsHost::exists(&*fs, &path_a));

        cx.update(|app| host.prune_closed(&[win_b], app));
        assert_eq!(
            host.sessions.len(),
            1,
            "only the still-open window survives"
        );
        assert!(
            FsHost::exists(&*fs, &path_a),
            "closing a's last window persists its state",
        );

        cx.update(|app| host.prune_closed(&[], app));
        assert!(
            host.sessions.is_empty(),
            "no open windows leaves no sessions"
        );
    }

    #[test]
    fn resolve_cwd_returns_nearest_ancestor_rooted_session() {
        let mut cx = TestAppContext::single();
        let (_fs, scheduler) = install_globals(&mut cx);

        let (ws_repo, win_repo) = dirty_workspace_window(&mut cx, "repo", Path::new("/repo"));
        scheduler.advance_clock(Duration::from_millis(1));
        let (ws_sub, win_sub) = dirty_workspace_window(&mut cx, "sub", Path::new("/repo/sub"));
        let uid_repo = ws_repo.read_with(&cx, |w, _| w.uid());
        let uid_sub = ws_sub.read_with(&cx, |w, _| w.uid());
        assert_ne!(uid_repo, uid_sub, "advancing the clock gives distinct uids");

        let mut host = AppHost::default();
        host.add_session(ws_repo, win_repo);
        host.add_session(ws_sub, win_sub);

        let (deep, sibling, exact, outside) = cx.update(|cx| {
            (
                host.resolve_cwd(Path::new("/repo/sub/deep"), cx),
                host.resolve_cwd(Path::new("/repo/other"), cx),
                host.resolve_cwd(Path::new("/repo"), cx),
                host.resolve_cwd(Path::new("/outside"), cx),
            )
        });
        assert_eq!(deep, Some(uid_sub), "deepest enclosing root wins");
        assert_eq!(sibling, Some(uid_repo), "falls back to the shallower root");
        assert_eq!(exact, Some(uid_repo), "exact root");
        assert_eq!(outside, None, "no session roots at any ancestor");

        scheduler.advance_clock(Duration::from_millis(1));
        let (ws_repo2, win_repo2) = dirty_workspace_window(&mut cx, "repo2", Path::new("/repo"));
        let uid_repo2 = ws_repo2.read_with(&cx, |w, _| w.uid());
        assert!(
            uid_repo2.0 > uid_repo.0,
            "later workspace has the higher uid"
        );
        host.add_session(ws_repo2, win_repo2);

        let tiebreak = cx.update(|cx| host.resolve_cwd(Path::new("/repo/x"), cx));
        assert_eq!(
            tiebreak,
            Some(uid_repo2),
            "highest uid wins among sessions sharing the nearest root",
        );
    }
}
