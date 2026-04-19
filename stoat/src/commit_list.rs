use crate::{
    host::{CommitFileChange, CommitInfo},
    review_session::ReviewSession,
};
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use stoat_scheduler::Task;

/// Commit-listing state owned by a [`crate::workspace::Workspace`] while
/// the user is in `"commits"` mode.
///
/// The log is virtualized: `commits` holds only the pages fetched so
/// far, and a [`CommitListState::pending_load`] task is spawned when
/// the cursor approaches the tail. Previews are likewise lazy: each
/// selected sha triggers a background build of a [`ReviewSession`] that
/// the right pane reuses via `render_review`.
pub(crate) struct CommitListState {
    pub workdir: PathBuf,
    pub commits: Vec<CommitInfo>,
    /// True once the backing walk hit a root commit; further
    /// `log_commits` calls with `after = last_sha` would return empty,
    /// so we stop asking.
    pub reached_end: bool,
    pub selected: usize,
    pub scroll_top: usize,
    /// Rows visible in the left list pane on the most recent render.
    /// Updated by `render_commits`; read by navigation handlers that
    /// need to keep the selection in view. Zero until first paint.
    pub viewport_rows: usize,
    pub pending_load: Option<Task<Vec<CommitInfo>>>,
    pub summaries: HashMap<String, Vec<CommitFileChange>>,
    pub preview_sessions: HashMap<String, Arc<ReviewSession>>,
    pub pending_preview: Option<PendingPreview>,
    /// Last sha the user requested a preview for. Tracked so a stale
    /// pending task (if the user scrolled past) can be discarded on
    /// completion.
    pub requested_preview: Option<String>,
}

pub(crate) struct PendingPreview {
    pub sha: String,
    pub task: Task<Option<ReviewSession>>,
}

impl CommitListState {
    pub(crate) fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            commits: Vec::new(),
            reached_end: false,
            selected: 0,
            scroll_top: 0,
            viewport_rows: 0,
            pending_load: None,
            summaries: HashMap::new(),
            preview_sessions: HashMap::new(),
            pending_preview: None,
            requested_preview: None,
        }
    }

    pub(crate) fn selected_sha(&self) -> Option<&str> {
        self.commits.get(self.selected).map(|c| c.sha.as_str())
    }

    /// Keep `selected` within `[scroll_top, scroll_top + height)`.
    pub(crate) fn ensure_selected_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        } else if self.selected >= self.scroll_top + height {
            self.scroll_top = self.selected + 1 - height;
        }
    }

    /// Move selection down by `step`, clamping at the last loaded
    /// commit. Returns true if the position changed.
    pub(crate) fn move_down(&mut self, step: usize) -> bool {
        if self.commits.is_empty() {
            return false;
        }
        let max = self.commits.len() - 1;
        let prev = self.selected;
        self.selected = (self.selected + step).min(max);
        self.selected != prev
    }

    pub(crate) fn move_up(&mut self, step: usize) -> bool {
        let prev = self.selected;
        self.selected = self.selected.saturating_sub(step);
        self.selected != prev
    }

    pub(crate) fn move_to_first(&mut self) -> bool {
        let prev = self.selected;
        self.selected = 0;
        self.selected != prev
    }

    pub(crate) fn move_to_last(&mut self) -> bool {
        if self.commits.is_empty() {
            return false;
        }
        let prev = self.selected;
        self.selected = self.commits.len() - 1;
        self.selected != prev
    }

    /// Poll the in-flight log-load task. On completion, appends results
    /// to `commits` and updates `reached_end`. Returns true when a
    /// result landed (caller should redraw).
    pub(crate) fn poll_pending_load(&mut self) -> bool {
        let Some(mut task) = self.pending_load.take() else {
            return false;
        };
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut task).poll(&mut cx) {
            Poll::Ready(page) => {
                if page.is_empty() {
                    self.reached_end = true;
                } else {
                    self.commits.extend(page);
                }
                true
            },
            Poll::Pending => {
                self.pending_load = Some(task);
                false
            },
        }
    }

    /// Poll the in-flight preview task. On completion, caches the
    /// session under its sha. Returns true when a result landed.
    pub(crate) fn poll_pending_preview(&mut self) -> bool {
        let Some(mut pending) = self.pending_preview.take() else {
            return false;
        };
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut pending.task).poll(&mut cx) {
            Poll::Ready(Some(session)) => {
                self.preview_sessions.insert(pending.sha, Arc::new(session));
                true
            },
            Poll::Ready(None) => true,
            Poll::Pending => {
                self.pending_preview = Some(pending);
                false
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::Stoat;

    /// Three-commit linear history for the working-directory path
    /// `/repo` with the oldest commit at the bottom, matching git's
    /// top-down newest-first log ordering.
    fn seed_history(h: &mut crate::test_harness::TestHarness) {
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("c1000001", "feat: add a.rs", &[("a.rs", "fn a() {}\n")])
            .commit_with_parent_message(
                "c1000002",
                "c1000001",
                "chore: tweak a",
                &[("a.rs", "fn a() {}\nfn a2() {}\n")],
            )
            .commit_with_parent_message(
                "c1000003",
                "c1000002",
                "feat: add b.rs",
                &[("a.rs", "fn a() {}\nfn a2() {}\n"), ("b.rs", "fn b() {}\n")],
            );
    }

    #[test]
    fn snapshot_commits_open() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_history(&mut h);
        h.open_commits("/repo");
        h.assert_snapshot("commits_open");
    }

    #[test]
    fn snapshot_commits_navigate_next() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_history(&mut h);
        h.open_commits("/repo");
        h.type_keys("j");
        h.assert_snapshot("commits_navigate_next");
    }

    #[test]
    fn snapshot_commits_navigate_last() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_history(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.assert_snapshot("commits_navigate_last");
    }

    #[test]
    fn snapshot_commits_empty_history() {
        let mut h = Stoat::test();
        h.resize(90, 10);
        h.fake_git().add_repo("/repo");
        h.open_commits("/repo");
        h.assert_snapshot("commits_empty_history");
    }

    #[test]
    fn open_commits_selects_head_by_default() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_history(&mut h);
        h.open_commits("/repo");
        let state = h
            .stoat
            .active_workspace()
            .commits
            .as_ref()
            .expect("commits state installed");
        assert_eq!(state.selected, 0);
        assert_eq!(
            state.commits.first().map(|c| c.sha.as_str()),
            Some("c1000003")
        );
        assert_eq!(state.commits.len(), 3);
        assert!(state.reached_end);
    }

    #[test]
    fn navigate_caches_selected_preview() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_history(&mut h);
        h.open_commits("/repo");
        h.type_keys("j");
        let state = h
            .stoat
            .active_workspace()
            .commits
            .as_ref()
            .expect("commits state");
        assert_eq!(state.selected, 1);
        let sha = state.commits[state.selected].sha.clone();
        assert!(
            state.preview_sessions.contains_key(&sha),
            "preview for selected sha must be cached after settle"
        );
        assert!(
            state.summaries.contains_key(&sha),
            "summary for selected sha must be cached"
        );
    }
}
