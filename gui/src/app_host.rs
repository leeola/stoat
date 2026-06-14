//! Process-level owner of live editor sessions.
//!
//! A session is a workspace plus the windows showing it. The host owns the set
//! at process scope -- rather than each window's view tree owning its workspace
//! solo -- so it persists workspaces as their windows close and on app quit,
//! and gives later IPC wiring (a socket, a cwd-to-session resolver) one place
//! to look up live sessions. Stored as a [`Global`]; install it once at startup
//! before opening windows.

use crate::workspace::Workspace;
use gpui::{AnyWindowHandle, App, Entity, Global};

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
#[derive(Default)]
pub struct AppHost {
    sessions: Vec<Session>,
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
}

#[cfg(test)]
mod tests {
    use super::{AppHost, Workspace};
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{AnyWindowHandle, Entity, TestAppContext, VisualContext};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat::{
        host::{FakeFs, FsHost, FsWatchHost},
        pane::Axis,
        workspace::persist::state_path_for,
    };
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) -> Arc<dyn FsHost> {
        let fs: Arc<dyn FsHost> = Arc::new(FakeFs::new());
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(FsHostGlobal(fs.clone()));
        });
        fs
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
        let fs = install_globals(&mut cx);
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
}
