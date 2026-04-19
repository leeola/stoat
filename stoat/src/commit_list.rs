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
    use crate::{app::Stoat, host::GitHost};

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
    fn snapshot_commits_open_review_readonly() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_history(&mut h);
        h.open_commits("/repo");
        h.type_keys("o");
        h.assert_snapshot("commits_open_review_readonly");
    }

    #[test]
    fn close_review_from_commits_returns_to_commits_mode() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_history(&mut h);
        h.open_commits("/repo");
        h.type_keys("j"); // select second commit
        h.type_keys("o"); // open review of it
        assert_eq!(h.stoat.mode, "review");
        let session_sha = match h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .map(|s| s.source.clone())
        {
            Some(crate::review_session::ReviewSource::Commit { sha, .. }) => sha,
            other => panic!("expected Commit source, got {other:?}"),
        };
        assert_eq!(session_sha, "c1000002");
        h.type_keys("q"); // close review
        assert_eq!(h.stoat.mode, "commits");
        let state = h
            .stoat
            .active_workspace()
            .commits
            .as_ref()
            .expect("commits state preserved");
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn review_remove_selected_on_head_amends() {
        use crate::review_session::ChunkStatus;

        let mut h = Stoat::test();
        h.resize(90, 16);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("parent", "prev", &[("a.rs", "one\ntwo\nthree\n")])
            .commit_with_parent_message(
                "head",
                "parent",
                "feat: drop this hunk",
                &[("a.rs", "one\ntwo_NEW\nthree\n")],
            );
        h.open_commits("/repo");
        h.type_keys("o");
        assert_eq!(h.stoat.mode, "review");

        // Stage the only chunk then dispatch removal.
        h.set_review_status(0, ChunkStatus::Staged);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRemoveSelected);

        let amends = h.fake_git().amend_history(std::path::Path::new("/repo"));
        assert_eq!(amends.len(), 1, "one amend recorded");
        assert_eq!(amends[0].old_head, "head");
        assert_eq!(amends[0].new_head, "amended-head-1");

        // The rewritten commit now matches its parent exactly (the only
        // modification was reverted), so scan_commit produces no
        // session and `close_review` bounces us back to commits mode.
        assert!(h.stoat.active_workspace().review.is_none());
        assert_eq!(h.stoat.mode, "commits");
    }

    #[test]
    fn review_remove_selected_on_non_head_rewrites_chain() {
        use crate::review_session::ChunkStatus;

        let mut h = Stoat::test();
        h.resize(90, 16);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("c1", "init", &[("a.rs", "line1\n")])
            .commit_with_parent_message("c2", "c1", "middle", &[("a.rs", "line1\nM\n")])
            .commit_with_parent_message("c3", "c2", "tip", &[("a.rs", "line1\nM\nN\n")]);
        h.open_commits("/repo");
        h.type_keys("j"); // move to "middle"
        assert_eq!(
            h.stoat.active_workspace().commits.as_ref().unwrap().commits[1].sha,
            "c2"
        );
        h.type_keys("o");
        h.set_review_status(0, ChunkStatus::Staged);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRemoveSelected);

        // rewrite_commit produced a new tip (the fake mints deterministic
        // shas). HEAD should now be a rewritten descendant.
        let commits_state = h.stoat.active_workspace().commits.as_ref();
        let top_sha = commits_state.and_then(|s| s.commits.first().map(|c| c.sha.clone()));
        // After rewrite, commit_list isn't refreshed automatically; the
        // repo's HEAD sha has changed though.
        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        assert_eq!(
            log.len(),
            3,
            "three commits remain (middle rewritten, tip cherry-picked)"
        );
        assert!(
            log[0].sha.starts_with("rewritten-c3"),
            "tip rewritten: {}",
            log[0].sha
        );
        assert!(
            log[1].sha.starts_with("rewritten-c2"),
            "middle rewritten: {}",
            log[1].sha
        );
        assert_eq!(log[2].sha, "c1", "root unchanged");
        drop(top_sha);
    }

    #[test]
    fn review_remove_selected_non_head_conflict_aborts() {
        use crate::review_session::ChunkStatus;

        let mut h = Stoat::test();
        h.resize(90, 16);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("c1", "init", &[("a.rs", "line1\n")])
            .commit_with_parent_message("c2", "c1", "middle", &[("a.rs", "line1\nM\n")])
            .commit_with_parent_message("c3", "c2", "tip", &[("a.rs", "line1\nM\nN\n")])
            .simulate_conflict_at("c3");

        h.open_commits("/repo");
        h.type_keys("j"); // select "c2"
        h.type_keys("o");
        h.set_review_status(0, ChunkStatus::Staged);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRemoveSelected);

        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(crate::badge::BadgeSource::Review)
            .expect("error badge");
        let badge = ws.badges.get(badge_id).unwrap();
        assert_eq!(badge.state, crate::badge::BadgeState::Error);
        assert!(
            badge.label.to_lowercase().contains("rewrite"),
            "badge mentions rewrite: {}",
            badge.label
        );

        // Original history intact; no new commits were published.
        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        let shas: Vec<_> = log.iter().map(|c| c.sha.clone()).collect();
        assert_eq!(shas, vec!["c3".to_string(), "c2".into(), "c1".into()]);
    }

    #[test]
    fn review_remove_selected_dirty_worktree_refuses() {
        use crate::review_session::ChunkStatus;

        let mut h = Stoat::test();
        h.resize(90, 16);
        let workdir = std::path::PathBuf::from("/repo");
        h.fake_git
            .add_repo(workdir.clone())
            .commit_with_message("parent", "prev", &[("a.rs", "base\n")])
            .commit_with_parent_message("head", "parent", "tip", &[("a.rs", "base\nnew\n")]);
        // Mark a file dirty so the dirty-worktree guard triggers.
        h.fake_git
            .add_repo(workdir.clone())
            .unstaged_file("a.rs", "something else\n");
        h.stoat.active_workspace_mut().git_root = workdir.clone();

        h.open_commits(workdir.clone());
        h.type_keys("o");
        h.set_review_status(0, ChunkStatus::Staged);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRemoveSelected);

        let amends = h.fake_git().amend_history(&workdir);
        assert!(amends.is_empty(), "dirty worktree must refuse the amend");
        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(crate::badge::BadgeSource::Review)
            .expect("error badge visible");
        let badge = ws.badges.get(badge_id).unwrap();
        assert_eq!(badge.state, crate::badge::BadgeState::Error);
        assert!(badge.label.to_lowercase().contains("dirty"));
    }

    #[test]
    fn review_remove_selected_nothing_staged_badges_info() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.fake_git
            .add_repo("/repo")
            .commit_with_message("head", "only", &[("a.rs", "only\n")]);
        h.open_commits("/repo");
        h.type_keys("o");
        // No status change; dispatch anyway.
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRemoveSelected);
        assert!(h
            .fake_git
            .amend_history(std::path::Path::new("/repo"))
            .is_empty());
        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(crate::badge::BadgeSource::Review)
            .expect("info badge visible");
        let badge = ws.badges.get(badge_id).unwrap();
        assert!(badge.label.contains("nothing"));
    }

    #[test]
    fn review_apply_staged_is_noop_for_commit_source() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_history(&mut h);
        h.open_commits("/repo");
        h.type_keys("o");
        assert_eq!(h.stoat.mode, "review");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewApplyStaged);
        let patches = h.fake_git().applied_patches(std::path::Path::new("/repo"));
        assert!(
            patches.is_empty(),
            "commit-source review must not apply any patches to the index"
        );
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
