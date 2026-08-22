use crate::{
    host::{CommitFileChange, CommitInfo},
    review_session::DiffDocument,
};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use stoat_scheduler::Task;

/// Commits whose built preview a surface keeps around.
///
/// Small because only the selected commit is ever rendered. The rest are held
/// so stepping back over ground already walked is instant, which a handful
/// covers.
const PREVIEW_CACHE_CAP: usize = 8;

/// Commit-listing state owned by a [`crate::workspace::Workspace`] while
/// the user is in `"commits"` mode.
///
/// The log is virtualized: `commits` holds only the pages fetched so
/// far, and a [`CommitListState::pending_load`] task is spawned when
/// the cursor approaches the tail. Previews are likewise lazy: each
/// selected sha triggers a background build of a [`DiffDocument`] that
/// the right pane paints.
// FIXME: Commit list selection/scroll not persisted across workspace
// save/load. `commits: Vec<CommitInfo>` is fetched asynchronously on open, so
// save/restore must persist the saved selected commit's SHA (not its index),
// and on load defer scroll restoration until the initial fetch reaches a page
// containing that SHA. `pending_load` / `pending_preview` are in-flight task
// handles and are intentionally not restorable.
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
    pub preview_sessions: PreviewCache,
    pub pending_preview: Option<PendingPreview>,
    /// Last sha the user requested a preview for. Tracked so a stale
    /// pending task (if the user scrolled past) can be discarded on
    /// completion.
    pub requested_preview: Option<String>,
}

pub(crate) struct PendingPreview {
    pub sha: String,
    pub task: Task<Option<DiffDocument>>,
}

/// Built diff previews for the commits a surface's selection has rested on,
/// capped so a long history cannot grow one without bound.
///
/// A [`DiffDocument`] carries both sides' text and span vectors for every file
/// its commit touched, so these are not cheap to keep and walking a few hundred
/// commits would keep all of them. Past [`PREVIEW_CACHE_CAP`] the least
/// recently used is dropped, and rebuilt from scratch if the selection returns
/// to it.
///
/// A [`DiffDocument`] rather than anything richer, because a preview reads a
/// diff and stages nothing in it.
///
/// Shared by the commits view and the commit picker, which both build previews
/// the same way. [`PendingPreview`] is the in-flight half of it.
#[derive(Default)]
pub(crate) struct PreviewCache {
    sessions: HashMap<String, Arc<DiffDocument>>,
    /// Cached shas, least recently used first, so eviction takes the front.
    recent: VecDeque<String>,
}

impl PreviewCache {
    /// Whether `sha` is cached, marking it most recently used when it is.
    ///
    /// This is the call a preview sync makes to decide whether to build, so
    /// asking and recording are the same act and no caller can do one without
    /// the other.
    pub(crate) fn mark_used(&mut self, sha: &str) -> bool {
        if !self.sessions.contains_key(sha) {
            return false;
        }
        if let Some(at) = self.recent.iter().position(|held| held == sha) {
            self.recent.remove(at);
        }
        self.recent.push_back(sha.to_owned());
        true
    }

    /// The session built for `sha`, without counting as a use.
    ///
    /// Render reads this per frame for the selected commit alone, which would
    /// say nothing [`Self::mark_used`] has not already said for that same sha.
    pub(crate) fn get(&self, sha: &str) -> Option<&Arc<DiffDocument>> {
        self.sessions.get(sha)
    }

    /// Cache `session` as the most recently used, dropping the least recently
    /// used when that puts the cache over the cap.
    pub(crate) fn insert(&mut self, sha: String, session: Arc<DiffDocument>) {
        if let Some(at) = self.recent.iter().position(|held| *held == sha) {
            self.recent.remove(at);
        }
        self.recent.push_back(sha.clone());
        self.sessions.insert(sha, session);

        while self.recent.len() > PREVIEW_CACHE_CAP {
            if let Some(oldest) = self.recent.pop_front() {
                self.sessions.remove(&oldest);
            }
        }
    }

    /// Drop every cached session, for a walk starting over on different
    /// commits.
    ///
    /// The use order goes with them. Leaving it would corrupt nothing, since a
    /// stale name only ever evicts a session already gone, but it would leave
    /// the two halves describing different sets for any later change to trip
    /// over.
    pub(crate) fn clear(&mut self) {
        self.sessions.clear();
        self.recent.clear();
    }
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
            preview_sessions: PreviewCache::default(),
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
    use super::{PreviewCache, PREVIEW_CACHE_CAP};
    use crate::{
        app::Stoat,
        review_session::DiffDocument,
        test_harness::{CommitSpec, TestHarness},
    };
    use std::sync::Arc;

    impl PreviewCache {
        fn cache_session(&mut self, sha: &str) {
            self.insert(sha.to_owned(), Arc::new(DiffDocument::default()));
        }
    }

    /// A cache holding `count` sessions, `sha0` the least recently used.
    fn filled_cache(count: usize) -> PreviewCache {
        let mut cache = PreviewCache::default();
        for n in 0..count {
            cache.cache_session(&format!("sha{n}"));
        }
        cache
    }

    fn held(cache: &PreviewCache, shas: &[&str]) -> Vec<bool> {
        shas.iter().map(|sha| cache.get(sha).is_some()).collect()
    }

    #[test]
    fn caching_past_the_cap_drops_the_least_recently_used() {
        let mut cache = filled_cache(PREVIEW_CACHE_CAP);
        cache.cache_session("overflow");

        assert_eq!(
            held(&cache, &["sha0", "sha1", "overflow"]),
            [false, true, true],
            "the oldest goes, the rest and the newcomer stay"
        );
    }

    #[test]
    fn marking_an_older_sha_used_spares_it_from_the_next_eviction() {
        let mut cache = filled_cache(PREVIEW_CACHE_CAP);

        assert!(cache.mark_used("sha0"), "the oldest is still cached");
        cache.cache_session("overflow");

        assert_eq!(
            held(&cache, &["sha0", "sha1", "overflow"]),
            [true, false, true],
            "the sha just used is spared and the one behind it goes instead"
        );
    }

    #[test]
    fn marking_a_sha_that_was_never_cached_reports_it_missing() {
        let mut cache = filled_cache(1);
        assert_eq!(
            [cache.mark_used("sha0"), cache.mark_used("absent")],
            [true, false],
            "only a cached sha counts as a use"
        );
    }

    #[test]
    fn clearing_drops_every_session() {
        let mut cache = filled_cache(3);
        cache.clear();
        assert_eq!(
            held(&cache, &["sha0", "sha1", "sha2"]),
            [false, false, false],
            "a restarted walk previews nothing it cached for the old one"
        );
    }

    /// Three-commit linear history for the working-directory path
    /// `/repo` with the oldest commit at the bottom, matching git's
    /// top-down newest-first log ordering.
    const HISTORY: &[CommitSpec<'static>] = &[
        ("c1000001", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
        (
            "c1000002",
            "chore: tweak a",
            &[("a.rs", "fn a() {}\nfn a2() {}\n")],
        ),
        (
            "c1000003",
            "feat: add b.rs",
            &[("a.rs", "fn a() {}\nfn a2() {}\n"), ("b.rs", "fn b() {}\n")],
        ),
    ];

    #[test]
    fn snapshot_commits_open() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history("/repo", HISTORY);
        h.open_commits("/repo");
        h.assert_snapshot("commits_open");
    }

    #[test]
    fn snapshot_commits_navigate_next() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history("/repo", HISTORY);
        h.open_commits("/repo");
        h.type_keys("j");
        h.assert_snapshot("commits_navigate_next");
    }

    #[test]
    fn snapshot_commits_navigate_last() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history("/repo", HISTORY);
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
        h.seed_linear_history("/repo", HISTORY);
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

    /// Enter checks the selected commit out and shows what it changed.
    ///
    /// The commit list used to open a read-only screen over the commit, which
    /// left the files on disk somewhere else entirely. Checking out means the
    /// buffers are the commit, so the reader can move through it as code
    /// instead of as a rendering of a diff.
    #[test]
    fn enter_walks_to_the_selected_commit() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history("/repo", HISTORY);
        h.fake_fs()
            .insert_file("/repo/a.rs", b"fn a() {}\nfn a2() {}\n");
        h.fake_fs().insert_file("/repo/b.rs", b"fn b() {}\n");
        h.open_commits("/repo");
        h.type_keys("j"); // select the middle commit
        h.type_keys("Enter");
        h.settle();

        let panes = &h.stoat.active_workspace().panes;
        assert_eq!(
            (
                h.fake_git().checkouts(std::path::Path::new("/repo")),
                diff_base(&h),
                panes.pane(panes.focus()).diff_mode,
            ),
            (
                vec!["detached:c1000002".to_string()],
                Some(Some("c1000001".to_string())),
                true,
            ),
            "the tree is the commit and the diff reads against its parent",
        );
    }

    /// A tree with uncommitted work refuses, because the checkout would have to
    /// overwrite it. Nothing moves: not HEAD, not the base, not the view.
    #[test]
    fn a_dirty_tree_refuses_to_walk_from_commits() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history("/repo", HISTORY);
        h.fake_git()
            .add_repo("/repo")
            .modified("a.rs", "fn a() {}\n", "fn a() { edited }\n");
        h.open_commits("/repo");
        h.type_keys("Enter");
        h.settle();

        assert_eq!(
            (
                h.fake_git().checkouts(std::path::Path::new("/repo")),
                diff_base(&h),
                dirty_badge(&h),
            ),
            (Vec::new(), None, Some("uncommitted changes".to_string()),),
            "nothing was checked out and no base was installed",
        );
    }

    fn diff_base(h: &TestHarness) -> Option<Option<String>> {
        match h.stoat.active_workspace().diff_base() {
            Some(crate::workspace::diff::DiffBase::Rev { sha }) => Some(sha.clone()),
            _ => None,
        }
    }

    fn dirty_badge(h: &TestHarness) -> Option<String> {
        use crate::badge::BadgeSource;
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|b| b.label.clone())
    }

    #[test]
    fn navigate_caches_selected_preview() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history("/repo", HISTORY);
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
            state.preview_sessions.get(&sha).is_some(),
            "preview for selected sha must be cached after settle"
        );
        assert!(
            state.summaries.contains_key(&sha),
            "summary for selected sha must be cached"
        );
    }
}
