use crate::{
    host::{CommitFileChange, CommitInfo},
    review_session::ReviewSession,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use stoat_scheduler::Task;

/// Workspace-persistence snapshot of a [`CommitListState`].
/// Captures only the user-visible selection + scroll intent --
/// the SHA rather than the index, because the index is meaningless
/// across save / load (the commit log is paged in lazily and the
/// page containing the previously selected commit may not have
/// arrived yet at restore time).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CommitListSnapshot {
    pub selected_sha: Option<String>,
    pub scroll_top: usize,
}

/// Commit-listing state owned by a [`crate::workspace::Workspace`] while
/// the user is in `"commits"` mode.
///
/// The log is virtualized: `commits` holds only the pages fetched so
/// far, and a [`CommitListState::pending_load`] task is spawned when
/// the cursor approaches the tail. Previews are likewise lazy: each
/// selected sha triggers a background build of a [`ReviewSession`] that
/// the right pane reuses via `render_review`.
// FIXME: Commit list selection/scroll not persisted across workspace
// save/load. `commits: Vec<CommitInfo>` is fetched asynchronously on open, so
// save/restore must persist the saved selected commit's SHA (not its index),
// and on load defer scroll restoration until the initial fetch reaches a page
// containing that SHA. `pending_load` / `pending_preview` are in-flight task
// handles and are intentionally not restorable.
pub struct CommitListState {
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
    /// SHA we're trying to restore selection onto after a workspace
    /// load. Set by [`Self::apply_snapshot`] when the SHA is not yet
    /// present in [`Self::commits`]; cleared in
    /// [`Self::poll_pending_load`] the moment the SHA appears in a
    /// loaded page, or when the walk hits [`Self::reached_end`]
    /// without finding it.
    pub pending_restore_sha: Option<String>,
}

pub struct PendingPreview {
    pub sha: String,
    pub task: Task<Option<ReviewSession>>,
}

impl CommitListState {
    pub fn new(workdir: PathBuf) -> Self {
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
            pending_restore_sha: None,
        }
    }

    /// Capture the user-visible selection + scroll intent for
    /// workspace persistence. The selected commit's SHA round-trips
    /// rather than its index, because the index is meaningless
    /// across save / load.
    pub fn snapshot(&self) -> CommitListSnapshot {
        CommitListSnapshot {
            selected_sha: self.selected_sha().map(String::from),
            scroll_top: self.scroll_top,
        }
    }

    /// Restore the user's previous selection + scroll. `scroll_top`
    /// applies immediately. The SHA lookup is best-effort: when the
    /// commits paged in synchronously contain the saved SHA,
    /// selection moves there; otherwise the SHA parks in
    /// [`Self::pending_restore_sha`] and the next
    /// [`Self::poll_pending_load`] completion resolves it. Missing
    /// `selected_sha` (saved with an empty commit list) leaves
    /// selection at zero.
    pub fn apply_snapshot(&mut self, snap: CommitListSnapshot) {
        self.scroll_top = snap.scroll_top;
        let Some(sha) = snap.selected_sha else {
            return;
        };
        if let Some(idx) = self.commits.iter().position(|c| c.sha == sha) {
            self.selected = idx;
        } else {
            self.pending_restore_sha = Some(sha);
        }
    }

    pub fn selected_sha(&self) -> Option<&str> {
        self.commits.get(self.selected).map(|c| c.sha.as_str())
    }

    /// Keep `selected` within `[scroll_top, scroll_top + height)`.
    pub fn ensure_selected_visible(&mut self, height: usize) {
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
    pub fn move_down(&mut self, step: usize) -> bool {
        if self.commits.is_empty() {
            return false;
        }
        let max = self.commits.len() - 1;
        let prev = self.selected;
        self.selected = (self.selected + step).min(max);
        self.selected != prev
    }

    pub fn move_up(&mut self, step: usize) -> bool {
        let prev = self.selected;
        self.selected = self.selected.saturating_sub(step);
        self.selected != prev
    }

    pub fn move_to_first(&mut self) -> bool {
        let prev = self.selected;
        self.selected = 0;
        self.selected != prev
    }

    pub fn move_to_last(&mut self) -> bool {
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
    ///
    /// Also resolves [`Self::pending_restore_sha`] when set: if the
    /// SHA appears in the newly loaded page, selection moves to it and
    /// the pending state clears. If the walk just hit `reached_end`
    /// without finding the SHA, the pending state clears as well so
    /// future polls do not keep scanning.
    pub fn poll_pending_load(&mut self) -> bool {
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
                self.resolve_pending_restore_sha();
                true
            },
            Poll::Pending => {
                self.pending_load = Some(task);
                false
            },
        }
    }

    fn resolve_pending_restore_sha(&mut self) {
        let Some(sha) = self.pending_restore_sha.as_ref() else {
            return;
        };
        if let Some(idx) = self.commits.iter().position(|c| &c.sha == sha) {
            self.selected = idx;
            self.pending_restore_sha = None;
        } else if self.reached_end {
            self.pending_restore_sha = None;
        }
    }

    /// Poll the in-flight preview task. On completion, caches the
    /// session under its sha. Returns true when a result landed.
    pub fn poll_pending_preview(&mut self) -> bool {
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
    mod snapshot {
        use super::super::{CommitListSnapshot, CommitListState};
        use crate::host::CommitInfo;
        use std::path::PathBuf;

        fn commit(sha: &str) -> CommitInfo {
            CommitInfo {
                sha: sha.to_string(),
                short_sha: sha.chars().take(7).collect(),
                summary: format!("summary {sha}"),
                author_name: "Author".into(),
                author_email: "author@example.com".into(),
                time: 0,
                parent_count: 0,
            }
        }

        fn state_with(shas: &[&str]) -> CommitListState {
            let mut s = CommitListState::new(PathBuf::from("/repo"));
            s.commits = shas.iter().map(|sha| commit(sha)).collect();
            s
        }

        #[test]
        fn snapshot_captures_selected_sha_and_scroll() {
            let mut s = state_with(&["aaa", "bbb", "ccc"]);
            s.selected = 1;
            s.scroll_top = 5;
            let snap = s.snapshot();
            assert_eq!(snap.selected_sha.as_deref(), Some("bbb"));
            assert_eq!(snap.scroll_top, 5);
        }

        #[test]
        fn snapshot_with_empty_commits_has_no_sha() {
            let s = CommitListState::new(PathBuf::from("/repo"));
            let snap = s.snapshot();
            assert!(snap.selected_sha.is_none());
            assert_eq!(snap.scroll_top, 0);
        }

        #[test]
        fn apply_snapshot_with_loaded_sha_sets_selected_immediately() {
            let mut s = state_with(&["aaa", "bbb", "ccc"]);
            s.apply_snapshot(CommitListSnapshot {
                selected_sha: Some("ccc".into()),
                scroll_top: 3,
            });
            assert_eq!(s.selected, 2);
            assert_eq!(s.scroll_top, 3);
            assert!(s.pending_restore_sha.is_none());
        }

        #[test]
        fn apply_snapshot_with_unloaded_sha_parks_pending() {
            let mut s = state_with(&["aaa"]);
            s.apply_snapshot(CommitListSnapshot {
                selected_sha: Some("zzz".into()),
                scroll_top: 7,
            });
            assert_eq!(s.selected, 0);
            assert_eq!(s.scroll_top, 7);
            assert_eq!(s.pending_restore_sha.as_deref(), Some("zzz"));
        }

        #[test]
        fn resolve_pending_after_page_arrives_sets_selected_and_clears() {
            let mut s = state_with(&["aaa"]);
            s.apply_snapshot(CommitListSnapshot {
                selected_sha: Some("ccc".into()),
                scroll_top: 0,
            });
            s.commits.extend([commit("bbb"), commit("ccc")]);
            s.resolve_pending_restore_sha();
            assert_eq!(s.selected, 2);
            assert!(s.pending_restore_sha.is_none());
        }

        #[test]
        fn resolve_pending_clears_when_reached_end_without_match() {
            let mut s = state_with(&["aaa", "bbb"]);
            s.apply_snapshot(CommitListSnapshot {
                selected_sha: Some("zzz".into()),
                scroll_top: 0,
            });
            s.reached_end = true;
            s.resolve_pending_restore_sha();
            assert!(s.pending_restore_sha.is_none());
            assert_eq!(s.selected, 0);
        }

        #[test]
        fn apply_snapshot_with_none_sha_leaves_selection() {
            let mut s = state_with(&["aaa", "bbb"]);
            s.selected = 1;
            s.apply_snapshot(CommitListSnapshot {
                selected_sha: None,
                scroll_top: 9,
            });
            assert_eq!(s.selected, 1);
            assert_eq!(s.scroll_top, 9);
            assert!(s.pending_restore_sha.is_none());
        }
    }
}
