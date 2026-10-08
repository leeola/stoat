use crate::{
    host::{CommitFileChange, CommitInfo, GitRepo},
    pane::View,
    review_session::DiffDocument,
};
use serde::{Deserialize, Serialize};
use slotmap::new_key_type;
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

new_key_type! {
    /// Workspace-scoped key for a [`CommitListState`] in the workspace's list
    /// collection, which a [`View::Commits`] pane names.
    pub struct CommitListId;
}

/// One commits list, which a pane shows through [`View::Commits`] and a
/// [`crate::workspace::Workspace`] holds in its list collection.
///
/// Held by the workspace rather than by the pane, as a terminal session is, so
/// a parked tab's list keeps its pages and its selection while the tab is out
/// of sight.
///
/// The log is virtualized: `commits` holds only the pages fetched so
/// far, and a [`CommitListState::pending_load`] task is spawned when
/// the cursor approaches the tail. Previews are likewise lazy: each
/// selected sha triggers a background build of a [`DiffDocument`] that
/// the right pane paints.
///
/// A saved session keeps a list as a [`CommitListSnap`], the reader's place
/// without the pages, which reload.
pub(crate) struct CommitListState {
    pub workdir: PathBuf,
    /// The repository the list walks, held rather than rediscovered.
    ///
    /// Every paging and preview handler needs it, and discovery walks the
    /// filesystem upward for a `.git`. The screen cannot outlive the workdir it
    /// opened over, so one handle answers for the life of the list.
    pub repo: Arc<dyn GitRepo>,
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
    /// First preview row shown in the detail pane, in the rows
    /// [`crate::render::commits::preview_row_count`] counts.
    ///
    /// Clamped by the render, where the diff's row count and the pane's height
    /// are both known. Reset to zero whenever the selection moves, because the
    /// offset belongs to the diff under it rather than to the pane.
    pub preview_scroll: usize,
    /// The input mode while the list's pane is focused, which the keymap's
    /// `mode == normal` guard on the commits bindings reads.
    ///
    /// The list's own, as a terminal session holds its own, so a chord left
    /// pending on another pane never carries into the list.
    pub mode: String,
    /// The view the list replaced in its pane, which closing the list puts
    /// back.
    pub covered: Option<View>,
    /// The saved selection a restored list moves onto once a loaded page
    /// holds its commit.
    ///
    /// The next page keeps loading while it waits, and the walk's end drops it
    /// when the commit is gone from the history.
    pub pending_selection: Option<SavedSelection>,
}

pub(crate) struct PendingPreview {
    pub sha: String,
    pub task: Task<PreviewLoad>,
}

/// Where a list's selection stood when its session was saved.
///
/// The commit is a sha rather than an index, because new commits on top shift
/// the indices, and a restored list loads its pages in the background.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedSelection {
    pub sha: String,
    /// The selection's row below the top of the list, so the commit comes
    /// back where it stood on screen.
    pub row: usize,
}

/// What a saved session keeps of one commits list, keyed in the session file
/// by the id its pane names.
///
/// The pages and the previews reload from the repository, so only the reader's
/// place survives.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct CommitListSnap {
    pub selection: Option<SavedSelection>,
    /// The view the list covered. Only an editor, an image, or a label is
    /// kept, because any other view names a live session the restore does not
    /// bring back, and a fresh session gives its id to an unrelated one.
    pub covered: Option<View>,
}

/// What one background preview build produces.
///
/// Both halves come from the same task because both read the same commit, and
/// a caller that reads one on the run loop pays for the tree walk there.
pub(crate) struct PreviewLoad {
    /// The commit's file-change summary, or `None` from a surface that paints
    /// none. Distinct from an empty list, which is a commit that changed
    /// nothing.
    pub summary: Option<Vec<CommitFileChange>>,
    /// `None` when the commit yields no diff to show, which is a final answer
    /// rather than a failure to retry.
    pub document: Option<DiffDocument>,
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
    sessions: HashMap<String, Option<Arc<DiffDocument>>>,
    /// Cached shas, least recently used first, so eviction takes the front.
    recent: VecDeque<String>,
}

/// What [`PreviewCache`] knows about one commit's preview.
pub(crate) enum Preview<'a> {
    /// No build has finished for this commit, so a caller paints "loading".
    Unbuilt,
    /// A build finished and the commit changed nothing.
    ///
    /// Distinct from [`Self::Unbuilt`] because it is a final answer. A caller
    /// that treats it as "still loading" waits forever, and a pump that does
    /// so asks for the same build on every pass.
    Empty,
    /// The built diff.
    Built(&'a Arc<DiffDocument>),
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

    /// What is known about `sha`'s preview, without counting as a use.
    ///
    /// Render reads this per frame for the selected commit alone, which would
    /// say nothing [`Self::mark_used`] has not already said for that same sha.
    pub(crate) fn get(&self, sha: &str) -> Preview<'_> {
        match self.sessions.get(sha) {
            None => Preview::Unbuilt,
            Some(None) => Preview::Empty,
            Some(Some(document)) => Preview::Built(document),
        }
    }

    /// Cache a built `document` for `sha` as the most recently used, dropping
    /// the least recently used when that puts the cache over the cap.
    pub(crate) fn insert(&mut self, sha: String, document: Arc<DiffDocument>) {
        self.record(sha, Some(document));
    }

    /// Record that `sha` was built and produced no diff.
    ///
    /// Held like any other answer, and evicted like one, so a commit that
    /// changed nothing is asked about once rather than on every pump pass.
    pub(crate) fn insert_empty(&mut self, sha: String) {
        self.record(sha, None);
    }

    fn record(&mut self, sha: String, document: Option<Arc<DiffDocument>>) {
        if let Some(at) = self.recent.iter().position(|held| *held == sha) {
            self.recent.remove(at);
        }
        self.recent.push_back(sha.clone());
        self.sessions.insert(sha, document);

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
    pub(crate) fn new(workdir: PathBuf, repo: Arc<dyn GitRepo>) -> Self {
        Self {
            workdir,
            repo,
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
            preview_scroll: 0,
            mode: "normal".to_string(),
            covered: None,
            pending_selection: None,
        }
    }

    /// The list's place as a saved session keeps it.
    ///
    /// A selection still waiting on its page is the place, since the reader
    /// has not left it.
    pub(crate) fn snapshot(&self) -> CommitListSnap {
        CommitListSnap {
            selection: self.pending_selection.clone().or_else(|| {
                self.selected_sha().map(|sha| SavedSelection {
                    sha: sha.to_string(),
                    row: self.selected.saturating_sub(self.scroll_top),
                })
            }),
            covered: self.covered.clone().filter(|view| {
                matches!(view, View::Editor(_) | View::Image { .. } | View::Label(_))
            }),
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

    /// Move selection to `index`, clamping at the last loaded commit.
    /// Returns true if the position changed.
    pub(crate) fn move_to(&mut self, index: usize) -> bool {
        if self.commits.is_empty() {
            return false;
        }
        let prev = self.selected;
        self.selected = index.min(self.commits.len() - 1);
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
    /// result landed, which tells the caller to redraw.
    ///
    /// A landed page that holds the pending selection's commit moves the
    /// selection onto it.
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
                self.resolve_pending_selection();
                true
            },
            Poll::Pending => {
                self.pending_load = Some(task);
                false
            },
        }
    }

    /// Select the pending selection's commit on the row it stood on, once the
    /// loaded pages hold it.
    ///
    /// The wait ends without a move when the walk reached its end, so a commit
    /// gone from the history leaves the selection where it is.
    fn resolve_pending_selection(&mut self) {
        let Some(pending) = &self.pending_selection else {
            return;
        };
        if let Some(index) = self.commits.iter().position(|c| c.sha == pending.sha) {
            self.selected = index;
            self.scroll_top = index.saturating_sub(pending.row);
        } else if !self.reached_end {
            return;
        }
        self.pending_selection = None;
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
            Poll::Ready(load) => {
                if let Some(summary) = load.summary {
                    self.summaries.insert(pending.sha.clone(), summary);
                }
                match load.document {
                    Some(document) => self
                        .preview_sessions
                        .insert(pending.sha, Arc::new(document)),
                    None => self.preview_sessions.insert_empty(pending.sha),
                }
                true
            },
            Poll::Pending => {
                self.pending_preview = Some(pending);
                false
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Preview, PreviewCache, SavedSelection, PREVIEW_CACHE_CAP};
    use crate::{
        app::Stoat,
        pane::View,
        review_session::DiffDocument,
        term_session::TermId,
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
        shas.iter()
            .map(|sha| matches!(cache.get(sha), Preview::Built(_)))
            .collect()
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
    fn a_snapshot_keeps_a_waiting_selection_and_drops_a_session_view() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history("/repo", HISTORY);
        h.open_commits("/repo");
        let waiting = SavedSelection {
            sha: "c1000001".into(),
            row: 2,
        };
        let state = h
            .stoat
            .active_workspace_mut()
            .focused_commits_mut()
            .expect("commits state");
        state.pending_selection = Some(waiting.clone());
        state.covered = Some(View::Terminal(TermId::default()));

        let snap = state.snapshot();

        assert_eq!(
            (snap.selection, snap.covered.is_none()),
            (Some(waiting), true),
            "the snapshot keeps the place a restore still waits on, and no shell id"
        );
    }

    /// A saved commit the history no longer holds loads every page once and
    /// then stops waiting, so a later reload does not walk the history again.
    #[test]
    fn a_saved_selection_gone_from_the_history_stops_waiting_at_the_end() {
        let mut h = Stoat::test();
        h.resize(90, 16);
        h.seed_linear_history("/repo", HISTORY);
        h.open_commits("/repo");
        h.stoat
            .active_workspace_mut()
            .focused_commits_mut()
            .expect("commits state")
            .pending_selection = Some(SavedSelection {
            sha: "c9999999".into(),
            row: 0,
        });

        h.type_keys("r");
        h.settle();

        let state = h
            .stoat
            .active_workspace()
            .focused_commits()
            .expect("commits state");
        assert_eq!(
            (
                state.pending_selection.clone(),
                state.selected,
                state.reached_end
            ),
            (None, 0, true),
            "the walk reaches its end without the commit and drops the wait"
        );
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
            .focused_commits()
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
            Some(crate::workspace::diff::DiffBase::Rev { sha, .. }) => Some(sha.clone()),
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
            .focused_commits()
            .expect("commits state");
        assert_eq!(state.selected, 1);
        let sha = state.commits[state.selected].sha.clone();
        assert!(
            matches!(state.preview_sessions.get(&sha), Preview::Built(_)),
            "preview for selected sha must be cached after settle"
        );
        assert!(
            state.summaries.contains_key(&sha),
            "summary for selected sha must be cached"
        );
    }
}
