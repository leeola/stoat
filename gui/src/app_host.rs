//! Process-level owner of live editor sessions.
//!
//! A session is a workspace plus the windows showing it. The host owns the set
//! at process scope -- rather than each window's view tree owning its workspace
//! solo -- so it persists workspaces as their windows close and on app quit,
//! binds the app IPC socket, and resolves a client's working directory to the
//! session enclosing it. Stored as a [`Global`]; install it once at startup
//! before opening windows.

use crate::workspace::Workspace;
use gpui::{AnyWindowHandle, App, Entity, Global, Task as ForegroundTask};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::workspace::WorkspaceUid;
use stoat_agent_claude_code::jsonrpc::{self, IncomingRequest};
use stoat_scheduler::{Executor, Task};
use tokio::sync::mpsc;

/// A live editor session: one workspace and the windows presenting it.
///
/// `windows` is a set rather than a single handle so a future
/// same-session-across-monitors feature needs no shape change; today each
/// session has exactly one window.
struct Session {
    workspace: Entity<Workspace>,
    windows: Vec<AnyWindowHandle>,
}

/// A read-only snapshot of a live session for the `list_sessions` query.
///
/// `buffers` counts registered buffers, which excludes the default launch
/// scratch (it lives outside the registry), so a fresh session reports zero --
/// the same set `--buffer` can address.
pub struct SessionSummary {
    pub uid: WorkspaceUid,
    pub root: PathBuf,
    pub windows: usize,
    pub buffers: usize,
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
    /// The foreground task that runs IPC verbs on the main thread, held for
    /// the process lifetime alongside [`Self::_ipc`].
    _dispatch: Option<ForegroundTask<()>>,
}

impl Global for AppHost {}

impl AppHost {
    /// Register a new single-window session for `workspace`, bumping its uid if
    /// it collides with an already-registered session.
    pub fn add_session(
        &mut self,
        workspace: Entity<Workspace>,
        window: AnyWindowHandle,
        cx: &mut App,
    ) {
        let current = workspace.read(cx).uid();
        let unique = self.unique_uid(current, cx);
        if unique != current {
            workspace.update(cx, |workspace, _| workspace.set_uid(unique));
        }
        self.sessions.push(Session {
            workspace,
            windows: vec![window],
        });
    }

    /// The first uid at or above `uid` not held by a live session, incrementing
    /// on collision. `WorkspaceUid::now` is a coarse-clock timestamp that can
    /// repeat across workspaces made in the same instant, so registration walks
    /// past any duplicate to keep live-session ids unique for id-addressing.
    fn unique_uid(&self, mut uid: WorkspaceUid, cx: &App) -> WorkspaceUid {
        while self
            .sessions
            .iter()
            .any(|session| session.workspace.read(cx).uid() == uid)
        {
            uid = WorkspaceUid(uid.0.wrapping_add(1));
        }
        uid
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

    /// The workspace of the live session with `uid`, if any.
    ///
    /// Returns the first match; the cwd resolver collapses a root to a single
    /// uid, so this is the session it selected.
    pub fn session_workspace(&self, uid: WorkspaceUid, cx: &App) -> Option<Entity<Workspace>> {
        self.sessions
            .iter()
            .find(|session| session.workspace.read(cx).uid() == uid)
            .map(|session| session.workspace.clone())
    }

    /// Summarize every live session: uid, root directory, and window and
    /// registered-buffer counts. Backs the `list_sessions` IPC verb.
    pub fn session_summaries(&self, cx: &App) -> Vec<SessionSummary> {
        self.sessions
            .iter()
            .map(|session| {
                let workspace = session.workspace.read(cx);
                SessionSummary {
                    uid: workspace.uid(),
                    root: workspace.git_root().clone(),
                    windows: session.windows.len(),
                    buffers: workspace.buffer_registry().read(cx).len(),
                }
            })
            .collect()
    }

    /// Bind the process IPC socket and start accepting clients, holding the
    /// accept loop and the foreground verb-dispatch task for the host's
    /// lifetime.
    ///
    /// Accepted requests are forwarded to a foreground task (see
    /// [`crate::ipc::spawn_dispatch`]) that runs each verb on the main thread.
    /// Binding failures are logged and swallowed rather than aborting startup:
    /// a live socket held by another Stoat instance, or an unresolvable runtime
    /// directory, must not stop this window from opening.
    pub fn serve(&mut self, executor: &Executor, cx: &mut App) {
        let path = match stoat_log::app_socket_path() {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!(%err, "resolving the app socket path failed; IPC disabled");
                return;
            },
        };
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let dispatch = crate::ipc::spawn_dispatch(cx, request_rx);
        let handler = Arc::new(move |request: IncomingRequest| {
            let _ = request_tx.send(request);
        });
        match jsonrpc::serve_unix(&path, executor, handler) {
            Ok(accept) => {
                self._ipc = Some(accept);
                self._dispatch = Some(dispatch);
            },
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
        cx.update(|app| {
            host.add_session(ws_a, win_a, app);
            host.add_session(ws_b, win_b, app);
        });
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
        cx.update(|app| {
            host.add_session(ws_repo, win_repo, app);
            host.add_session(ws_sub, win_sub, app);
        });

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
        cx.update(|app| host.add_session(ws_repo2, win_repo2, app));

        let tiebreak = cx.update(|cx| host.resolve_cwd(Path::new("/repo/x"), cx));
        assert_eq!(
            tiebreak,
            Some(uid_repo2),
            "highest uid wins among sessions sharing the nearest root",
        );
    }

    #[test]
    fn add_session_bumps_a_colliding_uid() {
        let mut cx = TestAppContext::single();
        let (_fs, _scheduler) = install_globals(&mut cx);

        let (ws_a, win_a) = dirty_workspace_window(&mut cx, "a", Path::new("/repo"));
        let (ws_b, win_b) = dirty_workspace_window(&mut cx, "b", Path::new("/repo"));
        let uid_a = ws_a.read_with(&cx, |w, _| w.uid());
        let uid_b = ws_b.read_with(&cx, |w, _| w.uid());
        assert_eq!(
            uid_a, uid_b,
            "the fixed test clock makes both uids identical"
        );

        let mut host = AppHost::default();
        cx.update(|app| {
            host.add_session(ws_a, win_a, app);
            host.add_session(ws_b, win_b, app);
        });

        let uids = cx.update(|app| {
            host.sessions
                .iter()
                .map(|session| session.workspace.read(app).uid())
                .collect::<Vec<_>>()
        });
        assert_eq!(uids[0], uid_a, "the first session keeps its uid");
        assert_ne!(
            uids[1], uids[0],
            "the second registration bumped the colliding uid",
        );
    }
}
