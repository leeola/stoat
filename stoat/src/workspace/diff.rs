//! Building a buffer's diff against git, and the state that keeps it fresh.
//!
//! A diff map is what the gutter marks, the diff view, and the change motions
//! all read. Producing one takes the repo mutex, decompresses the HEAD and index
//! blobs, and diffs the whole file, so none of it belongs on the run loop and
//! none of it should happen twice for the same text. What lives here is that
//! pipeline: the compute functions the blocking jobs run, the two memos that
//! keep a keystroke from re-reading blobs or re-parsing a base text, and the
//! per-buffer bookkeeping that decides when a recompute is owed.
//!
//! [`Workspace`](super::Workspace) keeps the driving loop, since scheduling a
//! diff needs the buffer registry and the pane layout as well as this state.

use super::Workspace;
use crate::{
    action_handlers::movement::ChangeDir,
    buffer::{BufferId, TextBufferSnapshot},
    buffer_registry::{self, BufferRegistry},
    code_index::build::file_id,
    diff::{self, ReviewFileInput},
    diff_cache::ContentHash,
    diff_map::{
        changes_to_hunks, line_starts, mark_staged, merge_structural_detail, staged_marks,
        BaseHighlights, DiffHunk, DiffHunkStatus, DiffMap,
    },
    display_map::{highlights::HighlightStyle, syntax_theme::SyntaxStyles},
    editor_state::EditorId,
    host::{git::HunkTallies, FsHost, GitHost, GitRepo},
    jumplist::JumpEntry,
    multi_buffer::MultiBuffer,
    syntax_parse,
};
use codegraph::FileId;
use std::{
    cmp::{Ordering, Reverse},
    collections::{HashMap, VecDeque},
    future::Future,
    mem,
    ops::Range,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use stoat_language::{
    structural_diff::{self, MatchedPairs, TreeCache},
    HighlightId, Language, LanguageRegistry, SyntaxMap, Tree,
};
use stoat_scheduler::{Executor, Task};
use stoat_text::Rope;
use tokio::sync::Notify;

/// How long a buffer must hold one version before its diff is recomputed.
///
/// A diff walks the whole file on a blocking thread and the next keystroke
/// invalidates it, so a typing burst is worth one diff at the end rather than
/// one per edit. Short enough that a reader pausing mid-thought still sees the
/// gutter catch up.
pub(crate) const DIFF_SETTLE: Duration = Duration::from_millis(250);

/// Per-file diff memo, keyed by graph [`FileId`], holding the base and buffer
/// content hashes the ranges were measured from alongside the ranges.
///
/// Both hashes still matching is what lets a scan reuse an entry rather than
/// diff the file again.
pub(crate) type ChangedRangesMemo = HashMap<FileId, (ContentHash, ContentHash, Vec<Range<usize>>)>;

/// A working-tree diff scan, ready to install on the workspace it ran for.
///
/// [`scan_changed_ranges`] produces one off the run loop, so what it learned has
/// to travel back as data rather than as a mutation.
#[derive(Default)]
pub(crate) struct ChangedRangesScan {
    /// The changed byte ranges per file, which becomes
    /// [`Workspace::changed_ranges`] whole.
    pub(super) ranges: HashMap<FileId, Vec<Range<usize>>>,
    /// The files the scan had to diff, with the hashes it diffed them at. The
    /// memo takes these on, so a scan finding the same texts reuses them.
    pub(super) computed: Vec<(FileId, ContentHash, ContentHash, Vec<Range<usize>>)>,
}

/// What a workspace's buffers diff against, when that is not the index.
///
/// With no displacing base the working tree diffs against HEAD, with the
/// changes the index holds as staged marks, or against the index when the
/// reader picks it. Reviewing a commit checks it out and points the diff at the
/// commit's parent, so the base moves off HEAD while the buffers stay the
/// working tree. An agent's proposed edits have no commit behind them at all,
/// so they carry their base as text. A pair of files has no commit behind it
/// either, so it carries the other file's text.
#[derive(Clone)]
pub(crate) enum DiffBase {
    /// The working tree against HEAD, with the index marking which hunks are
    /// staged, for a reader who wants every change since the commit in the
    /// right column.
    Head,
    /// The tree at `sha`. `None` is the empty tree, which is what a root
    /// commit's parent amounts to.
    Rev {
        sha: Option<String>,
        /// How the revision became the base, which is what names the two sides.
        origin: RevOrigin,
    },
    /// Base text supplied directly, keyed by absolute path. A path the map
    /// does not carry falls back to the index, so an untouched file still
    /// diffs normally.
    Memory {
        files: HashMap<PathBuf, Arc<String>>,
    },
    /// One open file measured against another, for two files git does not
    /// relate.
    ///
    /// A diff job builds it for one path from [`DiffState::pairs`], and
    /// [`DiffState::base_override`] never holds it. `path`'s buffer diffs
    /// against `text`, the content of `base_path`.
    Pair {
        path: PathBuf,
        base_path: PathBuf,
        text: Arc<String>,
    },
}

impl DiffBase {
    /// The revision the reader named as `name`, which resolved to `sha`.
    pub(crate) fn named(name: &str, sha: String) -> Self {
        Self::Rev {
            sha: Some(sha),
            origin: RevOrigin::Named(name.to_string()),
        }
    }

    /// The parent of `commit`, as the base of a review that checked `commit`
    /// out.
    ///
    /// A `parent` of `None` is the empty tree under a root commit.
    pub(crate) fn parent_of(commit: &str, parent: Option<String>) -> Self {
        Self::Rev {
            sha: parent,
            origin: RevOrigin::ParentOf(commit.to_string()),
        }
    }

    /// `base`, the merge base of `from` and `against`, as the base of a review
    /// of `from` against `against`.
    pub(crate) fn review(base: String, from: &str, against: &str) -> Self {
        Self::Rev {
            sha: Some(base),
            origin: RevOrigin::Review {
                from: from.to_string(),
                against: against.to_string(),
            },
        }
    }
}

/// How a [`DiffBase::Rev`] became the base.
///
/// The sides bar names the two columns from it.
#[derive(Clone)]
pub(crate) enum RevOrigin {
    /// The reader named the revision, spelled as typed.
    Named(String),
    /// The revision is the parent of this commit, which a review checked out
    /// under the buffers. It holds the commit's full sha.
    ParentOf(String),
    /// A review of `from` against `against`, both as the reader typed them,
    /// whose revision is their merge base. The tip of `from` is checked out
    /// under the buffers.
    Review { from: String, against: String },
}

/// The other open file that one file measures against, for two files git does
/// not relate.
///
/// [`DiffState::pairs`] keys it by the measured file, which diffs against
/// `text` while every other file keeps the workspace's base.
#[derive(Clone)]
pub(crate) struct PairBase {
    pub(crate) base_path: PathBuf,
    /// The content of `base_path`, which follows the base file's buffer while
    /// that buffer is open.
    pub(crate) text: Arc<String>,
    /// The version of the base file's buffer that `text` was read at, which is
    /// how the diff drive knows the base moved.
    pub(crate) base_version: u64,
}

/// Which of its two own bases the working tree diffs against, picked by the
/// reader.
///
/// HEAD is the default, so a fresh session shows every change since the last
/// commit. A revision, a commit review, or an agent proposal displaces the
/// pick while that base holds. The diff returns to the pick when that base
/// gives way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WorktreeBase {
    /// The index, with the changes it holds over HEAD as staged marks, for a
    /// reader who picks only the changes not yet staged.
    Index,
    /// HEAD, as [`DiffBase::Head`] reads it, with the index marking which
    /// hunks are staged.
    #[default]
    Head,
}

/// The base [`DiffState::effective_base`] answers for the HEAD pick.
///
/// A `static` rather than a promoted `&DiffBase::Head`, because the enum
/// carries a `HashMap` variant and so has drop glue, which blocks rvalue
/// promotion.
static HEAD_BASE: DiffBase = DiffBase::Head;

/// What one repository tally job reads.
struct RepoTally {
    tallies: HunkTallies,
    /// The files changed since the revision base, `None` under any other base.
    rev_changed: Option<Vec<PathBuf>>,
}

/// The per-buffer bookkeeping that decides when a diff is owed, and what the
/// last one read.
///
/// Every entry is keyed by buffer except the blob cache and the pairs, which
/// are keyed by path, because a blob or a pair belongs to a file rather than
/// to whichever buffer happens to hold it open.
#[derive(Default)]
pub(crate) struct DiffState {
    /// In-flight diff-map population jobs, one per buffer. Held so the spawned
    /// blocking diff is not cancelled before it installs its [`DiffMap`] on the
    /// buffer.
    pub(super) jobs: HashMap<BufferId, DiffJob>,
    /// Diffs that a caller waits on, one per buffer, with the landings to run
    /// once the hunks arrive.
    ///
    /// A diff open places the view and the cursor by the hunks, and resolving
    /// the base and running the line pass costs 5-50 ms. A want runs both on
    /// the blocking pool, so the loop paints the plain frame meanwhile. A
    /// changed-file hop wants the drive's whole job instead, since it lands on
    /// the runs the tree pass makes. A `.git` write marks a want superseded and
    /// never drops it, so its landings still run.
    wants: HashMap<BufferId, DiffWant>,
    /// The buffer version each buffer's hunks-only map landed for.
    ///
    /// Such a map holds the hunks and none of the diff view's colors, so it
    /// does not count as current. The drive passes its settle gate at this
    /// version, since a buffer that was opened and not typed in has no burst
    /// to wait out, and the colors arrive a frame later.
    hunks_only: HashMap<BufferId, u64>,
    /// Buffer edit version each buffer's `diff_map` was last populated for.
    ///
    /// Records the buffers that keep no diff too, those outside any repo's
    /// workdir and those the repo ignores, with a cleared map so they are not
    /// retried every frame. Drives re-population when a buffer is edited past
    /// the recorded version.
    pub(super) versions: HashMap<BufferId, u64>,
    /// The buffer version each buffer's last diff job ran against.
    ///
    /// Kept through [`Self::invalidate`] and [`Self::invalidate_all`]. A git
    /// write moves the blobs without moving the buffer, so a buffer still at
    /// this version has no typing burst to wait out.
    diffed: HashMap<BufferId, u64>,
    /// Each diffed file's HEAD and index blobs, keyed by path.
    ///
    /// Saves the repo mutex and a blob decompression on every recompute, which
    /// is otherwise paid per keystroke. [`Self::invalidate`] drops one path's
    /// entry after a stage or an amend, and [`Self::invalidate_all`], which the
    /// `.git` watcher drives, drops every entry, so no entry outlives the git
    /// state it was read from.
    ///
    /// A `None` entry records a miss, which is the answer for a file outside
    /// the workdir or one the repo ignores. An untracked file's entry is the
    /// empty pair. Reaching either answer costs a working-tree stat walk and
    /// two rename passes, and only a git write turns a path into a rename
    /// target, so each is as cacheable as a hit and clears with the rest.
    pub(super) base_text: HashMap<PathBuf, Option<DiffBaseText>>,
    /// A base that displaces the working tree's pick, `None` while nothing
    /// does.
    ///
    /// Set through [`super::Workspace::set_diff_base`], which invalidates
    /// every buffer, since a base change moves every diff in the workspace. It
    /// never holds a [`DiffBase::Pair`], since a pair belongs to one file.
    ///
    /// See also:
    /// - [`Self::effective_base`] for the base the buffers diff against.
    /// - [`Self::pairs`] for the files that measure against another file.
    pub(super) base_override: Option<DiffBase>,
    /// The files measured against another open file, keyed by the measured
    /// path.
    ///
    /// A buffer holds one diff map, so a file measures against one base at a
    /// time, and a second pair for a file replaces the first. A pair holds for
    /// its own file alone, so pairs in other tabs and a git diff beside them
    /// stay as they are.
    pub(super) pairs: HashMap<PathBuf, PairBase>,
    /// The base the working tree diffs against when nothing displaces it.
    ///
    /// Every caller that drops a revision, a review, or a proposal lands on
    /// it, because [`Self::effective_base`] reads it while
    /// [`Self::base_override`] is `None`.
    pub(super) worktree_base: WorktreeBase,
    /// Parsed trees the structural refinement reuses across settles.
    ///
    /// A settle re-parses the same base and the same buffer prefix the last one
    /// read, so the memo turns the per-keystroke refinement into one parse of
    /// what actually changed. Shared with the blocking job, which is where the
    /// parses run.
    pub(super) tree_cache: TreeCache,
    /// Hunks staged and unstaged across the whole repo, plus the count each
    /// changed file carries, as the status bar's repo-wide tally reads them.
    ///
    /// `None` until the first tally lands, and for a workspace root outside any
    /// repo. Five events owe it a fresh read, each by clearing
    /// [`Self::tally_current`]. They are a landed save, a working-tree write
    /// that the watcher reports, a staging action, a re-read of the git state,
    /// and a first open.
    ///
    /// An edit is not one of them. The tally reads what is on disk, which an
    /// unsaved edit has not touched, so re-walking the repository per keystroke
    /// costs a full three-diff repo scan for an answer that never changed.
    pub(super) repo_hunk_tallies: Option<HunkTallies>,
    /// The files changed since the revision base, absolute, as the last tally
    /// read them under one. `None` under any other base.
    ///
    /// The tally counts against HEAD, but under a revision base the changes
    /// view lists everything changed since that revision, a file committed
    /// since and clean in the tree included.
    pub(super) rev_changed: Option<Vec<PathBuf>>,
    /// The in-flight repo-wide tally, held so it is not cancelled before its
    /// answer lands.
    tally_job: Option<Task<Option<RepoTally>>>,
    /// Whether the stored tally is known to match what is on disk.
    ///
    /// Starts `false`, because a fresh workspace has read no tally and the first
    /// drive owes one. A landed tally sets it, and a save, a working-tree write,
    /// a staging action, a git re-read, or a first open clears it. An unsaved
    /// edit does not, since the tally reads the disk and an unsaved edit has not
    /// touched it.
    pub(super) tally_current: bool,
    /// The version each buffer is currently settling on, and when that version
    /// was first seen. Read by [`Self::settled`].
    settle: HashMap<BufferId, (u64, Instant)>,
    /// The redraw timers [`Self::settled`] arms, held so they are not cancelled
    /// on drop. Replaced per buffer, so a burst keeps one.
    settle_timers: HashMap<BufferId, Task<()>>,
    /// The base buffer version that each pair waits to settle, and when that
    /// version was first seen, keyed by the measured path.
    ///
    /// A map of its own rather than entries in [`Self::settle`], which the
    /// base buffer's own diff keys by the same buffer and version. That map
    /// drops its entry when it answers, so a shared entry opens a second window
    /// for whichever caller asks second.
    pair_settle: HashMap<PathBuf, (u64, Instant)>,
    /// The redraw timers for the [`Self::pair_settle`] windows, held so they
    /// are not cancelled on drop.
    pair_settle_timers: HashMap<PathBuf, Task<()>>,
    /// Moves each time a diff map is installed on a buffer, so a reader of
    /// the open buffers' maps tells a moved map from a still one without
    /// reading every buffer.
    ///
    /// A landing whose hunks render the same as the installed map installs
    /// nothing, and moves nothing.
    pub(super) maps_epoch: u64,
}

impl DiffState {
    /// The base every file without a pair diffs against, `None` while the pick
    /// is the index and nothing displaces it.
    ///
    /// A displacing base wins over the pick, and HEAD, the default pick,
    /// answers [`DiffBase::Head`].
    pub(super) fn effective_base(&self) -> Option<&DiffBase> {
        self.base_override.as_ref().or(match self.worktree_base {
            WorktreeBase::Head => Some(&HEAD_BASE),
            WorktreeBase::Index => None,
        })
    }

    /// The base a diff job measures `path` against, its pair when it has one
    /// and [`Self::effective_base`] otherwise.
    fn base_for(&self, path: &Path) -> Option<DiffBase> {
        match self.pairs.get(path) {
            Some(pair) => Some(DiffBase::Pair {
                path: path.to_path_buf(),
                base_path: pair.base_path.clone(),
                text: pair.text.clone(),
            }),
            None => self.effective_base().cloned(),
        }
    }

    /// Force a recompute of `id`'s diff map against fresh blobs, by dropping its
    /// recorded version and `path`'s cached blobs.
    ///
    /// A stage or an amend moves one of the two blobs, and the cached pair
    /// marks the hunks staged as they were before the write. The text did not
    /// move, so the recompute skips the settle window. `path` is `None` for a
    /// buffer with no file.
    ///
    /// A job or a want in flight is marked superseded and kept. A want's
    /// landings still run. A job's map still paints, and the recompute starts
    /// once it lands, so the buffer never has two jobs at once.
    pub(super) fn invalidate(&mut self, id: BufferId, path: Option<&Path>) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.superseded = true;
        }
        self.versions.remove(&id);
        self.hunks_only.remove(&id);
        if let Some(want) = self.wants.get_mut(&id) {
            want.superseded = true;
        }
        self.tally_current = false;
        if let Some(path) = path {
            self.base_text.remove(path);
        }
    }

    /// Measure `path` against `pair`, in place of any pair it had, where `id`
    /// is the measured file's open buffer.
    ///
    /// Only `path` re-diffs, and with no settle window, since its own text did
    /// not move.
    pub(super) fn set_pair(&mut self, path: PathBuf, pair: PairBase, id: Option<BufferId>) {
        self.stale_measured(&path, id);
        self.pairs.insert(path, pair);
    }

    /// Return `path` to the workspace's base, and answer whether it had a pair.
    ///
    /// Only `path` re-diffs, as after [`Self::set_pair`].
    pub(super) fn remove_pair(&mut self, path: &Path, id: Option<BufferId>) -> bool {
        if self.pairs.remove(path).is_none() {
            return false;
        }
        self.stale_measured(path, id);
        true
    }

    /// Drop `path`'s pair settle window, and stale its map and its cached base.
    fn stale_measured(&mut self, path: &Path, id: Option<BufferId>) {
        self.pair_settle.remove(path);
        self.pair_settle_timers.remove(path);
        match id {
            Some(id) => self.invalidate(id, Some(path)),
            None => {
                self.base_text.remove(path);
            },
        }
    }

    /// Stale every buffer's diff map by dropping all recorded versions and
    /// marking the jobs in flight superseded.
    ///
    /// Used after git state moves under the editor. An external rebase or
    /// checkout changes HEAD, so a map computed against the old one describes a
    /// base that no longer exists, whatever the buffer's own version says.
    ///
    /// A job in flight is marked superseded and not dropped. Its map installs
    /// when it lands, because the job read its blobs no earlier than the map on
    /// screen and so is no staler. The buffer stays stale, so the next drive
    /// runs it again against fresh blobs. The next drive recomputes only visible
    /// buffers, so hidden ones re-diff lazily when next shown. An unedited
    /// buffer recomputes without the settle window.
    pub(super) fn invalidate_all(&mut self) {
        for job in self.jobs.values_mut() {
            job.superseded = true;
        }
        for want in self.wants.values_mut() {
            want.superseded = true;
        }
        self.versions.clear();
        self.hunks_only.clear();
        self.tally_current = false;
        // The cached blobs were read from the git state this call is reacting
        // to having changed, so they are exactly what must not be reused.
        self.base_text.clear();
    }

    /// Stale every buffer's diff map for a new base, and drop the jobs in
    /// flight.
    ///
    /// A job in flight read the base that the change replaces. After a `.git`
    /// write, [`Self::invalidate_all`] keeps such a job, because its blobs are
    /// no staler than the map on screen. After a base change, its map shows a
    /// base the reader has left, so the job goes.
    pub(super) fn reset_for_new_base(&mut self) {
        self.jobs.clear();
        self.invalidate_all();
    }

    /// Mark the repo-wide tally as owed, so the next drive spawns one.
    ///
    /// This serves the events that move the tally without staling any buffer
    /// diff. A landed save is the case in point: the file's own hunks are
    /// unchanged by the write, but the repo's counts are not. A working-tree
    /// write that the watcher reports is another. Another tool's write moves
    /// the repo's counts, also for a file that no buffer holds.
    pub(super) fn stale_tally(&mut self) {
        self.tally_current = false;
    }

    /// Whether `id`'s installed map was computed for buffer version `version`.
    pub(super) fn current(&self, id: BufferId, version: u64) -> bool {
        self.versions.get(&id) == Some(&version)
    }

    /// Whether `id`'s installed map holds the hunks for buffer version
    /// `version` and waits for its colors.
    pub(super) fn hunks_installed(&self, id: BufferId, version: u64) -> bool {
        self.hunks_only.get(&id) == Some(&version)
    }

    /// Whether a hunks-only diff of `id` is on the pool.
    #[cfg(test)]
    pub(super) fn want_pending(&self, id: BufferId) -> bool {
        self.wants.contains_key(&id)
    }

    /// Run `landing` when `id`'s want lands, and answer `true`. With no want
    /// of `id` in flight, answer `false` and keep nothing.
    pub(super) fn add_landing(&mut self, id: BufferId, landing: DiffLanding) -> bool {
        let Some(want) = self.wants.get_mut(&id) else {
            return false;
        };
        want.landings.push(landing);
        true
    }

    /// Drop the latched-view landing that `editor` waits on in `id`'s want.
    pub(super) fn drop_latched_view(&mut self, id: BufferId, editor: EditorId) {
        let Some(want) = self.wants.get_mut(&id) else {
            return;
        };
        want.landings.retain(|landing| {
            !matches!(landing, DiffLanding::LatchedView { editor: waiting } if *waiting == editor)
        });
    }

    /// Drop everything held for a closing buffer, and `path`'s blobs with it.
    ///
    /// Two of the collections hold a `Task`, and dropping one cancels work
    /// whose result nothing reads any more.
    pub(super) fn release(&mut self, id: BufferId, path: Option<&Path>) {
        self.jobs.remove(&id);
        self.wants.remove(&id);
        self.versions.remove(&id);
        self.hunks_only.remove(&id);
        self.diffed.remove(&id);
        self.settle.remove(&id);
        self.settle_timers.remove(&id);
        if let Some(path) = path {
            self.base_text.remove(path);
        }
    }

    /// Whether anything is still held for `id`, or for `path`'s blobs.
    ///
    /// Kept beside [`Self::release`] so the two lists stay the same list.
    #[cfg(test)]
    pub(super) fn holds(&self, id: BufferId, path: Option<&Path>) -> bool {
        self.jobs.contains_key(&id)
            || self.wants.contains_key(&id)
            || self.versions.contains_key(&id)
            || self.hunks_only.contains_key(&id)
            || self.diffed.contains_key(&id)
            || self.settle.contains_key(&id)
            || self.settle_timers.contains_key(&id)
            || path.is_some_and(|path| self.base_text.contains_key(path))
    }

    /// Whether `buffer_id` has held `version` for [`DIFF_SETTLE`], and so is
    /// worth diffing.
    ///
    /// A version seen for the first time opens the window and arms a redraw
    /// timer, so the frame that closes the window is drawn even when nothing
    /// else asks for one.
    ///
    /// A closed window stays closed for that version. A buffer whose diff job
    /// is still in flight asks again on later drives, and its next job starts
    /// as soon as the job in flight lands.
    pub(super) fn settled(
        &mut self,
        executor: &Executor,
        redraw_notify: &Arc<Notify>,
        buffer_id: BufferId,
        version: u64,
    ) -> bool {
        let now = executor.now();
        match self.settle.get(&buffer_id) {
            Some((settling, since)) if *settling == version => {
                now.duration_since(*since) >= DIFF_SETTLE
            },
            _ => {
                self.settle.insert(buffer_id, (version, now));
                self.settle_timers
                    .insert(buffer_id, settle_timer(executor, redraw_notify));
                false
            },
        }
    }

    /// Start a diff of `id` on the blocking pool, and run `landing` once its
    /// hunks land.
    ///
    /// The job resolves the base and runs the line pass, which is what a diff
    /// open places the view and the cursor by. It parses nothing, so the diff
    /// view's colors arrive with a later drive. A landing that asks for the
    /// whole map gets the drive's job instead. That job also runs the tree
    /// pass, which splits a hunk into the runs a walk stops on, and the colors
    /// and anchors, so its map counts as current once it lands.
    ///
    /// A want in flight at the buffer's version takes the landing and spawns
    /// nothing, unless the landing asks for the whole map and the want holds
    /// the hunks alone. A want that gives way hands its landings to the new
    /// one, as a want at an older version does.
    ///
    /// Answers `landing` back when nothing is wanted, which is the answer for a
    /// buffer with no path, so the caller runs it at once.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn want(
        &mut self,
        buffers: &BufferRegistry,
        git_root: &Path,
        executor: &Executor,
        git_host: &Arc<dyn GitHost>,
        language_registry: &LanguageRegistry,
        syntax_styles: &SyntaxStyles,
        base_cache: &BaseHighlightCache,
        redraw_notify: &Arc<Notify>,
        id: BufferId,
        landing: Option<DiffLanding>,
    ) -> Option<DiffLanding> {
        let Some(path) = buffers.path_for(id).map(Path::to_path_buf) else {
            return landing;
        };
        let Some(shared) = buffers.get(id) else {
            return landing;
        };
        // The whole snapshot rather than its text, which the job materializes
        // on the pool.
        let buffer_snapshot = shared.read().expect("buffer poisoned").snapshot.clone();
        let target_version = buffer_snapshot.version;
        let whole = landing.as_ref().is_some_and(DiffLanding::wants_whole_map);

        let mut landings = match self.wants.remove(&id) {
            Some(mut want) if want.target_version == target_version && (want.whole || !whole) => {
                want.landings.extend(landing);
                self.wants.insert(id, want);
                return None;
            },
            Some(want) => want.landings,
            None => Vec::new(),
        };
        landings.extend(landing);

        // A hunks-only job takes no language, memo, tree, or anchors. Its
        // hunks lack anchors on purpose, which is what tells
        // `DiffMap::renders_same_as` that the drive's colored map decorates
        // differently.
        let (language, buffer_tree) = match whole {
            true => (
                language_registry.for_path(&path),
                buffers
                    .syntax(id)
                    .filter(|state| state.version == target_version)
                    .map(|state| state.tree.clone()),
            ),
            false => (None, None),
        };

        let task = executor.spawn_blocking({
            let git_host = git_host.clone();
            let git_root = git_root.to_path_buf();
            let redraw = redraw_notify.clone();
            let syntax_styles = syntax_styles.clone();
            let base_cache = base_cache.clone();
            let diff_base = self.base_for(&path);
            let cached_base = self.base_text.get(&path).cloned();
            let tree_cache = whole.then(|| self.tree_cache.clone());
            move || {
                let out = run_diff_job(
                    &*git_host,
                    &git_root,
                    path,
                    diff_base.as_ref(),
                    cached_base,
                    &buffer_snapshot,
                    target_version,
                    id,
                    language.as_ref(),
                    &syntax_styles,
                    &base_cache,
                    tree_cache.as_ref(),
                    buffer_tree.as_ref(),
                    whole,
                );
                redraw.notify_one();
                out
            }
        });
        self.wants.insert(
            id,
            DiffWant {
                target_version,
                task,
                landings,
                superseded: false,
                whole,
            },
        );

        // Owed rather than read here, because a repo-wide tally is a walk of
        // every diff in the repository.
        self.tally_current = false;
        None
    }

    /// Install each wanted map whose hunks arrived, and answer its buffer with
    /// the landings to run, in the order the callers added them.
    ///
    /// A landed map is whole when the want ran the drive's job, or when the
    /// file has no base or no language, since there are no colors to wait for,
    /// so its version counts as current. Any other map records a hunks-only
    /// marker, and the drive adds its colors. A
    /// superseded want's map still paints, and files neither its blobs nor a
    /// version.
    ///
    /// A want installs nothing when a whole map at its version or a later one
    /// is already recorded. Its landings still run.
    pub(super) fn poll_wants(
        &mut self,
        buffers: &BufferRegistry,
        language_registry: &LanguageRegistry,
    ) -> Vec<(BufferId, Vec<DiffLanding>)> {
        let waker = futures::task::noop_waker();
        let mut landed = Vec::new();
        self.wants.retain(|_, want| {
            let mut cx = Context::from_waker(&waker);
            match Pin::new(&mut want.task).poll(&mut cx) {
                Poll::Ready(out) => {
                    landed.push((
                        out,
                        want.superseded,
                        want.whole,
                        mem::take(&mut want.landings),
                    ));
                    false
                },
                Poll::Pending => true,
            }
        });

        let mut landings = Vec::with_capacity(landed.len());
        for (out, superseded, ran_whole, want_landings) in landed {
            let (id, version) = (out.buffer_id, out.target_version);
            // A drive job in flight when the want started sometimes lands first,
            // at this version or a later one. Its whole map is on the buffer and
            // its version is recorded, so a hunks-only map must not replace it.
            let outrun = self
                .versions
                .get(&id)
                .is_some_and(|&recorded| recorded >= version);
            if !outrun {
                let whole = ran_whole
                    || out.base.is_none()
                    || language_registry.for_path(&out.path).is_none();
                self.land(buffers, out, superseded);
                if !superseded {
                    // A whole map replaces the hunks a want installed before
                    // it, so their marker goes with them.
                    match whole {
                        true => {
                            self.versions.insert(id, version);
                            self.hunks_only.remove(&id);
                        },
                        false => {
                            self.hunks_only.insert(id, version);
                        },
                    }
                }
            }
            landings.push((id, want_landings));
        }
        landings
    }

    /// Install `diff_map` on `id` and record it as computed for the buffer's
    /// current version, so it reads as current to [`Self::diff_map_current`].
    ///
    /// Lets a test stand up a diff map without a git fixture behind it. Writing
    /// the map alone would leave it version-less, which every caller correctly
    /// treats as stale.
    #[cfg(test)]
    pub(super) fn install_test(
        &mut self,
        buffers: &BufferRegistry,
        id: BufferId,
        diff_map: DiffMap,
    ) {
        let Some(shared) = buffers.get(id) else {
            return;
        };
        let version = {
            let mut guard = shared.write().expect("buffer poisoned");
            guard.diff_map = Some(diff_map);
            guard.snapshot.version
        };
        self.versions.insert(id, version);
        self.maps_epoch += 1;
    }

    /// Re-read each pair base's text once its base file's buffer has held a
    /// new version for [`DIFF_SETTLE`].
    ///
    /// Each pair waits out its own window. The measured file's map goes stale
    /// with its base, and its next diff skips a second window, because its own
    /// text did not move. A base file with no open buffer keeps the text last
    /// read.
    fn refresh_pair_bases(
        &mut self,
        buffers: &BufferRegistry,
        executor: &Executor,
        redraw_notify: &Arc<Notify>,
    ) {
        let now = executor.now();
        for (path, pair) in &mut self.pairs {
            let Some(shared) = buffers
                .id_for_path(&pair.base_path)
                .and_then(|id| buffers.get(id))
            else {
                continue;
            };
            let version = shared.read().expect("buffer poisoned").snapshot.version;
            if version == pair.base_version {
                continue;
            }

            match self.pair_settle.get(path) {
                Some(&(settling, since)) if settling == version => {
                    if now.duration_since(since) < DIFF_SETTLE {
                        continue;
                    }
                },
                _ => {
                    self.pair_settle.insert(path.clone(), (version, now));
                    self.pair_settle_timers
                        .insert(path.clone(), settle_timer(executor, redraw_notify));
                    continue;
                },
            }

            self.pair_settle.remove(path);
            {
                let guard = shared.read().expect("buffer poisoned");
                pair.base_version = guard.snapshot.version;
                pair.text = Arc::new(guard.snapshot.visible_text.to_string());
            }
            self.base_text.remove(path);
            let Some(id) = buffers.id_for_path(path) else {
                continue;
            };
            if let Some(job) = self.jobs.get_mut(&id) {
                job.superseded = true;
            }
            self.versions.remove(&id);
            self.hunks_only.remove(&id);
            if let Some(want) = self.wants.get_mut(&id) {
                want.superseded = true;
            }
        }
    }

    /// Populate visible git-tracked buffers' diff maps on a background thread.
    ///
    /// Polls in-flight jobs and installs their diff maps, then spawns a job for
    /// each visible git-tracked buffer whose diff is stale. Each pair base is
    /// refreshed from its buffer first, so the jobs below read the current base.
    ///
    /// Mirrors [`Self::drive_parse_jobs`] with at most one job per buffer,
    /// coalescing rapid edits by re-queuing only after the in-flight job
    /// completes. A buffer with no path, no repo, or no HEAD content records its
    /// version with a cleared map, so it is not retried until the next edit.
    ///
    /// Answers whether a repository tally landed, which moves the files a
    /// changes view lists.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn drive(
        &mut self,
        buffers: &BufferRegistry,
        git_root: &Path,
        visible: &[BufferId],
        executor: &Executor,
        git_host: &Arc<dyn GitHost>,
        language_registry: &Arc<LanguageRegistry>,
        syntax_styles: &SyntaxStyles,
        base_cache: &BaseHighlightCache,
        redraw_notify: &Arc<Notify>,
    ) -> bool {
        self.refresh_pair_bases(buffers, executor, redraw_notify);

        let waker = futures::task::noop_waker();
        let mut completed: Vec<(DiffJobOutput, bool, bool, Instant)> = Vec::new();
        self.jobs.retain(|_, job| {
            let mut cx = Context::from_waker(&waker);
            match Pin::new(&mut job.task).poll(&mut cx) {
                Poll::Ready(out) => {
                    completed.push((out, job.superseded, job.base_moved, job.started));
                    false
                },
                Poll::Pending => true,
            }
        });
        for (out, superseded, base_moved, started) in completed {
            if base_moved {
                tracing::info!(
                    target: "stoat::diff",
                    path = %out.path.display(),
                    ms = executor.now().duration_since(started).as_millis() as u64,
                    "diff job landed after its base moved"
                );
            }
            let (buffer_id, target_version) = (out.buffer_id, out.target_version);
            self.land(buffers, out, superseded);
            // Recorded either way, so an unchanged result still counts as
            // diffed and the buffer is not tried again next frame. A superseded
            // job's unrecorded version owes the buffer a re-run.
            if !superseded {
                self.versions.insert(buffer_id, target_version);
                self.hunks_only.remove(&buffer_id);
            }
            self.diffed.insert(buffer_id, target_version);
        }

        let tally_landed = self.drive_tally(git_root, executor, git_host, redraw_notify);

        // The staleness checks come first so a settled buffer costs one lock
        // acquisition and nothing else. Its path would otherwise be cloned and
        // dropped every frame.
        for &buffer_id in visible {
            let Some(shared) = buffers.get(buffer_id) else {
                continue;
            };
            let cur_version = shared.read().expect("buffer poisoned").snapshot.version;

            if self.versions.get(&buffer_id) == Some(&cur_version) {
                continue;
            }
            if self
                .wants
                .get(&buffer_id)
                .is_some_and(|want| want.target_version == cur_version)
            {
                continue;
            }
            // The buffer is still at the version its last diff read, so only its
            // blobs moved. There is no typing burst to wait out.
            let base_moved = self.diffed.get(&buffer_id) == Some(&cur_version);
            // A hunks-only map marks a buffer that was opened and not typed in,
            // which has no burst to wait out either, and still owes its colors.
            let opened = self.hunks_only.get(&buffer_id) == Some(&cur_version);
            if !base_moved
                && !opened
                && !self.settled(executor, redraw_notify, buffer_id, cur_version)
            {
                continue;
            }
            // A newer version waits for the job in flight rather than running
            // beside it. The check sits after the settle window, so the window
            // runs meanwhile and the next job starts on the drive that lands
            // this one.
            if self.jobs.contains_key(&buffer_id) {
                continue;
            }

            let Some(path) = buffers.path_for(buffer_id).map(Path::to_path_buf) else {
                continue;
            };
            // The whole snapshot rather than its rope, since the hunks are
            // anchored against the text they were diffed from.
            let buffer_snapshot = shared.read().expect("buffer poisoned").snapshot.clone();

            let language = language_registry.for_path(&path);
            let cached_base = self.base_text.get(&path).cloned();
            // Parsing this buffer is the largest item in the diff, and the
            // highlight pipeline has already done it for these exact bytes:
            // equal versions mean equal text. Cloning a tree is a refcount
            // bump, and the clone is `Send`, which the background dropper
            // already relies on. A stale version leaves the diff to parse.
            let buffer_tree = buffers
                .syntax(buffer_id)
                .filter(|state| state.version == cur_version)
                .map(|state| state.tree.clone());

            let task = executor.spawn_blocking({
                let git_host = git_host.clone();
                let git_root = git_root.to_path_buf();
                let redraw = redraw_notify.clone();
                let syntax_styles = syntax_styles.clone();
                let base_cache = base_cache.clone();
                let path = path.clone();
                let diff_base = self.base_for(&path);
                let tree_cache = self.tree_cache.clone();
                move || {
                    let out = run_diff_job(
                        &*git_host,
                        &git_root,
                        path,
                        diff_base.as_ref(),
                        cached_base,
                        &buffer_snapshot,
                        cur_version,
                        buffer_id,
                        language.as_ref(),
                        &syntax_styles,
                        &base_cache,
                        Some(&tree_cache),
                        buffer_tree.as_ref(),
                        true,
                    );
                    redraw.notify_one();
                    out
                }
            });
            self.jobs.insert(
                buffer_id,
                DiffJob {
                    task,
                    superseded: false,
                    base_moved,
                    started: executor.now(),
                },
            );
        }
        tally_landed
    }

    /// Install the repository tally when its job lands, spawn one when a tally
    /// is owed, and report whether one landed.
    ///
    /// Under a revision base the job also lists the files changed since the
    /// revision, which the changes view lists in the tally's place.
    pub(super) fn drive_tally(
        &mut self,
        git_root: &Path,
        executor: &Executor,
        git_host: &Arc<dyn GitHost>,
        redraw_notify: &Arc<Notify>,
    ) -> bool {
        let mut landed = false;
        if let Some(job) = &mut self.tally_job {
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            if let Poll::Ready(tally) = Pin::new(job).poll(&mut cx) {
                // A `None` answer means the root is outside a repo now, which
                // says nothing about the last tally that was read inside one.
                if let Some(tally) = tally {
                    self.repo_hunk_tallies = Some(tally.tallies);
                    self.rev_changed = tally.rev_changed;
                    landed = true;
                }
                self.tally_job = None;
            }
        }
        if !self.tally_current && self.tally_job.is_none() {
            self.tally_current = true;
            let rev = match self.effective_base() {
                Some(DiffBase::Rev { sha: Some(sha), .. }) => Some(sha.clone()),
                _ => None,
            };
            self.tally_job = Some(executor.spawn_blocking({
                let git_host = git_host.clone();
                let git_root = git_root.to_path_buf();
                let redraw = redraw_notify.clone();
                move || {
                    let tally = git_host.discover(&git_root).map(|repo| RepoTally {
                        tallies: repo.hunk_tallies(),
                        rev_changed: rev.map(|sha| {
                            repo.changed_files_from(&sha)
                                .into_iter()
                                .filter(|file| !file.untracked)
                                .map(|file| file.path)
                                .collect()
                        }),
                    });
                    redraw.notify_one();
                    tally
                }
            }));
        }
        landed
    }

    /// Whether the repository tally is owed or on the pool.
    pub(super) fn tally_pending(&self) -> bool {
        !self.tally_current || self.tally_job.is_some()
    }

    /// Install `out`'s map on its buffer, and file the blobs it read unless a
    /// later write superseded the job that read them.
    ///
    /// A superseded map still paints, since it read its blobs no earlier than
    /// the map on screen.
    fn land(&mut self, buffers: &BufferRegistry, out: DiffJobOutput, superseded: bool) {
        if !superseded {
            // Filed whether or not it resolved, since a miss is what an
            // ignored buffer keeps re-deriving otherwise.
            self.base_text.insert(out.path, out.base);
        }
        if let Some(shared) = buffers.get(out.buffer_id) {
            let mut guard = shared.write().expect("buffer poisoned");
            // A recompute landing on the same hunks is not news. Every
            // decoration consumer keys off the map's version, and the minimap's
            // edge sweep re-derives the whole file from it, so installing an
            // identical map costs that for nothing. A write under .git re-diffs
            // every visible buffer against content that did not move, which is
            // where this earns its keep.
            let same = match (&guard.diff_map, &out.diff_map) {
                (Some(installed), Some(fresh)) => installed.renders_same_as(fresh),
                (None, None) => true,
                _ => false,
            };
            if !same {
                guard.diff_map = out.diff_map;
                self.maps_epoch += 1;
            }
        }
    }
}

pub(super) struct DiffJob {
    pub(super) task: Task<DiffJobOutput>,
    /// A `.git` write, a stage, or a pair base refresh landed after the job
    /// read its blobs, so its map still paints and its version and blobs do not
    /// count.
    pub(super) superseded: bool,
    /// The buffer held the text its last diff read, so only the base moved,
    /// through a git write or a switch of base. Such a job skips the settle
    /// window, and its landing logs how long it ran, which measures how long a
    /// stage takes to reach the gutter.
    pub(super) base_moved: bool,
    pub(super) started: Instant,
}

pub(super) struct DiffJobOutput {
    pub(super) buffer_id: BufferId,
    /// The file the job diffed, so its base text can be filed under the same
    /// key the next job for it will look under.
    pub(super) path: PathBuf,
    pub(super) target_version: u64,
    pub(super) diff_map: Option<DiffMap>,
    /// The blobs the diff ran against, `None` when no repo holds the file in
    /// its workdir or the repo ignores it, and there was nothing to diff.
    ///
    /// Filed into [`DiffState::base_text`] either way, because `None` is the
    /// answer for an ignored file and re-deriving it costs a working-tree
    /// walk.
    pub(super) base: Option<DiffBaseText>,
}

/// A diff on the blocking pool that a caller waits on, and what runs once it
/// lands.
pub(super) struct DiffWant {
    target_version: u64,
    task: Task<DiffJobOutput>,
    /// What the callers that wanted the hunks do with them, in the order they
    /// asked.
    landings: Vec<DiffLanding>,
    /// A `.git` write landed after the want read its blobs, so its map still
    /// paints, and its blobs and its marker do not count.
    superseded: bool,
    /// The job is the drive's, with the tree pass, the colors, and anchors, so
    /// its map counts as current once it lands.
    whole: bool,
}

/// What a caller that wanted a buffer's hunks does once they land.
///
/// A diff open places the view and the cursor by the hunks, so each landing is
/// the step that waited for them. One that acts on an editor names it, since
/// the focus moves on its own clock while the hunks are on the pool.
#[derive(Debug, Clone)]
pub(crate) enum DiffLanding {
    /// Show a latched pane's editor as a diff when its file has a change, and
    /// plain when it has none.
    LatchedView { editor: EditorId },
    /// Land a diff toggle's cursor on the change nearest it, with `origin` as
    /// the jumplist entry the jump records.
    ToggleJump {
        editor: EditorId,
        origin: Option<JumpEntry>,
    },
    /// A change-walk press that arrived before the hunks.
    ChangeWalk { dir: ChangeDir, count: u32 },
    /// A first-change or last-change press that arrived before the hunks.
    EdgeChange { last: bool },
    /// Report two equal files once a pair press's hunks are in, when `editor`
    /// still holds the focus and shows the diff.
    ///
    /// The verdict waits for the hunks, since the map on the buffer at the
    /// press still measures the file against the base it had before the pair.
    PairVerdict { editor: EditorId },
    /// A changed-file hop that opened its target, landing on the file's first
    /// (Next) or last (Prev) stop once the whole map is in.
    ///
    /// `origin` is the jumplist entry for where the hop started, and `wrapped`
    /// says the walk went past an end. `fallback` is the scan's row and the
    /// rows its hunk covers, for a map that offers no stop.
    FileHop {
        dir: ChangeDir,
        origin: Option<JumpEntry>,
        wrapped: bool,
        fallback: (u32, Range<u32>),
    },
    /// A flip's screen row, applied once the hunks are in so the row is
    /// measured against the display the reader sees.
    HeldRow { editor: EditorId, screen_row: u32 },
}

impl DiffLanding {
    /// Whether the landing reads the stops a walk offers, which the tree pass
    /// splits into runs, so it waits for the whole map rather than the hunks
    /// alone.
    pub(crate) fn wants_whole_map(&self) -> bool {
        matches!(self, Self::FileHop { .. })
    }

    /// The variant's name, which the session log gives the landing in place
    /// of its fields.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::LatchedView { .. } => "LatchedView",
            Self::ToggleJump { .. } => "ToggleJump",
            Self::ChangeWalk { .. } => "ChangeWalk",
            Self::EdgeChange { .. } => "EdgeChange",
            Self::PairVerdict { .. } => "PairVerdict",
            Self::FileHop { .. } => "FileHop",
            Self::HeldRow { .. } => "HeldRow",
        }
    }
}

/// A task that wakes a redraw when a settle window closes, so the frame that
/// ends the window is drawn even when nothing else asks for one.
fn settle_timer(executor: &Executor, redraw_notify: &Arc<Notify>) -> Task<()> {
    let timer_executor = executor.clone();
    executor.spawn_with_redraw(redraw_notify.clone(), async move {
        timer_executor.timer(DIFF_SETTLE).await;
    })
}

/// Diff `buffer_snapshot`, the text of `path` at `target_version`, against its
/// base, and package what the run read.
///
/// Runs on the blocking pool. `cached_base` is the base an earlier run filed,
/// and its absence resolves the base through `git_host` against `diff_base`.
/// With a language, the run also parses the base for the diff view's colors
/// and refines the hunks structurally, reusing `tree_memo` and `buffer_tree`.
/// With `anchor`, the hunks are anchored against the snapshot.
#[allow(clippy::too_many_arguments)]
fn run_diff_job(
    git_host: &dyn GitHost,
    git_root: &Path,
    path: PathBuf,
    diff_base: Option<&DiffBase>,
    cached_base: Option<Option<DiffBaseText>>,
    buffer_snapshot: &TextBufferSnapshot,
    target_version: u64,
    buffer_id: BufferId,
    language: Option<&Arc<Language>>,
    syntax_styles: &SyntaxStyles,
    base_cache: &BaseHighlightCache,
    tree_memo: Option<&TreeCache>,
    buffer_tree: Option<&Tree>,
    anchor: bool,
) -> DiffJobOutput {
    // Materialized only now that a job is committed, off the event-loop thread.
    let buffer_text = buffer_snapshot.visible_text.to_string();
    // A miss is an answer, so it is filed back like a hit and the next settle
    // in an ignored buffer walks nothing.
    let base = match cached_base {
        Some(base) => base,
        None => resolve_base(git_host, git_root, &path, diff_base),
    };
    let diff_map = base.as_ref().and_then(|base| {
        compute_diff_map(
            &buffer_text,
            language,
            syntax_styles,
            base_cache,
            base,
            tree_memo,
            buffer_tree,
        )
        .map(|mut diff_map| {
            if anchor {
                diff_map.anchor_hunks(buffer_snapshot);
            }
            diff_map
        })
    });
    DiffJobOutput {
        buffer_id,
        path,
        target_version,
        diff_map,
        base,
    }
}

/// The names of what the diff view's left and right columns hold under
/// `base`, as `(left, right)`.
///
/// A commit review names the commit's parent on the left and the commit on the
/// right, because the review checks that commit out under the buffers. A
/// revision the reader named keeps its spelling beside the short sha it
/// resolved to. A pair names its two files.
pub(crate) fn diff_sides(base: Option<&DiffBase>) -> (String, String) {
    match base {
        None => ("index".to_string(), "working tree".to_string()),
        Some(DiffBase::Head) => ("HEAD".to_string(), "working tree".to_string()),
        Some(DiffBase::Rev { sha, origin }) => {
            let base = match sha {
                Some(sha) => short_sha(sha),
                // A root commit's parent, against which every line reads added.
                None => "empty".to_string(),
            };
            match origin {
                RevOrigin::Named(name) => {
                    let left = match sha {
                        Some(sha) if sha.starts_with(name.as_str()) => base,
                        _ => format!("{name} {base}"),
                    };
                    (left, "working tree".to_string())
                },
                RevOrigin::ParentOf(commit) => {
                    let left = match sha {
                        Some(_) => format!("parent {base}"),
                        None => base,
                    };
                    (left, format!("commit {}", short_sha(commit)))
                },
                RevOrigin::Review { from, against } => (format!("{against} {base}"), from.clone()),
            }
        },
        // An agent's proposal sits under no revision. The base is the file as
        // it stood before the proposal, which is what "original" names.
        Some(DiffBase::Memory { .. }) => ("original".to_string(), "proposal".to_string()),
        Some(DiffBase::Pair {
            path, base_path, ..
        }) => pair_names(base_path, path),
    }
}

/// The names a pair diff shows for its two files, the base first.
///
/// Each file takes its file name alone. Two files with one name take their
/// paths below the directory they share, so the two sides never read the same.
pub(crate) fn pair_names(base_path: &Path, path: &Path) -> (String, String) {
    let file_name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let names = (file_name(base_path), file_name(path));
    if names.0 != names.1 {
        return names;
    }

    let shared = base_path
        .components()
        .zip(path.components())
        .take_while(|(left, right)| left == right)
        .count();
    let below = |path: &Path| {
        path.components()
            .skip(shared)
            .collect::<PathBuf>()
            .to_string_lossy()
            .into_owned()
    };
    (below(base_path), below(path))
}

/// The first seven chars of `sha`, the length the sides bar and the base
/// prompt show.
pub(crate) fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Where a diff view stands in the repository's review, as the bar names it.
///
/// `file_at` of `file_total` places the focused file among the changed files,
/// in the tally's path order. The focused file always counts, since the view
/// shows it whether or not the tally lists it. `hunk_at` of `hunk_total`
/// places the cursor among every hunk in the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HunkPosition {
    pub(crate) file_at: usize,
    pub(crate) file_total: usize,
    /// `None` for a cursor above the focused file's first hunk, which is the
    /// one place the walk has no hunk behind it.
    pub(crate) hunk_at: Option<usize>,
    pub(crate) hunk_total: usize,
}

/// Where the focused diff view stands among the repository's changed files
/// and hunks, or `None` when the focused pane is not a diff view over a file
/// the repo tracks.
///
/// A changes view counts through its focused excerpt, which is a diff view.
///
/// The hunk total counts the focused file through its live diff map rather
/// than through the stored tally, because the map is what the reader is
/// looking at and the tally is refreshed a beat later. The focused file counts
/// changes, with hunks that only whitespace-only rows separate folded into
/// one, as the walk crosses them. A file counts as changed when the tally
/// gives it a hunk.
///
/// Under the index base the count is of unstaged hunks alone, in the focused
/// file and in the tally. Those hunks are the stops the diff view's walk makes.
/// Under every other base the tally counts from HEAD.
pub(crate) fn repo_hunk_position(ws: &Workspace) -> Option<HunkPosition> {
    let (editor_id, _) = ws.focused_editor()?;
    let editor = ws.editors.get(editor_id)?;
    if !editor.diff_view {
        return None;
    }
    let path = ws.buffers.path_for(editor.buffer_id)?;
    let relative = path
        .strip_prefix(&ws.git_root)
        .unwrap_or(path)
        .to_path_buf();
    let totals = match ws.diff_base() {
        None => ws.repo_unstaged_hunk_totals()?,
        Some(_) => ws.repo_hunk_totals()?,
    };

    let snapshot = MultiBuffer::singleton(ws.buffers.get(editor.buffer_id)?).snapshot();
    let diff_map = snapshot.diff_map.as_ref()?;
    let cursor_row = {
        let sel = editor.selections.newest_anchor();
        let offset = stoat_text::cursor_offset(
            snapshot.rope(),
            snapshot.resolve_anchor(&sel.tail()),
            snapshot.resolve_anchor(&sel.head()),
        );
        snapshot.rope().offset_to_point(offset).row
    };

    let live = diff_map.live_hunks(&snapshot);
    let (before, total, files_before, files_other) = totals.iter().fold(
        (0, live.change_count(), 0, 0),
        |(before, total, files_before, files_other), (path, count)| {
            let changed = usize::from(*count > 0);
            match path.cmp(&relative) {
                Ordering::Less => (
                    before + count,
                    total + count,
                    files_before + changed,
                    files_other + changed,
                ),
                // The live map already answered for this file.
                Ordering::Equal => (before, total, files_before, files_other),
                Ordering::Greater => (before, total + count, files_before, files_other + changed),
            }
        },
    );

    Some(HunkPosition {
        file_at: files_before + 1,
        file_total: files_other + 1,
        hunk_at: live
            .hunk_index_at(snapshot.rope(), cursor_row)
            .map(|k| before + k),
        hunk_total: total,
    })
}

/// The texts a file's diff measures its buffer against, as git last reported
/// them.
///
/// Each of them changes only with a write under `.git`, which is watched. A
/// diff recomputed for a keystroke therefore reuses what the last one read,
/// rather than taking the repo mutex and decompressing the same bytes again.
#[derive(Clone)]
pub(crate) struct DiffBaseText {
    /// The text the left column shows and the hunks describe.
    pub(crate) base: Arc<String>,
    /// The text a hunk must already sit in to read staged.
    staged_text: Arc<String>,
    /// The text behind the base. When present, the changes the base holds over
    /// it become the map's staged marks.
    pub(crate) staged_from: Option<Arc<String>>,
    /// Fingerprints of `base` and `staged_text`, taken once here.
    ///
    /// The staged text is usually a blob of its own that holds the base's
    /// bytes. Every recompute asks whether the two agree, to reuse one diff
    /// for both. Reading them to answer that costs a pass over the file per
    /// settle, where a blob only changes when something writes under `.git`.
    base_hash: [u8; 32],
    staged_hash: [u8; 32],
}

impl DiffBaseText {
    fn new(base: Arc<String>, staged_text: Arc<String>, staged_from: Option<Arc<String>>) -> Self {
        let base_hash = buffer_registry::fingerprint_bytes(&base);
        let staged_hash = buffer_registry::fingerprint_bytes(&staged_text);
        Self {
            base,
            staged_text,
            staged_from,
            base_hash,
            staged_hash,
        }
    }
}

/// Base texts each layer holds before the oldest goes.
///
/// A base outlives its use the moment HEAD moves past it, and nothing keyed by
/// content can know that, so the bound is what keeps the memo to the working
/// set. One entry per open buffer's live base plus headroom, which is what
/// [`DEFAULT_HIGHLIGHT_RETENTION`](crate::app::DEFAULT_HIGHLIGHT_RETENTION)
/// allows the same way.
const BASE_HIGHLIGHT_MEMO_CAPACITY: usize = 64;

/// Memoized diff-view base-text work, shared across the blocking jobs that
/// build diff maps.
///
/// Two layers, because they go stale on different inputs. Each is bounded at
/// [`BASE_HIGHLIGHT_MEMO_CAPACITY`] and drops its oldest entry to stay there.
///
/// A bound rather than an expiry, because a key is a content hash: nothing here
/// can tell a base still in use from one a commit moved past. Every commit that
/// touches an open file adds a pair, a review adds a pair per file per commit
/// stepped, and a theme reload gives each base recomputed after it a second
/// bucket.
#[derive(Default)]
pub(crate) struct BaseHighlightMemo {
    /// Highlight ids over every layer of a base text's parse, keyed by its
    /// content hash and language name, so an unchanged base is parsed once
    /// across edits. Theme-independent.
    parses: VecDeque<(ParseKey, Arc<HighlightIds>)>,
    /// Those spans resolved to styles and split per base line. Keyed
    /// additionally by the [`SyntaxStyles`] generation, since the resolution is
    /// what the theme changes.
    buckets: VecDeque<(BucketKey, Arc<BaseHighlights>)>,
}

/// A base text and the language that parsed it.
type ParseKey = (ContentHash, String);

/// Each capture's byte range in a base text and its highlight id, before a
/// theme resolves the id to a style.
type HighlightIds = Vec<(Range<usize>, HighlightId)>;

/// A [`ParseKey`] and the [`SyntaxStyles`] generation its spans resolved under.
type BucketKey = (ContentHash, String, u64);

impl BaseHighlightMemo {
    /// How many base texts each layer holds, at most
    /// [`BASE_HIGHLIGHT_MEMO_CAPACITY`].
    #[cfg(test)]
    fn len(&self) -> (usize, usize) {
        (self.parses.len(), self.buckets.len())
    }

    fn get_parse(&self, key: &ParseKey) -> Option<Arc<HighlightIds>> {
        entry_of(&self.parses, key)
    }

    fn insert_parse(&mut self, key: ParseKey, spans: Arc<HighlightIds>) {
        insert_bounded(&mut self.parses, key, spans);
    }

    fn get_bucket(&self, key: &BucketKey) -> Option<Arc<BaseHighlights>> {
        entry_of(&self.buckets, key)
    }

    fn insert_bucket(&mut self, key: BucketKey, bucketed: Arc<BaseHighlights>) {
        insert_bounded(&mut self.buckets, key, bucketed);
    }
}

/// The value `key` names, over at most [`BASE_HIGHLIGHT_MEMO_CAPACITY`] entries.
///
/// A scan that short costs less than the hash of the base text every caller has
/// already paid to build the key.
fn entry_of<K: PartialEq, V: Clone>(entries: &VecDeque<(K, V)>, key: &K) -> Option<V> {
    entries
        .iter()
        .find(|(entry, _)| entry == key)
        .map(|(_, value)| value.clone())
}

/// File `value` under `key`, dropping the oldest entry when the layer is full.
///
/// A key already present keeps the value it has. Two jobs can miss at once and
/// both compute, and the same key means the same value, so whichever landed
/// first stands.
fn insert_bounded<K: PartialEq, V>(entries: &mut VecDeque<(K, V)>, key: K, value: V) {
    if entries.iter().any(|(entry, _)| *entry == key) {
        return;
    }
    if entries.len() == BASE_HIGHLIGHT_MEMO_CAPACITY {
        entries.pop_front();
    }
    entries.push_back((key, value));
}

pub(crate) type BaseHighlightCache = Arc<Mutex<BaseHighlightMemo>>;

/// Compute a buffer's [`DiffMap`] against the texts in `base`.
///
/// The hunks describe what the buffer changes from the base text, and a hunk
/// reads staged when the staged text already holds it. With a `staged_from`
/// text, the changes the base text holds over it become the map's staged
/// marks.
///
/// The tree pass and the base-text highlights cost the most. A caller on the
/// UI thread passes no `language`, which skips both, so the line pass alone
/// lands in one frame.
///
/// Two differs answer two questions. The language-agnostic line pass owns the
/// extents and the staged marks, matching [`changed_byte_ranges`], so the
/// gutter and staging read line boundaries. With a `language` and a
/// `tree_memo`, a tree-sitter pass then owns the token detail, so a reindented
/// block washes what it actually changed rather than every line's prefix.
/// Without either, when either text is over
/// [`structural_diff::DEFAULT_BYTE_LIMIT`], or when the tree pass falls back to
/// lines, the line pass's char-refined spans stand. A file with no hunk skips
/// the tree pass, because the pass only details the line pass's hunks.
///
/// `buffer_tree` is a tree already parsed from `buffer_text`, which the tree
/// pass takes in place of parsing the buffer a second time. It must describe
/// exactly those bytes. A tree from another grammar is discarded downstream.
pub(super) fn compute_diff_map(
    buffer_text: &str,
    language: Option<&Arc<Language>>,
    syntax_styles: &SyntaxStyles,
    base_cache: &BaseHighlightCache,
    base: &DiffBaseText,
    tree_memo: Option<&TreeCache>,
    buffer_tree: Option<&Tree>,
) -> Option<DiffMap> {
    let base_text = &*base.base;
    let staged_text = &*base.staged_text;

    let result = structural_diff::diff(base_text, buffer_text);

    // Which buffer lines the staged text and the buffer disagree on, which is
    // what marks a hunk staged. With nothing staged the staged text holds the
    // base's bytes, so that question has the same answer as the diff just run,
    // and converting one result twice beats diffing the file twice.
    let staged_changed: Vec<Range<u32>> = {
        let hunks = if base.staged_hash == base.base_hash {
            changes_to_hunks(&result.changes, base_text, buffer_text)
        } else {
            let staged_result = structural_diff::diff(staged_text, buffer_text);
            changes_to_hunks(&staged_result.changes, staged_text, buffer_text)
        };
        hunks
            .into_iter()
            .map(|hunk| hunk.buffer_line_range)
            .collect()
    };

    // A whole-file removal is one change, not one per item the differ found
    // inside it. Left to the structural pass, a file holding two functions
    // tallies as two removals in the statusline.
    let had_content = !base_text.is_empty()
        || base
            .staged_from
            .as_deref()
            .is_some_and(|from| !from.is_empty());
    let mut diff_map = if buffer_removed(buffer_text) && had_content {
        if base_text.is_empty() {
            // The removal is already in the base, so the staged mark the
            // text behind it yields below is all there is to show.
            DiffMap::from_hunks([], Some(base.base.clone()))
        } else {
            let anchor = 0..0;
            // The removal is staged once the staged side is gone too. The
            // line differ answers this wrong. Against an empty staged text it
            // reads the removed buffer's lone newline as an added line.
            let unstaged_lines = match buffer_removed(staged_text) {
                true => Vec::new(),
                false => vec![anchor.clone()],
            };
            DiffMap::from_hunks(
                [DiffHunk {
                    status: DiffHunkStatus::Deleted,
                    buffer_start_line: 0,
                    buffer_line_range: anchor,
                    base_byte_range: 0..base_text.len(),
                    anchor_range: None,
                    token_detail: None,
                    unstaged_lines,
                    marked_rows: Vec::new(),
                }],
                Some(base.base.clone()),
            )
        }
    } else {
        let mut hunks = changes_to_hunks(&result.changes, base_text, buffer_text);
        mark_staged(&mut hunks, &staged_changed);

        // The tree pass answers only what the washes mark. It details only the
        // hunks the line pass found, so a file with no hunk skips it. A parse
        // failure or a fall back to lines leaves the line pass's char-refined
        // spans in place, which is what the view showed before this ran.
        if !hunks.is_empty()
            && let Some(language) = language
            && let Some(tree) = structural_diff::diff_with_language_cancellable(
                language,
                base_text,
                buffer_text,
                None,
                tree_memo,
                buffer_tree,
                MatchedPairs::Skip,
            )
            && !tree.fell_back_to_line_diff
        {
            merge_structural_detail(
                &mut hunks,
                &tree.changes,
                &tree.unsearched,
                base_text,
                buffer_text,
            );
        }
        DiffMap::from_hunks(hunks, Some(base.base.clone()))
    };
    if let Some(from) = &base.staged_from {
        diff_map.set_staged_marks(staged_marks(from, base_text));
    }
    if let Some(language) = language {
        // The left column paints the base's context lines with these colors, so
        // a file with no hunk still needs them. When the tree pass above ran,
        // it parsed this exact base into `tree_memo`, and this call reads that
        // tree. Otherwise this call parses the base itself.
        diff_map.set_base_highlights(compute_base_highlights(
            base_text,
            language,
            syntax_styles,
            base_cache,
            tree_memo,
        ));
    }
    Some(diff_map)
}

/// Read the texts a diff measures `path` against from the repo `git_root` is
/// in, per the workspace's base.
///
/// A pair base answers for its file with no repository. Otherwise [`None`]
/// when `git_root` is in no repo, or as [`base_texts`] answers.
fn resolve_base(
    git: &dyn GitHost,
    git_root: &Path,
    path: &Path,
    base_override: Option<&DiffBase>,
) -> Option<DiffBaseText> {
    if let Some(base) = pair_base(path, base_override) {
        return Some(base);
    }
    let repo = git.discover(git_root)?;
    base_texts(&*repo, path, base_override)
}

/// The texts `path` diffs against under `base_override`.
///
/// The base side is what the diff hunks describe. The staged side is what marks
/// them staged, being the content a hunk is already applied to.
///
/// With no override the base is the index and HEAD sits behind it. The hunks
/// are the working tree's unstaged edits, and the changes the index holds over
/// HEAD become staged marks. Every hunk reads unstaged, because the staged side
/// is the base itself.
///
/// Under [`DiffBase::Head`] the base is HEAD and the index marks which hunks are
/// staged.
///
/// Under a [`DiffBase::Rev`] the rev is the review base and HEAD is the commit
/// checked out over it, so a hunk already in that commit reads staged and a
/// worktree-only edit reads unstaged. Either side missing the file reads as
/// empty text rather than abandoning the diff, so a file the commit adds still
/// diffs against nothing.
///
/// A range review ([`RevOrigin::Review`]) reads the rev on both sides, so no
/// hunk reads staged. Every hunk of a range sits in the checked-out tip. With
/// HEAD on the staged side every hunk reads staged, which tells the reader
/// nothing.
///
/// Under a [`DiffBase::Memory`] both sides are the supplied text, which leaves
/// every hunk unstaged. Nothing has applied an agent's proposal anywhere. A path
/// the map does not carry diffs against the index, as with no override.
///
/// Under a [`DiffBase::Pair`] the measured file reads the other file's text on
/// both sides. A path outside the pair diffs against the index, as with no
/// override.
///
/// An untracked file reads as empty text on both sides, so every line is an
/// added, unstaged hunk. [`None`] only for a path outside the workdir or one
/// the repo ignores, which is what leaves such a buffer without a diff map.
pub(crate) fn base_texts(
    repo: &dyn GitRepo,
    path: &Path,
    base_override: Option<&DiffBase>,
) -> Option<DiffBaseText> {
    match base_override {
        Some(DiffBase::Rev { sha, origin }) => {
            let rev = Arc::new(match sha {
                Some(sha) => repo.content_at(sha, path).unwrap_or_default(),
                None => String::new(),
            });
            let staged = match origin {
                RevOrigin::Review { .. } => rev.clone(),
                RevOrigin::Named(_) | RevOrigin::ParentOf(_) => {
                    Arc::new(repo.head_content(path).unwrap_or_default())
                },
            };
            Some(DiffBaseText::new(rev, staged, None))
        },
        Some(DiffBase::Head) => {
            let (head, index) = working_tree_texts(repo, path)?;
            Some(DiffBaseText::new(head, index, None))
        },
        Some(DiffBase::Memory { files }) => match files.get(path) {
            Some(text) => Some(DiffBaseText::new(text.clone(), text.clone(), None)),
            None => index_base(repo, path),
        },
        Some(DiffBase::Pair { .. }) => {
            pair_base(path, base_override).or_else(|| index_base(repo, path))
        },
        None => index_base(repo, path),
    }
}

/// The base a [`DiffBase::Pair`] supplies for `path`, and `None` for every
/// other path and every other base.
///
/// Both sides are the supplied text, so every hunk reads unstaged, since no
/// index holds any of it.
fn pair_base(path: &Path, base_override: Option<&DiffBase>) -> Option<DiffBaseText> {
    match base_override {
        Some(DiffBase::Pair {
            path: pair_path,
            text,
            ..
        }) if pair_path == path => Some(DiffBaseText::new(text.clone(), text.clone(), None)),
        _ => None,
    }
}

/// The base `path` diffs against with no override, the index with HEAD behind
/// it.
fn index_base(repo: &dyn GitRepo, path: &Path) -> Option<DiffBaseText> {
    let (head, index) = working_tree_texts(repo, path)?;
    Some(DiffBaseText::new(index.clone(), index, Some(head)))
}

/// HEAD's and the index's text for `path`, as `(head, index)`.
///
/// A side with no blob reads as empty text. A tracked file with no index entry
/// is staged for deletion, and a file added to the index has no HEAD blob yet.
///
/// A file with no HEAD blob is looked up as the new side of a move before it
/// reads as added. `git mv` also puts the new path in the index, and a read of
/// that alone shows every line as a staged addition.
///
/// An untracked file, which neither side carries and no move explains, reads
/// as empty text on both sides, so every line is an added, unstaged hunk.
/// [`None`] only for a path outside the workdir or one the repo ignores.
fn working_tree_texts(repo: &dyn GitRepo, path: &Path) -> Option<(Arc<String>, Arc<String>)> {
    if let Some(head) = repo.head_content(path) {
        let index = repo.index_content(path).unwrap_or_default();
        return Some((Arc::new(head), Arc::new(index)));
    }
    if let Some(moved) = moved_base(repo, path) {
        return Some(moved);
    }
    if let Some(index) = repo.index_content(path) {
        return Some((Arc::default(), Arc::new(index)));
    }
    untracked_in_tree(repo, path).then(|| (Arc::default(), Arc::default()))
}

/// Whether `path` lies inside the repo's workdir with no ignore rule over it.
///
/// A file no blob holds is new work only there. The two files that keep no
/// diff are an ignored file, such as a build artifact or a session log under
/// an excluded directory, and a path outside the workdir. None of their lines
/// is work.
pub(crate) fn untracked_in_tree(repo: &dyn GitRepo, path: &Path) -> bool {
    repo.workdir()
        .is_some_and(|workdir| path.starts_with(&workdir))
        && !repo.is_path_ignored(path)
}

/// HEAD's and the index's text for a moved `path`, with HEAD read from the
/// blob it was moved from.
///
/// A move leaves no blob under the new path. A base read there alone shows
/// every line as added, and the old path shows every line as deleted.
///
/// [`None`] when `path` is not the new side of a move, which is the ordinary
/// answer for an untracked file. [`working_tree_texts`] then reads such a file
/// as new.
fn moved_base(repo: &dyn GitRepo, path: &Path) -> Option<(Arc<String>, Arc<String>)> {
    let old = repo.rename_source(path)?;
    let head = repo.head_content(&old)?;
    // A staged rename carries the new path in the index, while a plain
    // `fs::rename` leaves the index entry at the old one.
    let index = repo
        .index_content(path)
        .or_else(|| repo.index_content(&old))
        .unwrap_or_default();
    Some((Arc::new(head), Arc::new(index)))
}

/// Whether `buffer_text` stands for a file that is no longer in the working
/// tree.
///
/// A removed file still opens. The read misses and the open path substitutes a
/// lone newline, so the buffer holds `"\n"` rather than nothing. A test for an
/// empty string alone therefore misses every real removal and catches only a
/// file that exists holding zero bytes.
pub(crate) fn buffer_removed(buffer_text: &str) -> bool {
    buffer_text.is_empty() || buffer_text == "\n"
}

/// Highlight `base_text` for the diff view's left column, per base line.
///
/// Memoized against `cache` twice over. An unchanged base text is parsed once,
/// and its spans are resolved and bucketed once per theme, so a keystroke burst
/// that leaves the base alone costs two hash lookups rather than a walk over
/// every span with a style clone per line it touches.
///
/// `tree_memo` is the memo the structural diff parses its base into. Sharing it
/// costs a caller that already diffed this text nothing, because the tree is
/// then already there. Pass [`None`] and the parse happens here.
pub(crate) fn compute_base_highlights(
    base_text: &str,
    language: &Arc<Language>,
    syntax_styles: &SyntaxStyles,
    cache: &BaseHighlightCache,
    tree_memo: Option<&TreeCache>,
) -> Arc<BaseHighlights> {
    let content: ContentHash = blake3::hash(base_text.as_bytes()).into();
    let name = language.name.to_string();
    let bucket_key = (content, name.clone(), syntax_styles.generation);

    let parse_key = (content, name);
    let hit = {
        let guard = cache.lock().expect("base highlight cache poisoned");
        if let Some(bucketed) = guard.get_bucket(&bucket_key) {
            return bucketed;
        }
        guard.get_parse(&parse_key)
    };

    // Parsed outside the lock, which a miss holds only long enough to look up.
    // A workspace runs one diff job per stale buffer on the blocking pool, all
    // sharing this memo, and parsing under the lock queues every one of them
    // behind whichever job got there first.
    //
    // Two jobs missing at once both parse, which is the price. Same content and
    // same language means the same spans, so whichever lands first is kept and
    // the other is dropped.
    let spans = match hit {
        Some(spans) => spans,
        None => {
            let parsed = Arc::new(
                structural_diff::parse_memoized(language, base_text, tree_memo)
                    .map(|tree| layered_highlight_ids(language, &tree, base_text))
                    .unwrap_or_default(),
            );
            let mut guard = cache.lock().expect("base highlight cache poisoned");
            guard.insert_parse(parse_key.clone(), parsed.clone());
            guard.get_parse(&parse_key).unwrap_or(parsed)
        },
    };

    // Bucketed outside the lock, so one job's O(spans) resolve does not hold up
    // every other buffer's diff.
    let bucketed = Arc::new(bucket_base_highlights(&spans, base_text, syntax_styles));
    cache
        .lock()
        .expect("base highlight cache poisoned")
        .insert_bucket(bucket_key, bucketed.clone());
    bucketed
}

/// Highlight ids over every layer of `base_text`'s parse, host and injections
/// alike, which is how the buffer's own highlights read its text.
///
/// `tree` is the host parse, reused as the host layer, so only the injected
/// layers parse here. A reparse that gives up yields no ids, which paints the
/// base text plain.
fn layered_highlight_ids(language: &Arc<Language>, tree: &Tree, base_text: &str) -> HighlightIds {
    let rope = Rope::from(base_text);
    let mut map = SyntaxMap::default();
    if map
        .reparse(&rope, language.clone(), 0, Some(tree))
        .is_none()
    {
        return Vec::new();
    }
    syntax_parse::capture_highlight_ids(
        map.snapshot()
            .captures(0..rope.len(), &rope, |l| Some(l.highlight_query())),
    )
}

/// Resolve highlight ids to styles and bucket them per base line as line-local
/// byte ranges. A span crossing a newline is clipped to each line it touches.
///
/// Each line comes out as disjoint, start-sorted runs per [`flatten_line_spans`].
/// The left column's painter answers the first span still open at a byte, so an
/// overlap left in place hands every token the least specific capture over it.
fn bucket_base_highlights(
    spans: &[(Range<usize>, HighlightId)],
    base_text: &str,
    syntax_styles: &SyntaxStyles,
) -> BaseHighlights {
    let starts = line_starts(base_text);
    let line_of = |byte: usize| starts.partition_point(|&s| s <= byte).saturating_sub(1);

    let mut per_line: BaseHighlights = vec![Vec::new(); starts.len()];
    for (range, id) in spans {
        let Some(style_id) = syntax_styles.id_for_highlight(*id) else {
            continue;
        };
        let style = syntax_styles.interner[style_id].clone();

        let first = line_of(range.start);
        let last = line_of(range.end.saturating_sub(1).max(range.start));
        for line in first..=last {
            let line_start = starts[line];
            let line_end = starts.get(line + 1).copied().unwrap_or(base_text.len());
            let s = range.start.max(line_start) - line_start;
            let e = range.end.min(line_end) - line_start;
            if s < e {
                per_line[line].push((s..e, style.clone()));
            }
        }
    }

    for spans in &mut per_line {
        flatten_line_spans(spans);
    }
    per_line
}

/// Flatten one line's overlapping spans into disjoint, start-sorted runs, each
/// styled the way the right column styles the same bytes.
///
/// The spans sort into the order
/// [`SyntaxSnapshot::captures`](stoat_language::SyntaxSnapshot::captures) gives
/// the right column. Document order comes first, and the outer of two spans at
/// one start comes before the inner. The sort is stable, so spans over the same
/// bytes keep the query's pattern order.
///
/// A run between two span boundaries merges the open spans' styles in that
/// order, which is how
/// [`highlighted_chunks`](crate::display_map::highlights::highlighted_chunks)
/// merges the right column's open tokens. A later, more specific capture
/// therefore overrides the fields it sets and keeps the rest.
fn flatten_line_spans(spans: &mut Vec<(Range<usize>, HighlightStyle)>) {
    if spans.len() < 2 {
        return;
    }
    spans.sort_by_key(|(range, _)| (range.start, Reverse(range.end)));

    let mut bounds: Vec<usize> = spans
        .iter()
        .flat_map(|(range, _)| [range.start, range.end])
        .collect();
    bounds.sort_unstable();
    bounds.dedup();

    let mut open: Vec<usize> = Vec::new();
    let mut next = 0;
    let mut runs = Vec::with_capacity(spans.len());
    for pair in bounds.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        open.retain(|&i| spans[i].0.end > start);
        while spans
            .get(next)
            .is_some_and(|(range, _)| range.start == start)
        {
            open.push(next);
            next += 1;
        }
        if open.is_empty() {
            continue;
        }

        let mut style = HighlightStyle::default();
        for &i in &open {
            style.merge(&spans[i].1);
        }
        runs.push((start..end, style));
    }
    *spans = runs;
}

/// The working-tree byte ranges a file's hunks cover, diffing its HEAD text
/// against its working-tree text.
///
/// Hunk line ranges are converted to byte ranges in the working-tree text
/// so a symbol's byte def-range can be tested for overlap directly.
///
/// A deletion has no working-tree lines of its own, so it yields an empty range
/// at the seam it was removed from. That range is kept rather than dropped. The
/// overlap test the caller applies treats it as a point, which is how a
/// deletion marks the symbol it was cut out of.
///
/// Uses the line diff rather than the language-aware structural diff. The only
/// consumer tests whole-line overlap, and treating moved code as a delete plus
/// an add yields the same or a strictly larger changed set for that test, at a
/// fraction of the cost.
/// Diff every changed file against HEAD and collect the byte ranges its hunks
/// cover, reusing `memo` for a file whose base and buffer text both still hash
/// to what the ranges were measured from.
///
/// Free of the workspace so it runs on a blocking thread. The status walk, the
/// HEAD blobs, and a disk read per changed file are the whole cost of a
/// diff-filtered hop. [`Workspace::install_changed_ranges`] takes the result.
pub(crate) fn scan_changed_ranges(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_root: &Path,
    memo: &ChangedRangesMemo,
) -> ChangedRangesScan {
    let mut scan = ChangedRangesScan::default();
    let Some((_workdir, inputs)) = diff::scan_working_tree(git, fs, langs, git_root) else {
        return scan;
    };

    for input in &inputs {
        let fid = file_id(&input.rel_path);
        let base_hash = buffer_registry::fingerprint_bytes(input.base_text.as_str());
        let buffer_hash = buffer_registry::fingerprint_bytes(input.buffer_text.as_str());

        let ranges = match memo.get(&fid) {
            Some((cached_base, cached_buffer, cached))
                if *cached_base == base_hash && *cached_buffer == buffer_hash =>
            {
                cached.clone()
            },
            _ => {
                let computed = changed_byte_ranges(input);
                scan.computed
                    .push((fid, base_hash, buffer_hash, computed.clone()));
                computed
            },
        };

        if !ranges.is_empty() {
            scan.ranges.insert(fid, ranges);
        }
    }
    scan
}

fn changed_byte_ranges(input: &ReviewFileInput) -> Vec<Range<usize>> {
    let result = structural_diff::diff(&input.base_text, &input.buffer_text);
    let hunks = changes_to_hunks(&result.changes, &input.base_text, &input.buffer_text);
    let starts = line_starts(&input.buffer_text);

    let offset_of_row = |row: u32| {
        starts
            .get(row as usize)
            .copied()
            .unwrap_or(input.buffer_text.len())
    };

    hunks
        .iter()
        .map(|hunk| {
            offset_of_row(hunk.buffer_line_range.start)..offset_of_row(hunk.buffer_line_range.end)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        changed_byte_ranges, compute_base_highlights, compute_diff_map, diff_sides,
        repo_hunk_position, resolve_base, scan_changed_ranges, BaseHighlightCache,
        BaseHighlightMemo, DiffBase, DiffBaseText, DiffJob, DiffLanding, DiffState, HunkPosition,
        WorktreeBase, DIFF_SETTLE,
    };
    use crate::{
        action_handlers::{self, movement},
        app,
        buffer::BufferId,
        diff_map::{BaseHighlights, DiffHunk, DiffHunkStatus, DiffMap, StagedMark},
        display_map::syntax_theme::SyntaxStyles,
        host::DiffStatus,
        pane::View,
        review::ReviewFileInput,
        test_harness::TestHarness,
        theme::Theme,
        workspace::Workspace,
    };
    use futures::future;
    use ratatui::style::Color;
    use std::{
        collections::HashMap,
        ops::Range,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use stoat_action::{DiffAgainstIndex, ToggleStageHunk};
    use stoat_language::{parse, structural_diff::TreeCache, Language, LanguageRegistry};

    /// The bar reads the totals and a file list reads the per-file counts, so
    /// both come off one stored tally rather than two walks that could disagree.
    #[test]
    fn the_stored_tally_answers_both_the_totals_and_the_per_file_counts() {
        let mut h = TestHarness::with_size(40, 12);
        let workdir = PathBuf::from("/tally");
        h.stage_review_scenario(
            &workdir,
            &[("a.rs", "one\n", "ONE\n"), ("b.rs", "two\n", "TWO\n")],
        );
        h.fake_git().add_repo(&workdir).hunks("a.rs", 3);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("a.rs"));
        h.settle_diff_jobs();

        let ws = h.stoat.active_workspace();
        assert_eq!(
            ws.repo_change_counts(),
            Some((0, 4)),
            "the totals are the hunks each side carries",
        );
        assert_eq!(
            ws.repo_hunk_totals(),
            Some([(PathBuf::from("a.rs"), 3), (PathBuf::from("b.rs"), 1)].as_slice()),
            "and the per-file counts come off the same tally",
        );
    }

    /// An untracked file has no HEAD blob, so its base resolves through a
    /// rename lookup, which the local backend answers with a working-tree stat
    /// walk and two rename passes. Only a git write turns a path into a rename
    /// target, so that answer holds until one does.
    #[test]
    fn an_untracked_buffer_asks_for_its_base_once() {
        let mut h = TestHarness::with_size(40, 12);
        let workdir = PathBuf::from("/untracked");
        h.stage_review_scenario(&workdir, &[("tracked.rs", "one\n", "ONE\n")]);
        // Written straight into the fake fs, so it exists on disk with no
        // registration in the repo, which is what untracked means.
        let fresh = workdir.join("fresh.rs");
        h.fake_fs().insert_file(&fresh, b"new\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&fresh);
        h.settle_diff_jobs();

        let after_open = h.fake_git().rename_source_calls(&workdir);
        assert!(after_open > 0, "the first settle asks once");

        h.type_keys("i x <esc>");
        h.settle_diff_jobs();

        assert_eq!(
            h.fake_git().rename_source_calls(&workdir),
            after_open,
            "and the recorded base answers every settle after it",
        );
    }

    /// A range review reads its merge base on both sides, so no hunk of the
    /// range reads staged. A commit review keeps HEAD on the staged side.
    #[test]
    fn a_range_review_reads_its_base_on_both_sides() {
        let h = TestHarness::with_size(40, 12);
        h.fake_git()
            .add_repo("/repo")
            .commit("b1", &[("a.rs", "base\n")])
            .head_file("a.rs", "tip\n");
        let texts = |base: DiffBase| {
            let texts = resolve_base(
                &**h.fake_git(),
                Path::new("/repo"),
                Path::new("/repo/a.rs"),
                Some(&base),
            )
            .expect("a file in the repo");
            (texts.base.to_string(), texts.staged_text.to_string())
        };
        assert_eq!(
            [
                texts(DiffBase::review("b1".to_string(), "feature", "main")),
                texts(DiffBase::parent_of("t1", Some("b1".to_string()))),
            ],
            [
                ("base\n".to_string(), "base\n".to_string()),
                ("base\n".to_string(), "tip\n".to_string()),
            ],
        );
    }

    #[test]
    fn a_pair_base_answers_for_its_file_alone() {
        let mut h = TestHarness::with_size(40, 12);
        h.stage_review_scenario("/repo", &[("t.txt", "old\n", "new\n")]);
        let pair = DiffBase::Pair {
            path: "/pair/b.txt".into(),
            base_path: "/pair/a.txt".into(),
            text: Arc::new("one\n".to_string()),
        };
        let resolved = |root: &str, path: &str| {
            resolve_base(
                &**h.fake_git(),
                Path::new(root),
                Path::new(path),
                Some(&pair),
            )
            .map(|texts| texts.base.to_string())
        };
        assert_eq!(
            [
                resolved("/pair", "/pair/b.txt"),
                resolved("/pair", "/pair/a.txt"),
                resolved("/repo", "/repo/t.txt"),
            ],
            [Some("one\n".to_string()), None, Some("old\n".to_string())],
            "the pair's file reads the other file with no repository, and every other path \
             keeps its own base",
        );
    }

    /// A repo-wide tally is a walk of every diff in the repository, and an
    /// unsaved edit has not touched the disk it reads, so a settle must not buy
    /// one. A landed save has touched it, so that one must.
    #[test]
    fn only_a_disk_write_buys_another_repo_tally() {
        let mut h = TestHarness::with_size(40, 12);
        let workdir = PathBuf::from("/tally");
        h.stage_review_scenario(&workdir, &[("a.rs", "one\n", "ONE\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("a.rs"));
        h.settle_diff_jobs();

        let after_open = h.fake_git().tally_calls(&workdir);
        assert!(after_open > 0, "the first open reads one");

        h.type_keys("i x <esc>");
        h.settle_diff_jobs();
        assert_eq!(
            h.fake_git().tally_calls(&workdir),
            after_open,
            "an unsaved edit cannot move the tally, so it buys no walk",
        );

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveBuffer);
        h.settle_diff_jobs();
        assert_eq!(
            h.fake_git().tally_calls(&workdir),
            after_open + 1,
            "and a landed save buys exactly one",
        );
    }

    /// The reader walks the focused file's own map, so the position counts
    /// through it while the stored tally answers for the files ahead of it.
    /// The two disagree for a beat after an edit, and the map is the half that
    /// matches what is on screen.
    #[test]
    fn the_hunk_position_counts_the_focused_file_through_its_own_map() {
        let mut h = TestHarness::with_size(40, 16);
        let workdir = PathBuf::from("/position");
        let working: String = (0..12).map(|n| format!("line {n}\n")).collect();
        let head = working.replace("line 2\n", "old 2\n");
        h.stage_review_scenario(
            &workdir,
            &[("a.rs", "one\n", "ONE\n"), ("b.rs", &head, &working)],
        );
        h.fake_git().add_repo(&workdir).hunks("a.rs", 3);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("b.rs"));
        h.settle_diff_jobs();

        let editor_id = {
            let ws = h.stoat.active_workspace();
            let View::Editor(id) = ws.panes.pane(ws.panes.focus()).view else {
                panic!("the opened file focuses an editor pane");
            };
            id
        };
        let buffer_id = {
            let ws = h.stoat.active_workspace_mut();
            ws.editors[editor_id].set_diff_view(true);
            ws.editors[editor_id].buffer_id
        };
        let install = |h: &mut TestHarness| {
            let map = DiffMap::from_hunks([added_hunk(2..3), added_hunk(8..9)], None);
            h.stoat
                .active_workspace_mut()
                .install_test_diff_map(buffer_id, map);
        };

        let at = |hunk_at| HunkPosition {
            file_at: 2,
            file_total: 2,
            hunk_at,
            hunk_total: 5,
        };

        install(&mut h);
        assert_eq!(
            repo_hunk_position(h.stoat.active_workspace()),
            Some(at(None)),
            "b.rs is the second of the two changed files, the cursor opens above \
             its first hunk, and the total counts a.rs through the tally and b.rs \
             through its map",
        );

        h.type_keys("j j j j j j j j");
        install(&mut h);
        assert_eq!(
            repo_hunk_position(h.stoat.active_workspace()),
            Some(at(Some(5))),
            "the second of b.rs's own hunks, offset by the three a.rs carries",
        );
    }

    /// A removal of a file's last lines, in a file with no trailing newline,
    /// leaves no row after it. The walk lands on the last row, and the position
    /// names that removal rather than the hunk before it.
    #[test]
    fn a_landing_on_a_trailing_removal_has_a_position() {
        let mut h = TestHarness::with_size(40, 16);
        let workdir = PathBuf::from("/position");
        h.stage_review_scenario(&workdir, &[("b.rs", "a\nb\nc\nd\n", "a\nb\nc")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("b.rs"));
        h.settle_diff_jobs();

        let buffer_id = {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            editor.set_diff_view(true);
            editor.buffer_id
        };
        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(buffer_id, DiffMap::from_hunks([deleted_hunk(3)], None));

        movement::goto_change_impl(&mut h.stoat, movement::ChangeDir::Next, 1);
        assert_eq!(
            repo_hunk_position(h.stoat.active_workspace()),
            Some(HunkPosition {
                file_at: 1,
                file_total: 1,
                hunk_at: Some(1),
                hunk_total: 1,
            }),
            "the walk lands the removal on row 2, and the position counts it",
        );
    }

    /// Two edits that only a blank line separates are one change, the one the
    /// walk crosses in one press, so the position counts them once.
    #[test]
    fn two_edits_around_a_blank_line_count_as_one_change() {
        let mut h = TestHarness::with_size(40, 16);
        let workdir = PathBuf::from("/position");
        h.stage_review_scenario(&workdir, &[("b.rs", "a\nb\n\nc\nd\n", "a\nB\n\nC\nd\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("b.rs"));
        h.settle_diff_jobs();

        let buffer_id = {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            editor.set_diff_view(true);
            editor.buffer_id
        };
        h.type_keys("j j j");
        h.stoat.active_workspace_mut().install_test_diff_map(
            buffer_id,
            DiffMap::from_hunks([added_hunk(1..2), added_hunk(3..4)], None),
        );

        assert_eq!(
            repo_hunk_position(h.stoat.active_workspace()),
            Some(HunkPosition {
                file_at: 1,
                file_total: 1,
                hunk_at: Some(1),
                hunk_total: 1,
            }),
            "the cursor on the second edit stands on change one of one",
        );
    }

    /// Under the index base the position counts the stops the diff view's walk
    /// makes, and a file whose changes are all staged offers none.
    #[test]
    fn the_hunk_position_under_the_index_base_leaves_staged_changes_out() {
        let mut h = TestHarness::with_size(40, 16);
        let workdir = PathBuf::from("/position");
        h.stage_review_scenario(&workdir, &[("b.rs", "one\n", "ONE\n")]);
        h.fake_git().add_repo(&workdir).staged_file("a.rs", "x\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("b.rs"));
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Index);
        h.settle_diff_jobs();
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);

        let index = repo_hunk_position(h.stoat.active_workspace());
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Head);
        h.settle_diff_jobs();
        let head = repo_hunk_position(h.stoat.active_workspace());

        let only = |file_at, at_and_total| HunkPosition {
            file_at,
            file_total: file_at,
            hunk_at: Some(at_and_total),
            hunk_total: at_and_total,
        };
        assert_eq!(
            [index, head],
            [Some(only(1, 1)), Some(only(2, 2))],
            "the staged a.rs counts as a changed file and a hunk under HEAD alone",
        );
    }

    /// The tally lists a pure move with no hunks. Such a file is no stop for
    /// the walk, so it neither moves the focused file's place nor joins the
    /// count.
    #[test]
    fn the_file_place_counts_only_files_with_hunks() {
        let mut h = TestHarness::with_size(40, 16);
        let workdir = PathBuf::from("/position");
        h.stage_review_scenario(
            &workdir,
            &[
                ("a.rs", "one\n", "ONE\n"),
                ("a_moved.rs", "two\n", "TWO\n"),
                ("b.rs", "three\n", "THREE\n"),
                ("c.rs", "four\n", "FOUR\n"),
            ],
        );
        h.fake_git().add_repo(&workdir).hunks("a_moved.rs", 0);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("b.rs"));
        h.settle_diff_jobs();
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);

        let position =
            repo_hunk_position(h.stoat.active_workspace()).expect("the diff view has a position");
        assert_eq!(
            (position.file_at, position.file_total),
            (2, 3),
            "a.rs stands before b.rs and c.rs after it",
        );
    }

    fn added_hunk(line_range: Range<u32>) -> DiffHunk {
        DiffHunk {
            status: DiffHunkStatus::Added,
            unstaged_lines: vec![line_range.clone()],
            marked_rows: Vec::new(),
            buffer_start_line: line_range.start,
            buffer_line_range: line_range,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        }
    }

    /// A removal that marks buffer `row`, the row after the removed lines.
    fn deleted_hunk(row: u32) -> DiffHunk {
        let rows = row..row;
        DiffHunk {
            status: DiffHunkStatus::Deleted,
            unstaged_lines: vec![rows.clone()],
            marked_rows: Vec::new(),
            buffer_start_line: row,
            buffer_line_range: rows,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        }
    }

    /// A base outlives its use the moment HEAD moves past it, and a key of
    /// content hash and language cannot tell the two apart. The bound is what
    /// keeps a session's memo to its working set.
    #[test]
    fn the_base_highlight_memo_holds_its_capacity() {
        let language = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .expect("rust language");
        let styles = SyntaxStyles::from_theme(&Theme::empty());
        let cache: BaseHighlightCache = Arc::new(Mutex::new(BaseHighlightMemo::default()));
        let lengths = || cache.lock().expect("cache poisoned").len();

        let base = |i: usize| format!("fn main() {{\n    let x = {i};\n}}\n");
        let filed = |text: &str| {
            cache
                .lock()
                .expect("cache poisoned")
                .get_parse(&(
                    blake3::hash(text.as_bytes()).into(),
                    language.name.to_string(),
                ))
                .is_some()
        };

        let first = base(0);
        compute_base_highlights(&first, &language, &styles, &cache, None);
        assert!(filed(&first), "the first base is filed");

        // A rebuilt style table misses the bucket, so the second call reaches
        // the parse layer rather than returning ahead of it. A tree memo of its
        // own is what shows whether it parsed: a parse would file a tree there,
        // and a hit on the spans never calls the parser at all.
        let rebuilt = SyntaxStyles::from_theme(&Theme::empty());
        let second_trees = TreeCache::default();
        compute_base_highlights(&first, &language, &rebuilt, &cache, Some(&second_trees));
        assert!(
            second_trees.lock().expect("tree memo poisoned").is_empty(),
            "a base the memo still holds takes no second parse",
        );

        // One past the capacity, so the first base is the one evicted.
        for i in 1..=super::BASE_HIGHLIGHT_MEMO_CAPACITY {
            compute_base_highlights(&base(i), &language, &styles, &cache, None);
        }

        assert_eq!(
            lengths(),
            (
                super::BASE_HIGHLIGHT_MEMO_CAPACITY,
                super::BASE_HIGHLIGHT_MEMO_CAPACITY
            ),
            "both layers stop at the capacity rather than growing with the bases",
        );
        assert!(!filed(&first), "and the oldest base is the one that went",);
    }

    fn diff_base(base: &str, staged_text: &str, staged_from: Option<&str>) -> DiffBaseText {
        let text = |text: &str| Arc::new(text.to_string());
        DiffBaseText::new(text(base), text(staged_text), staged_from.map(text))
    }

    /// A diff map parses its base for the structural pass and again for the
    /// left column's colors, which is 4 ms twice on a large file. One memo
    /// serves both, so the second pass finds the tree the first left.
    ///
    /// Two things have to hold. The highlight pass must reach the memo at all,
    /// which an empty one filling by that call alone shows. Both passes must
    /// then key the same base the same way, which the whole diff leaving one
    /// entry shows.
    #[test]
    fn the_base_highlights_and_the_structural_pass_share_one_parse() {
        let language = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .expect("rust language");
        let styles = SyntaxStyles::from_theme(&Theme::empty());
        let cache: BaseHighlightCache = Arc::new(Mutex::new(BaseHighlightMemo::default()));
        let memo_len = |memo: &TreeCache| memo.lock().expect("tree memo poisoned").len();

        let head = "fn main() {\n    let x = 1;\n}\n";
        let buffer = "fn main() {\n    let x = 41;\n}\n";

        let highlights_only = TreeCache::default();
        compute_base_highlights(head, &language, &styles, &cache, Some(&highlights_only));
        assert_eq!(
            memo_len(&highlights_only),
            1,
            "the highlight parse goes through the shared memo",
        );

        let base = diff_base(head, head, None);
        let whole_diff = TreeCache::default();
        compute_diff_map(
            buffer,
            Some(&language),
            &styles,
            // A fresh style cache, so the highlight pass reaches its parse
            // instead of answering from the spans the call above filed.
            &Arc::new(Mutex::new(BaseHighlightMemo::default())),
            &base,
            Some(&whole_diff),
            None,
        )
        .expect("a tracked file has a diff map");
        assert_eq!(
            memo_len(&whole_diff),
            1,
            "both passes key one base the same way",
        );
    }

    /// The diff job hands the tree pass the tree the highlight pipeline
    /// already built for the buffer, which is the whole point of the
    /// parameter. If a supplied tree changes the hunks, the diff a reader sees
    /// depends on whether a parse happened to have landed.
    #[test]
    fn a_supplied_buffer_tree_yields_the_hunks_a_fresh_parse_does() {
        let language = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .expect("rust language");
        let styles = SyntaxStyles::from_theme(&Theme::empty());
        let cache: BaseHighlightCache = Arc::new(Mutex::new(BaseHighlightMemo::default()));

        let head = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let buffer = "fn main() {\n    let x = 41;\n    let y = 2;\n}\n";
        let base = diff_base(head, head, None);

        let tree = parse(&language, buffer, None).expect("buffer parses");
        let hunks = |buffer_tree| {
            compute_diff_map(
                buffer,
                Some(&language),
                &styles,
                &cache,
                &base,
                None,
                buffer_tree,
            )
            .expect("a tracked file has a diff map")
            .hunks()
            .cloned()
            .collect::<Vec<_>>()
        };

        assert_eq!(
            hunks(Some(&tree)),
            hunks(None),
            "the supplied tree stands in for the parse, it does not change the diff",
        );

        // A language override moves a buffer's tree away from the grammar the
        // path selects, and tree-sitter aborts the process rather than reject
        // the mismatch. Discarding the tree is what keeps that unreachable.
        let foreign = LanguageRegistry::standard()
            .for_path(Path::new("a.json"))
            .expect("json language");
        let foreign_tree = parse(&foreign, buffer, None).expect("buffer parses as json");
        assert_eq!(
            hunks(Some(&foreign_tree)),
            hunks(None),
            "a tree from another grammar is discarded, not diffed against",
        );
    }

    /// A file equal to its base skips the tree pass, which has no hunk to
    /// detail. The left column still paints the base's colors, so the base
    /// highlights must not ride on that pass.
    #[test]
    fn an_unchanged_file_still_gets_its_base_highlights() {
        let (styles, language) = themed_language(
            "a.rs",
            r##"theme t {
                syntax.keyword.fg = "#050505";
            }"##,
        );
        let text = "fn main() {}\n";

        let diff_map = compute_diff_map(
            text,
            Some(&language),
            &styles,
            &Arc::new(Mutex::new(BaseHighlightMemo::default())),
            &diff_base(text, text, None),
            Some(&TreeCache::default()),
            None,
        )
        .expect("a tracked file has a diff map");

        let first_token = diff_map
            .base_highlights_for_line(0)
            .and_then(|spans| spans.iter().find(|(range, _)| range.contains(&0)))
            .map(|(range, style)| (range.clone(), style.foreground));
        assert_eq!(
            (diff_map.hunks().count(), first_token),
            (0, Some((0..2, Some(Color::Rgb(5, 5, 5))))),
            "the file has no hunk, and its base `fn` still paints as a keyword",
        );
    }

    /// The map the line pass alone builds for `buffer` against `base`.
    fn plain_diff(buffer: &str, base: &DiffBaseText) -> DiffMap {
        compute_diff_map(
            buffer,
            None,
            &SyntaxStyles::from_theme(&Theme::empty()),
            &BaseHighlightCache::default(),
            base,
            None,
            None,
        )
        .expect("a diff map")
    }

    /// The base holds one change over the text behind it and the buffer one
    /// change over the base. The first is a staged mark and the second the
    /// one unstaged hunk.
    #[test]
    fn a_staged_from_text_becomes_staged_marks() {
        let dm = plain_diff(
            "a\nB\nC\n",
            &diff_base("a\nB\nc\n", "a\nB\nc\n", Some("a\nb\nc\n")),
        );

        assert_eq!(
            dm.hunks()
                .map(|hunk| (hunk.buffer_start_line, hunk.staged()))
                .collect::<Vec<_>>(),
            [(2, false)],
            "the buffer's own change is the one hunk",
        );
        assert_eq!(
            *dm.staged_marks,
            [StagedMark {
                status: DiffHunkStatus::Modified,
                base_lines: 1..2,
            }],
            "the base's change over the text behind it",
        );
        assert_eq!(dm.staged_counts(), (1, 1), "the mark and the hunk");
    }

    /// A file removed in the index leaves an empty base with the file's text
    /// behind it. The removal is already staged, so it is one staged mark and
    /// no hunk.
    #[test]
    fn a_removal_already_in_the_index_is_one_staged_mark() {
        let dm = plain_diff("\n", &diff_base("", "", Some("x\n")));

        assert_eq!(dm.hunks().count(), 0, "no hunk");
        assert_eq!(
            *dm.staged_marks,
            [StagedMark {
                status: DiffHunkStatus::Deleted,
                base_lines: 0..0,
            }],
            "the removal is one staged mark",
        );
        assert_eq!(dm.staged_counts(), (1, 0), "counted as staged");
    }

    /// Bucketing the base spans is O(spans) with a style clone per line each
    /// span touches, and it ran per recompute over a base text and a style
    /// table that a keystroke burst leaves alone. Only a rebuilt style table
    /// can change the answer, so only that may cost the walk again.
    #[test]
    fn base_highlights_reuse_the_bucketed_map_until_the_styles_are_rebuilt() {
        let cache = Arc::new(Mutex::new(BaseHighlightMemo::default()));
        let language = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .expect("rust language");
        let base = "fn main() {\n    let x = 1;\n}\n";
        let styles = SyntaxStyles::from_theme(&Theme::empty());

        let first = compute_base_highlights(base, &language, &styles, &cache, None);
        let second = compute_base_highlights(base, &language, &styles, &cache, None);
        assert!(
            Arc::ptr_eq(&first, &second),
            "the same style table serves the bucketed map it already built",
        );

        let rebuilt = SyntaxStyles::from_theme(&Theme::empty());
        let third = compute_base_highlights(base, &language, &rebuilt, &cache, None);
        assert!(
            !Arc::ptr_eq(&first, &third),
            "a rebuilt style table cannot reuse a resolve made against the old one",
        );
        assert_eq!(
            *first, *third,
            "and the miss is conservative: the same theme resolves the same way",
        );
    }

    /// Two captures over one token resolve to the later, more specific one, as
    /// the right column resolves them. Otherwise every identifier on the left
    /// takes the query's leading `@variable` color, and a doc comment takes the
    /// plain comment color.
    #[test]
    fn base_highlights_let_the_later_capture_win() {
        let lines = themed_base_highlights(
            "a.rs",
            r##"theme t {
                syntax.variable.fg = "#010101";
                syntax.function.fg = "#020202";
                syntax.comment.fg = "#030303";
                syntax.comment.doc.fg = "#040404";
            }"##,
            "/// doc\nfn main() {\n    foo();\n}\n",
        );
        let fg_at = |line: usize, byte: usize| {
            lines[line]
                .iter()
                .find(|(range, _)| range.contains(&byte))
                .and_then(|(_, style)| style.foreground)
        };

        assert_eq!(
            (fg_at(2, 4), fg_at(2, 6), fg_at(0, 0)),
            (
                Some(Color::Rgb(2, 2, 2)),
                Some(Color::Rgb(2, 2, 2)),
                Some(Color::Rgb(4, 4, 4)),
            ),
            "the call paints as a function and the doc comment as a doc comment"
        );
        assert!(
            lines.iter().all(|spans| spans
                .windows(2)
                .all(|pair| pair[0].0.end <= pair[1].0.start)),
            "every line's runs are disjoint and start-sorted, got {lines:?}"
        );
    }

    /// A fenced block in removed markdown takes its own language's colors on
    /// the left, as the right column paints the same block through its
    /// injection layer.
    #[test]
    fn base_highlights_color_an_injected_fenced_block() {
        let lines = themed_base_highlights(
            "a.md",
            r##"theme t {
                syntax.keyword.fg = "#050505";
            }"##,
            "```rust\nfn main() {}\n```\n",
        );

        assert_eq!(
            lines[1]
                .iter()
                .find(|(range, _)| range.contains(&0))
                .map(|(range, style)| (range.clone(), style.foreground)),
            Some((0..2, Some(Color::Rgb(5, 5, 5)))),
            "the fenced `fn` paints as a rust keyword, got {:?}",
            lines[1]
        );
    }

    /// The base highlights of `text` in the language `path` selects, under a
    /// theme parsed from `theme_src`, with every standard language's highlight
    /// map pointed at that theme.
    fn themed_base_highlights(path: &str, theme_src: &str, text: &str) -> Arc<BaseHighlights> {
        let (styles, language) = themed_language(path, theme_src);
        let cache: BaseHighlightCache = Arc::new(Mutex::new(BaseHighlightMemo::default()));
        compute_base_highlights(text, &language, &styles, &cache, None)
    }

    /// The syntax styles of a theme parsed from `theme_src`, and the language
    /// `path` selects, with every standard language's highlight map pointed at
    /// those styles.
    fn themed_language(path: &str, theme_src: &str) -> (SyntaxStyles, Arc<Language>) {
        let (config, errors) = stoat_config::parse(theme_src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let theme = Theme::from_config(&config.expect("a theme config"), "t").expect("a theme");
        let styles = SyntaxStyles::from_theme(&theme);
        let registry = LanguageRegistry::standard();
        app::install_highlight_maps(&registry, &styles);
        let language = registry
            .for_path(Path::new(path))
            .expect("a bundled language");
        (styles, language)
    }

    fn input(base: &str, buffer: &str) -> ReviewFileInput {
        ReviewFileInput {
            path: PathBuf::from("/repo/a.rs"),
            rel_path: "a.rs".to_string(),
            language: None,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }
    }

    #[test]
    fn changed_byte_ranges_covers_an_added_line() {
        let ranges = changed_byte_ranges(&input("fn foo() {}\n", "fn foo() {}\nfn bar() {}\n"));
        assert!(
            ranges.iter().any(|r| r.contains(&15)),
            "the added second line's bytes are reported changed, got {ranges:?}",
        );
    }

    #[test]
    fn changed_byte_ranges_empty_when_identical() {
        assert!(changed_byte_ranges(&input("fn foo() {}\n", "fn foo() {}\n")).is_empty());
    }

    /// Every hunk becomes one working-tree byte range, deletions included.
    ///
    /// A deletion has no working-tree lines of its own, so it lands as an empty
    /// range at the seam it was cut from, wherever that seam falls. The range
    /// is kept rather than dropped. The caller's overlap test reads an empty
    /// range as a point, so a symbol spanning the seam still reports changed,
    /// which is the whole signal a deletion has to give.
    ///
    /// The last case has no closing newline, so its range ends at the text's
    /// end rather than at a line start the table does not hold.
    ///
    /// Compared as pairs because an expected `[2..2]` reads to the compiler as
    /// a range that might have meant a repeat count.
    #[test]
    fn changed_byte_ranges_converts_every_hunk_including_deletion_seams() {
        let cases = [
            ("a\nb\nc\n", "a\nc\n", vec![(2, 2)]),
            ("a\nb\nc\n", "b\nc\n", vec![(0, 0)]),
            ("a\nb\nc\nd\n", "a\nc\nd\ne\n", vec![(2, 2), (6, 8)]),
            ("a\nb", "a\nc", vec![(2, 3)]),
        ];

        for (base, buffer, expected) in cases {
            let got: Vec<(usize, usize)> = changed_byte_ranges(&input(base, buffer))
                .into_iter()
                .map(|r| (r.start, r.end))
                .collect();
            assert_eq!(got, expected, "{base:?} -> {buffer:?}");
        }
    }

    #[test]
    fn changed_ranges_scan_memoizes_across_unchanged_scans() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n")],
        );

        let git = h.stoat.git_host.clone();
        let fs = h.stoat.fs_host.clone();
        let langs = h.stoat.language_registry.clone();
        let scan_and_install = |ws: &mut Workspace| {
            let scan = scan_changed_ranges(
                git.as_ref(),
                fs.as_ref(),
                &langs,
                &ws.git_root.clone(),
                &ws.changed_ranges_memo_snapshot(),
            );
            ws.install_changed_ranges(scan);
        };

        let ws = h.stoat.active_workspace_mut();

        scan_and_install(ws);
        assert_eq!(
            ws.changed_ranges_recomputes, 1,
            "the first scan diffs the changed file once"
        );
        assert!(
            !ws.changed_ranges.is_empty(),
            "the working-tree change is recorded"
        );

        scan_and_install(ws);
        assert_eq!(
            ws.changed_ranges_recomputes, 1,
            "a second scan over the unchanged tree reuses the memo, no re-diff"
        );
        assert!(
            !ws.changed_ranges.is_empty(),
            "the recorded change survives the memo hit"
        );
    }

    #[test]
    fn diff_job_populates_tracked_buffer_diff_map() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard
            .diff_map
            .as_ref()
            .expect("the tracked buffer's diff map is populated");

        assert_eq!(
            dm.status_for_line(1),
            DiffStatus::Modified,
            "the edited second line reads modified"
        );
        assert_eq!(
            dm.status_for_line(0),
            DiffStatus::Unchanged,
            "the unchanged first line reads unchanged"
        );
    }

    /// A moved file carries its content across, so it owes no hunks. Its base
    /// lives under the path it came from. A read of the new path alone shows
    /// the whole file as freshly written.
    #[test]
    fn a_moved_buffer_diffs_against_the_old_path_and_shows_no_hunks() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_rename_scenario("/repo", "old.txt", "new.txt", "a\nb\n", "a\nb\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/new.txt"));
        h.settle_diff_jobs();

        assert_eq!(
            focused_diff_statuses(&h, 2),
            vec![DiffStatus::Unchanged, DiffStatus::Unchanged],
            "a move edits no line, so the moved buffer owes no hunks",
        );
    }

    /// `git mv` also puts the new path in the index. The move still decides
    /// the base, so the index's copy under the new path adds nothing staged.
    #[test]
    fn a_staged_rename_diffs_against_the_old_path() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_rename_scenario("/repo", "old.txt", "new.txt", "a\nb\n", "a\nb\n");
        h.fake_git()
            .add_repo("/repo")
            .index_file("new.txt", "a\nb\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/new.txt"));
        h.settle_diff_jobs();

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        assert_eq!(
            (hunk_flags(dm), dm.staged_counts()),
            (Vec::new(), (0, 0)),
            "a move edits no line, staged or not",
        );
    }

    #[test]
    fn a_moved_and_edited_buffer_shows_only_the_edit() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_rename_scenario("/repo", "old.txt", "new.txt", "a\nb\n", "a\nc\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/new.txt"));
        h.settle_diff_jobs();

        assert_eq!(
            focused_diff_statuses(&h, 2),
            vec![DiffStatus::Unchanged, DiffStatus::Modified],
            "only the edited line is work the move did not carry across",
        );
    }

    /// Per-line diff status of the focused editor's buffer, for the first
    /// `lines` lines. Panics when the buffer carries no diff map, which is the
    /// failure a test asserting statuses wants named.
    fn focused_diff_statuses(h: &TestHarness, lines: u32) -> Vec<DiffStatus> {
        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer = ws
            .buffers
            .get(ws.editors[editor_id].buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("buffer has a diff map");
        (0..lines).map(|line| dm.status_for_line(line)).collect()
    }

    /// Neither blob can change without a write under `.git`, which invalidates
    /// the cache, so a recompute driven by an edit has no reason to read them
    /// again. Each read is a repo-mutex acquisition and a decompression.
    #[test]
    fn a_second_diff_of_one_file_reads_no_blobs() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        let repo = PathBuf::from("/repo");
        let first = h.fake_git().blob_reads(&repo);
        assert!(first > 0, "the first diff has to read the blobs");

        // Move the buffer so the next drive finds the diff stale and recomputes.
        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        ws.buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "x");
        h.settle_diff_jobs();

        assert_eq!(
            h.fake_git().blob_reads(&repo),
            first,
            "the second diff reuses the blobs the first one read",
        );
    }

    /// A diff walks the whole file on a blocking thread and the next keystroke
    /// invalidates it, so a burst is worth one diff at the end rather than one
    /// per edit.
    #[test]
    fn a_burst_of_edits_diffs_once_after_it_settles() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        let repo = PathBuf::from("/repo");
        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;

        // Typing, with a frame between keystrokes and none of them settling.
        for _ in 0..5 {
            h.stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer")
                .write()
                .expect("poisoned")
                .edit(0..0, "x");
            h.stoat.drive_background();
            h.settle();
            assert!(
                h.stoat.active_workspace().diff.jobs.is_empty(),
                "a keystroke inside the settle window spawns no diff",
            );
        }

        let before = h.fake_git().blob_reads(&repo);
        h.advance_clock(DIFF_SETTLE + std::time::Duration::from_millis(1));
        h.stoat.drive_background();
        assert_eq!(
            h.stoat.active_workspace().diff.jobs.len(),
            1,
            "and the settle spawns one job for the whole burst",
        );

        h.settle();
        h.stoat.drive_background();
        assert_eq!(
            h.fake_git().blob_reads(&repo),
            before,
            "which reuses the cached blobs rather than rereading them",
        );
    }

    /// A repo where `a.rs` gains a line over HEAD, open with the background
    /// diff on, and the buffer's id. The open leaves the map to the caller.
    fn wanted_harness() -> (TestHarness, BufferId) {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.stoat.open_file(Path::new("/repo/a.rs"));
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        (h, buffer_id)
    }

    /// The buffer's diff as `(hunk count, base colors in, hunks-only map in,
    /// whole map current)`.
    fn wanted_map(h: &TestHarness, buffer_id: BufferId) -> (usize, bool, bool, bool) {
        let ws = h.stoat.active_workspace();
        let (hunks, colors) = {
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            let dm = guard.diff_map.as_ref().expect("diff map installed");
            (
                dm.hunks_in_range(0..u32::MAX).len(),
                dm.base_highlights_for_line(0).is_some(),
            )
        };
        (
            hunks,
            colors,
            ws.hunks_installed(buffer_id),
            ws.diff_map_current(buffer_id),
        )
    }

    /// A buffer that was opened and not typed in has no burst to wait out, so
    /// the drive starts its colors job the frame its hunks land.
    #[test]
    fn a_wanted_map_lands_hunks_without_colors_and_the_drive_adds_them_with_no_settle() {
        let (mut h, buffer_id) = wanted_harness();

        h.settle_diff_map(buffer_id);
        assert_eq!(
            wanted_map(&h, buffer_id),
            (1, false, true, false),
            "the want lands the hunk with its colors owed",
        );

        h.stoat.drive_background();
        h.settle();
        h.stoat.drive_background();
        assert_eq!(
            wanted_map(&h, buffer_id),
            (1, true, false, true),
            "the drive lands the colors with no clock advance",
        );
    }

    /// The blobs a want read predate a `.git` write that lands before it, so
    /// its map paints and the next diff reads the blobs again.
    #[test]
    fn a_git_write_during_a_want_files_no_base() {
        let (mut h, buffer_id) = wanted_harness();
        let repo = PathBuf::from("/repo");
        let before = h.fake_git().blob_reads(&repo);

        action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, None);
        let want_reads = h.fake_git().blob_reads(&repo) - before;
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.settle();
        assert_eq!(
            wanted_map(&h, buffer_id),
            (1, false, false, false),
            "the map installs and records neither a marker nor a version",
        );

        let reads = h.fake_git().blob_reads(&repo);
        h.settle_diff_jobs();
        assert_eq!(
            (h.fake_git().blob_reads(&repo) - reads, want_reads > 0),
            (want_reads, true),
            "the next diff reads the blobs the want read",
        );
    }

    /// A drive job in flight when a diff open wants the same version lands
    /// first, and the want's hunks-only map then has nothing to add.
    #[test]
    fn a_want_that_lands_after_a_drive_job_keeps_the_jobs_colors() {
        let (mut h, buffer_id) = wanted_harness();
        h.stoat.drive_background();
        h.advance_clock(DIFF_SETTLE + std::time::Duration::from_millis(1));
        h.stoat.drive_background();
        assert_eq!(
            h.stoat.active_workspace().diff.jobs.len(),
            1,
            "the drive has a job in flight",
        );

        action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, None);
        h.run_until_parked();
        h.stoat.drive_background();
        assert_eq!(
            wanted_map(&h, buffer_id),
            (1, true, false, true),
            "the job's whole map stays on the buffer",
        );
    }

    #[test]
    fn a_second_want_at_the_same_version_adds_its_landing_and_no_job() {
        let (mut h, buffer_id) = wanted_harness();
        let hops = h.blocking_calls();

        for last in [false, true] {
            action_handlers::review::ensure_diff_map(
                &mut h.stoat,
                buffer_id,
                Some(DiffLanding::EdgeChange { last }),
            );
        }

        let lasts: Vec<bool> = h.stoat.active_workspace().diff.wants[&buffer_id]
            .landings
            .iter()
            .map(|landing| matches!(landing, DiffLanding::EdgeChange { last: true }))
            .collect();
        assert_eq!(
            (h.blocking_calls() - hops, lasts),
            (1, vec![false, true]),
            "one job carries both landings in the order they came",
        );
    }

    fn file_hop() -> DiffLanding {
        DiffLanding::FileHop {
            dir: movement::ChangeDir::Next,
            origin: None,
            wrapped: false,
            fallback: (0, 0..1),
        }
    }

    /// A hop lands on the runs the tree pass makes, which installed hunks alone
    /// do not hold, so it wants the whole map and lands it current.
    #[test]
    fn a_hop_over_installed_hunks_wants_the_whole_map() {
        let (mut h, buffer_id) = wanted_harness();
        h.settle_diff_map(buffer_id);

        action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, Some(file_hop()));
        let whole = h
            .stoat
            .active_workspace()
            .diff
            .wants
            .get(&buffer_id)
            .map(|want| want.whole);
        assert_eq!(whole, Some(true), "the hop waits for the whole map");

        h.settle();
        assert_eq!(
            wanted_map(&h, buffer_id),
            (1, true, false, true),
            "the whole map lands with its colors and counts as current",
        );
    }

    #[test]
    fn a_whole_want_takes_over_a_hunks_want_in_flight() {
        let (mut h, buffer_id) = wanted_harness();
        let hops = h.blocking_calls();

        action_handlers::review::ensure_diff_map(
            &mut h.stoat,
            buffer_id,
            Some(DiffLanding::EdgeChange { last: false }),
        );
        action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, Some(file_hop()));

        let want = &h.stoat.active_workspace().diff.wants[&buffer_id];
        assert_eq!(
            (h.blocking_calls() - hops, want.whole, want.landings.len()),
            (2, true, 2),
            "the whole job replaces the hunks one and carries both landings",
        );
    }

    #[test]
    fn a_pair_base_follows_an_edit_of_its_file() {
        let mut h = pair_harness();
        assert_eq!(
            focused_diff_statuses(&h, 2),
            [DiffStatus::Unchanged, DiffStatus::Modified]
        );

        rewrite_base(&h, "one\nTWO\n");
        h.settle_diff_jobs();
        assert_eq!(
            (pair_text(&h, "/pair/b.txt"), focused_diff_statuses(&h, 2)),
            (
                Some("one\nTWO\n".to_string()),
                vec![DiffStatus::Unchanged, DiffStatus::Unchanged]
            ),
            "the base took the edit and the hunk is gone",
        );
    }

    /// A file whose git diff is current when the pair opens re-diffs against
    /// the pair, rather than keep the map the git diff left.
    #[test]
    fn a_pair_replaces_the_git_diff_its_file_had() {
        let mut h = TestHarness::with_size(120, 20);
        h.stage_review_scenario("/repo", &[("b.txt", "zero\n", "one\nTWO\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_side_by_side(("/repo/a.txt", "one\ntwo\n"), ("/repo/b.txt", "one\nTWO\n"));
        h.settle_diff_jobs();
        let git = focused_diff_statuses(&h, 2);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
        h.settle_diff_jobs();
        assert_eq!(
            (git, focused_diff_statuses(&h, 2)),
            (
                vec![DiffStatus::Modified, DiffStatus::Modified],
                vec![DiffStatus::Unchanged, DiffStatus::Modified],
            ),
        );
    }

    /// Closing a diff view with no base installed stales no other file, so a
    /// pair in another tab keeps its current map.
    #[test]
    fn closing_a_git_diff_re_diffs_no_other_file() {
        let mut h = pair_harness();
        let (id, version) = {
            let ws = h.stoat.active_workspace();
            let id = ws
                .buffers
                .id_for_path(Path::new("/pair/b.txt"))
                .expect("the measured file is open");
            let buffer = ws.buffers.get(id).expect("buffer");
            let version = buffer.read().expect("poisoned").snapshot.version;
            (id, version)
        };

        action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.open_file(Path::new("/pair/a.txt"));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        assert!(
            h.stoat.active_workspace().diff.current(id, version),
            "the pair's map stays current"
        );
    }

    #[test]
    fn each_pair_follows_its_own_base() {
        let mut h = pair_harness();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.open_side_by_side(("/pair/c.txt", "x\ny\n"), ("/pair/d.txt", "x\nY\n"));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
        h.settle_diff_jobs();

        rewrite_base(&h, "one\nTWO\n");
        h.settle_diff_jobs();
        assert_eq!(
            (pair_text(&h, "/pair/b.txt"), pair_text(&h, "/pair/d.txt")),
            (Some("one\nTWO\n".to_string()), Some("x\ny\n".to_string())),
            "the edit of a.txt moved b.txt's base alone",
        );
    }

    #[test]
    fn a_pair_base_waits_out_the_settle_window() {
        let mut h = pair_harness();
        rewrite_base(&h, "one\nTWO\n");
        h.stoat.drive_background();
        assert_eq!(
            pair_text(&h, "/pair/b.txt"),
            Some("one\ntwo\n".to_string()),
            "a moved base waits for the window like any edit",
        );
    }

    /// `/pair/b.txt` diffed against `/pair/a.txt`, side by side and settled.
    fn pair_harness() -> TestHarness {
        let mut h = TestHarness::with_size(120, 20);
        h.stoat.set_diff_warm_auto(true);
        h.open_side_by_side(("/pair/a.txt", "one\ntwo\n"), ("/pair/b.txt", "one\nTWO\n"));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
        h.settle_diff_jobs();
        h
    }

    /// The text of the pair `path` measures against.
    fn pair_text(h: &TestHarness, path: &str) -> Option<String> {
        let pair = h.stoat.active_workspace().pair_for(Path::new(path))?;
        Some(pair.text.to_string())
    }

    /// Replace the whole text of the `/pair/a.txt` buffer, as an edit in its
    /// pane does.
    fn rewrite_base(h: &TestHarness, text: &str) {
        let ws = h.stoat.active_workspace();
        let id = ws
            .buffers
            .id_for_path(Path::new("/pair/a.txt"))
            .expect("the base file is open");
        let buffer = ws.buffers.get(id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let len = guard.snapshot.visible_text.len();
        guard.edit(0..len, text);
    }

    /// Every decoration consumer keys off the diff map's version, and the
    /// minimap re-derives the whole file from it, so a version that moves for a
    /// map nobody can tell apart is work spent on nothing. A write under `.git`
    /// re-diffs every visible buffer whether or not its content moved.
    #[test]
    fn rediffing_untouched_content_keeps_the_installed_map() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let map_version = |h: &TestHarness| {
            let ws = h.stoat.active_workspace();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            guard.diff_map.as_ref().expect("diffed").version()
        };
        let first = map_version(&h);

        // A git write re-diffs every visible buffer, content or no.
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.settle_diff_jobs();
        assert_eq!(
            map_version(&h),
            first,
            "a rediff of untouched content keeps the map it already had",
        );

        // A real change to the hunks has to land.
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "new\n");
        h.settle_diff_jobs();
        assert_ne!(
            map_version(&h),
            first,
            "and an added line is a different diff",
        );
    }

    #[test]
    fn drive_diff_jobs_skips_an_already_current_buffer() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        // The buffer's diff is now current. Another event-loop turn must not
        // respawn a job for the unchanged version.
        h.stoat.drive_background();
        assert!(
            h.stoat.active_workspace().diff.jobs.is_empty(),
            "a drive over an already-diffed buffer spawns no new job",
        );
    }

    #[test]
    fn invalidate_all_diffs_redrives_a_visible_buffer_against_the_new_head() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "the settled job leaves the buffer's diff current",
        );
        assert_eq!(
            base_text_of(&h, buffer_id),
            "a\nb\n",
            "the first diff is computed against the original HEAD",
        );

        // An external rebase moves HEAD under the editor. The buffer is
        // untouched, so only the invalidation can force a re-diff.
        h.fake_git().add_repo("/repo").head_file("a.txt", "a\nZ\n");
        h.stoat.active_workspace_mut().invalidate_all_diffs();

        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "invalidation drops the recorded version",
        );
        h.settle_diff_jobs();
        assert_eq!(
            base_text_of(&h, buffer_id),
            "a\nZ\n",
            "the redrive diffs the buffer against the moved HEAD",
        );
    }

    /// A job still running against the old base never installs once the base
    /// changes, so the map the view opened on stays.
    #[test]
    fn a_base_change_drops_the_diff_running_against_the_old_base() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_index_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n", "a\nd\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;

        h.stoat
            .active_workspace_mut()
            .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
        h.stoat.drive_background();
        action_handlers::dispatch(&mut h.stoat, &DiffAgainstIndex);
        h.settle();
        h.stoat.drive_background();

        assert_eq!(
            base_text_of(&h, buffer_id),
            "a\nc\n",
            "the index base stands, and the HEAD job never lands",
        );
    }

    /// A stage stales the diff of a buffer whose job still runs. The job keeps
    /// its slot and loses only its standing, so no second job starts beside it.
    #[test]
    fn invalidate_marks_the_job_in_flight_superseded() {
        let h = TestHarness::with_size(80, 24);
        let id = BufferId::new(1);
        let mut state = DiffState::default();
        state.jobs.insert(id, pending_job(&h));

        state.invalidate(id, None);

        assert_eq!(
            state.jobs.get(&id).map(|job| job.superseded),
            Some(true),
            "the job stays in its slot, marked superseded",
        );
    }

    /// A buffer edited while its diff job runs waits for that job rather than
    /// starting a second one beside it. Its settle window runs meanwhile, so
    /// the next diff lands with no further wait once the job in flight is gone.
    #[test]
    fn a_newer_version_waits_for_the_job_in_flight() {
        let (mut h, buffer_id) = open_unstaged_change();
        let job = pending_job(&h);
        h.stoat
            .active_workspace_mut()
            .diff
            .jobs
            .insert(buffer_id, job);

        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "x");
        h.settle_diff_jobs();
        let current_while_busy = h.stoat.active_workspace().diff_map_current(buffer_id);

        // The clock does not advance from here, so the diff lands only on a
        // settle window that ran while the job was in flight.
        h.stoat.active_workspace_mut().diff.jobs.remove(&buffer_id);
        h.stoat.drive_background();
        h.settle();
        h.stoat.drive_background();

        assert_eq!(
            (
                current_while_busy,
                h.stoat.active_workspace().diff_map_current(buffer_id)
            ),
            (false, true),
            "the settled edit waits for the job in flight, then diffs at once",
        );
    }

    /// A diff job that never lands, so a drive finds it in flight.
    fn pending_job(h: &TestHarness) -> DiffJob {
        DiffJob {
            task: h.stoat.executor.spawn(future::pending()),
            superseded: false,
            base_moved: false,
            started: h.stoat.executor.now(),
        }
    }

    /// A stage moves the index blob and leaves the buffer where it was, so the
    /// mark has to flip on the next drive, with no clock advance and no `.git`
    /// event behind it.
    #[test]
    fn a_git_write_restages_an_unedited_buffer_on_the_next_drive() {
        let (mut h, buffer_id) = open_unstaged_change();
        assert_eq!(
            staged_on_row_one(&h, buffer_id),
            Some(false),
            "the change starts unstaged"
        );

        // The fake's patch apply records the patch and writes no index blob, so
        // the test writes the blob a real stage leaves.
        h.fake_git().add_repo("/repo").index_file("a.txt", "a\nc\n");
        h.stoat
            .active_workspace_mut()
            .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
        h.stoat.drive_background();
        h.settle();
        h.stoat.drive_background();

        assert_eq!(
            staged_on_row_one(&h, buffer_id),
            Some(true),
            "the next drive reads the staged blob"
        );
    }

    /// The test scheduler runs the press's git work inline, so the landing
    /// waits in the queue. The one drive below is the frame that lands it, and
    /// that frame owes the diff the landing staled.
    #[test]
    fn a_landed_stage_starts_its_diff_in_the_same_frame() {
        let (mut h, buffer_id) = open_unstaged_change();
        assert_eq!(
            staged_on_row_one(&h, buffer_id),
            Some(false),
            "the change starts unstaged"
        );

        movement::set_cursor_row(
            action_handlers::focused_editor_mut(&mut h.stoat).expect("editor"),
            1,
        );
        action_handlers::dispatch(&mut h.stoat, &ToggleStageHunk);
        // The fake's patch apply records the patch and writes no index blob, so
        // the test writes the blob a real stage leaves.
        h.fake_git().add_repo("/repo").index_file("a.txt", "a\nc\n");

        h.stoat.drive_background();
        assert_eq!(
            h.stoat.active_workspace().diff.jobs.len(),
            1,
            "the frame that lands the stage starts its diff"
        );

        h.settle();
        h.stoat.drive_background();
        assert_eq!(
            staged_on_row_one(&h, buffer_id),
            Some(true),
            "the diff the landing started reads the staged blob"
        );
    }

    /// Only a job whose base moved under unchanged text skips the settle window
    /// and logs its duration. A keystroke's job waits the window out.
    #[test]
    fn only_a_diff_job_after_a_write_reads_its_base_as_moved() {
        let (mut h, buffer_id) = open_unstaged_change();
        let base_moved = |h: &TestHarness| -> Vec<bool> {
            h.stoat
                .active_workspace()
                .diff
                .jobs
                .values()
                .map(|job| job.base_moved)
                .collect()
        };

        movement::set_cursor_row(
            action_handlers::focused_editor_mut(&mut h.stoat).expect("editor"),
            1,
        );
        action_handlers::dispatch(&mut h.stoat, &ToggleStageHunk);
        h.fake_git().add_repo("/repo").index_file("a.txt", "a\nc\n");
        h.stoat.drive_background();
        let after_write = base_moved(&h);

        h.settle();
        h.stoat.drive_background();
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "x");
        h.stoat.drive_background();
        h.advance_clock(DIFF_SETTLE + std::time::Duration::from_millis(1));
        h.stoat.drive_background();

        assert_eq!(
            (after_write, base_moved(&h)),
            (vec![true], vec![false]),
            "the write's job skips the settle window, and the keystroke's waits it out",
        );
    }

    /// The watcher's echo of a stage arrives while the job the stage started is
    /// in flight. That job already read the staged blob.
    #[test]
    fn a_git_write_lets_the_job_in_flight_paint() {
        let (mut h, buffer_id) = open_unstaged_change();
        assert_eq!(
            staged_on_row_one(&h, buffer_id),
            Some(false),
            "the change starts unstaged"
        );

        h.fake_git().add_repo("/repo").index_file("a.txt", "a\nc\n");
        h.stoat
            .active_workspace_mut()
            .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
        h.stoat.drive_background();
        h.stoat.active_workspace_mut().invalidate_all_diffs();

        h.settle();
        h.stoat.drive_background();
        assert_eq!(
            staged_on_row_one(&h, buffer_id),
            Some(true),
            "the job in flight at the .git write still paints"
        );
        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "a re-run against the fresh blobs is owed"
        );

        h.settle();
        h.stoat.drive_background();
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "the re-run lands and the map is current"
        );
    }

    /// Open `a.txt` with one unstaged change on row 1, and settle its first
    /// diff.
    fn open_unstaged_change() -> (TestHarness, BufferId) {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        (h, buffer_id)
    }

    /// Whether the installed map marks row 1's change as staged.
    fn staged_on_row_one(h: &TestHarness, buffer_id: BufferId) -> Option<bool> {
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard
            .diff_map
            .as_ref()
            .expect("diff map")
            .staged_for_line(1)
    }

    /// Every hunk's buffer spans, keyed by the first buffer row it covers.
    fn detail_spans_of(h: &TestHarness, buffer_id: BufferId) -> Vec<(u32, usize)> {
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        dm.hunks()
            .map(|hunk| {
                let spans = hunk
                    .token_detail
                    .as_ref()
                    .map_or(0, |detail| detail.buffer_spans.len());
                (hunk.buffer_start_line, spans)
            })
            .collect()
    }

    /// Open `contents` as a rust file diffed against `base`, and settle the
    /// background job that carries the structural refinement.
    fn settled_rust_diff(base: &str, buffer: &str) -> (TestHarness, BufferId) {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.rs", base, buffer)]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        (h, buffer_id)
    }

    /// Open `buffer` as a file with no language diffed against `base`, and
    /// settle the line pass that diffs it.
    fn settled_plain_diff(base: &str, buffer: &str) -> (TestHarness, BufferId) {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.unknownext", base, buffer)]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.unknownext"));
        h.settle_diff_jobs();
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        (h, buffer_id)
    }

    /// A reindent under a new block is what the structural pass exists for. The
    /// line differ sees every moved line's prefix rewritten and washes it; the
    /// tree differ sees unchanged statements and marks only the braces.
    #[test]
    fn a_reindent_under_a_new_block_washes_only_the_braces() {
        const BASE: &str = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";
        const BUFFER: &str =
            "fn f() {\n    if x {\n        let a = 1;\n        let b = 2;\n        let c = 3;\n    }\n}\n";

        let (h, buffer_id) = settled_rust_diff(BASE, BUFFER);
        let spans = detail_spans_of(&h, buffer_id);
        // The block adds four tokens, `if`, `x`, and its two braces. The line
        // differ washes nine spans here, one per rewritten indent.
        assert_eq!(
            spans,
            [(1, 4)],
            "the reindented statements wash nothing, and only `if x` and its braces mark"
        );
    }

    /// The statuses and extents a settled rust diff of `base` against `buffer`
    /// produced, in hunk order.
    fn hunk_shapes(base: &str, buffer: &str) -> Vec<(DiffHunkStatus, Range<u32>)> {
        let (h, buffer_id) = settled_rust_diff(base, buffer);
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        dm.hunks()
            .map(|hunk| (hunk.status, hunk.buffer_line_range.clone()))
            .collect()
    }

    /// Three functions with blank lines between them, one of which relocates.
    const THREE_FNS: &str =
        "fn a() {\n    let x = 1;\n}\n\nfn b() {\n    let y = 2;\n}\n\nfn c() {\n    let z = 3;\n}\n";

    /// The tree differ marks tokens, so it can never reach a line holding none.
    /// A relocated block carrying a blank line has to read as a move anyway, or
    /// the separator between two functions decides that neither of them moved.
    #[test]
    fn a_move_carrying_a_blank_line_is_still_a_move() {
        const MOVED: &str =
            "fn c() {\n    let z = 3;\n}\n\nfn a() {\n    let x = 1;\n}\n\nfn b() {\n    let y = 2;\n}\n";

        assert_eq!(
            MOVED.lines().nth(3),
            Some(""),
            "the fixture needs the blank inside the relocated hunk",
        );
        assert_eq!(
            hunk_shapes(THREE_FNS, MOVED),
            [
                (DiffHunkStatus::Moved, 0..4),
                (DiffHunkStatus::Moved, 11..11),
            ],
            "the four-line block reads as a move, blank line and all, and so does \
             the run it left at the end",
        );
    }

    /// Half a relocated block is still an addition. Staging reads the extents,
    /// so calling a hunk moved when only part of it came from somewhere else
    /// would put a move chip on lines that were written here.
    #[test]
    fn a_hunk_only_partly_relocated_stays_an_addition() {
        const PARTIAL: &str = "fn b() {\n    let y = 2;\n}\n\nfn c() {\n    let z = 3;\n}\n\nfn \
                               a() {\n    let x = 1;\n}\n\nfn d() {\n    let w = 4;\n}\n";

        assert_eq!(
            hunk_shapes(THREE_FNS, PARTIAL),
            [
                (DiffHunkStatus::Moved, 0..0),
                (DiffHunkStatus::Added, 7..15),
            ],
            "the added run holds one relocated function and one written here, so it is an addition",
        );
    }

    /// A function relocated within a file is the one thing only the tree pass
    /// sees. The line differ reads the old site as a deletion and the new one
    /// as an addition, and nothing about either says they are halves of the
    /// same thing -- which is what left the origin chips and the m/M jumps with
    /// no data outside their own tests.
    #[test]
    fn a_function_moved_within_a_file_reads_as_a_move() {
        const BASE: &str = "fn a() {\n    let x = 1;\n}\n\nfn b() {\n    let y = 2;\n}\n";
        const BUFFER: &str = "fn b() {\n    let y = 2;\n}\n\nfn a() {\n    let x = 1;\n}\n";

        let (h, buffer_id) = settled_rust_diff(BASE, BUFFER);
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");

        let moved: Vec<_> = dm
            .hunks()
            .filter(|hunk| hunk.status == DiffHunkStatus::Moved)
            .collect();
        assert!(
            !moved.is_empty(),
            "a relocated function reads as a move: {:?}",
            dm.hunks()
                .map(|h| (h.status, h.buffer_start_line))
                .collect::<Vec<_>>(),
        );
        assert!(
            moved.iter().any(
                |hunk| hunk.token_detail.as_ref().is_some_and(|detail| detail
                    .buffer_spans
                    .iter()
                    .chain(&detail.base_spans)
                    .any(|span| span.move_metadata.is_some()))
            ),
            "and carries the metadata naming where it came from",
        );
    }

    /// A move is a relabelling, not a re-extent. The staging siblings under this
    /// parent read the line ranges, so an `s` on a hunk that just became a move
    /// has to stage exactly what it staged as an addition.
    #[test]
    fn flipping_a_hunk_to_moved_leaves_its_extents_alone() {
        const BASE: &str = "fn a() {\n    let x = 1;\n}\n\nfn b() {\n    let y = 2;\n}\n";
        const BUFFER: &str = "fn b() {\n    let y = 2;\n}\n\nfn a() {\n    let x = 1;\n}\n";

        let extents = |language: bool| {
            let mut h = TestHarness::with_size(80, 24);
            h.stage_review_scenario(
                "/repo",
                &[(
                    match language {
                        true => "a.rs",
                        // No grammar, so the tree pass never runs and the line
                        // pass's reading stands untouched.
                        false => "a.txt",
                    },
                    BASE,
                    BUFFER,
                )],
            );
            h.stoat.set_diff_warm_auto(true);
            h.open_file(Path::new(match language {
                true => "/repo/a.rs",
                false => "/repo/a.txt",
            }));
            h.settle_diff_jobs();
            let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
            let ws = h.stoat.active_workspace();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            let dm = guard.diff_map.as_ref().expect("diff map populated");
            dm.hunks()
                .map(|hunk| (hunk.buffer_line_range.clone(), hunk.unstaged_lines.clone()))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            extents(true),
            extents(false),
            "the tree pass renames the hunks and moves none of their lines",
        );
    }

    /// A whitespace-only edit marks the row in the gutter but washes nothing,
    /// which is an empty detail rather than an absent one.
    #[test]
    fn a_whitespace_only_edit_carries_empty_detail() {
        const BASE: &str = "fn f() {\n    let a = 1;\n}\n";
        const BUFFER: &str = "fn f() {\n        let a = 1;\n}\n";

        let (h, buffer_id) = settled_rust_diff(BASE, BUFFER);
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        let modified: Vec<_> = dm
            .hunks()
            .filter(|hunk| hunk.status == DiffHunkStatus::Modified)
            .collect();
        assert_eq!(modified.len(), 1, "one modified hunk: {modified:?}");
        let detail = modified[0]
            .token_detail
            .as_ref()
            .expect("a modified hunk carries detail, empty or not");
        assert_eq!(
            (detail.buffer_spans.len(), detail.base_spans.len()),
            (0, 0),
            "the reindented row washes nothing"
        );
    }

    /// A file with no language never reaches the tree differ, so its washes
    /// stay the line pass's char-refined spans.
    #[test]
    fn a_file_with_no_language_keeps_the_char_refined_spans() {
        let (h, buffer_id) = settled_plain_diff("alpha\n", "alPHa\n");
        let spans = detail_spans_of(&h, buffer_id);
        assert!(
            spans.iter().any(|(_, count)| *count > 0),
            "the line pass still refines the changed characters: {spans:?}"
        );
    }

    /// A newline added after the last line pairs that line with itself, so the
    /// row keeps its gutter mark and neither side washes any of its text.
    #[test]
    fn a_newline_added_at_the_end_of_a_file_with_no_language_washes_nothing() {
        let (h, buffer_id) = settled_plain_diff("alpha", "alpha\n");
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        let modified: Vec<_> = dm
            .hunks()
            .filter(|hunk| hunk.status == DiffHunkStatus::Modified)
            .collect();
        assert_eq!(modified.len(), 1, "one modified hunk: {modified:?}");

        let detail = modified[0]
            .token_detail
            .as_ref()
            .expect("the line pass details its modified hunk");
        assert_eq!(
            (
                detail.buffer_spans.len(),
                detail.base_spans.len(),
                dm.base_line_refined(0)
            ),
            (0, 0, true),
            "a terminator-only change names no changed chars, and the base row keeps its text"
        );
    }
    /// The base text of `buffer_id`'s installed diff map.
    fn base_text_of(h: &TestHarness, buffer_id: BufferId) -> String {
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        dm.base_text().expect("base text").to_string()
    }

    /// The settled diff map of a file whose HEAD holds a/b/c/d, whose index
    /// holds b->B, and whose working tree adds d->D, under `base`.
    ///
    /// The working tree's pick is the index, so `None` reads the index.
    fn half_staged_map(base: Option<DiffBase>) -> DiffMap {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
        );
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Index);
        h.stoat.active_workspace_mut().set_diff_base(base);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/f.txt"));
        h.settle_diff_jobs();

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard.diff_map.clone().expect("diff map populated")
    }

    /// Each hunk as `(buffer_start_line, staged)`.
    fn hunk_flags(dm: &DiffMap) -> Vec<(u32, bool)> {
        dm.hunks()
            .map(|hunk| (hunk.buffer_start_line, hunk.staged()))
            .collect()
    }

    /// Under the index pick, the index's own change is a staged mark beside its
    /// base line, and the one hunk is the working tree's edit.
    #[test]
    fn an_index_base_reads_the_index_hunk_as_a_staged_mark() {
        let dm = half_staged_map(None);

        assert_eq!(
            hunk_flags(&dm),
            [(3, false)],
            "the unstaged d->D hunk alone"
        );
        assert_eq!(
            dm.staged_mark_at_base_line(1),
            Some(DiffHunkStatus::Modified),
            "b->B is a staged mark on base line 1",
        );
        assert_eq!(dm.staged_counts(), (1, 1), "the mark and the hunk");
    }

    /// The HEAD base shows every change since the commit as a hunk, and the
    /// index marks the one it already holds.
    #[test]
    fn a_head_base_reads_index_hunks_staged_and_worktree_edits_unstaged() {
        let dm = half_staged_map(Some(DiffBase::Head));

        assert_eq!(
            hunk_flags(&dm),
            [(1, true), (3, false)],
            "the index-staged line-1 hunk is staged, the line-3 hunk is not",
        );
        assert_eq!(
            *dm.staged_marks,
            Vec::<StagedMark>::new(),
            "and no staged marks"
        );
    }

    /// A file added to the index has no HEAD blob. It diffs against its index
    /// text, and the whole of that text is a staged addition.
    #[test]
    fn a_file_added_to_the_index_diffs_against_it() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        {
            let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            builder.index_file("new.rs", "x\n");
            builder.unstaged_file("new.rs", "x\ny\n");
        }
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/new.rs"));
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Index);
        h.settle_diff_jobs();

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        assert_eq!(
            base_text_of(&h, buffer_id),
            "x\n",
            "the index text is the base"
        );
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        assert_eq!(hunk_flags(dm), [(1, false)], "the working tree's added y");
        assert_eq!(
            *dm.staged_marks,
            [StagedMark {
                status: DiffHunkStatus::Added,
                base_lines: 0..1,
            }],
            "the index's x is a staged addition",
        );
    }

    /// A diff toggle's hunks land without the base's colors, which cost a parse
    /// of the whole base file, and the background pass adds the colors later.
    #[test]
    fn the_diff_toggle_opens_on_hunks_and_takes_its_colors_later() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        let map = |h: &TestHarness| {
            let ws = h.stoat.active_workspace();
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            let dm = guard.diff_map.as_ref().expect("diff map installed");
            (
                dm.hunks_in_range(0..u32::MAX).len(),
                dm.base_highlights_for_line(0).is_some(),
            )
        };

        h.stoat.toggle_diff_view();
        h.settle();
        assert_eq!(
            map(&h),
            (1, false),
            "the toggle opens on the hunk, with the base left uncolored",
        );
        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "and leaves the version unrecorded, so the background pass takes it up",
        );

        h.settle_diff_jobs();
        assert_eq!(
            map(&h),
            (1, true),
            "which lands the same hunk with its colors",
        );
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "and the whole map settles",
        );
    }

    /// A two-file repo where `a.rs` and `b.rs` differ from HEAD and `clean.rs`
    /// does not, opened on `a.rs` with the diff latch armed.
    fn latched_harness() -> TestHarness {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[
                ("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n"),
                ("b.rs", "fn baz() {}\n", "fn baz() {}\nfn qux() {}\n"),
            ],
        );
        // Matching HEAD and working text, which stage_review_scenario refuses,
        // so the latch has a genuinely clean file to fall back to plain on.
        h.fake_fs()
            .insert_file(Path::new("/repo/clean.rs"), b"fn same() {}\n");
        {
            let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            builder.head_file("clean.rs", "fn same() {}\n");
        }
        h.open_file(Path::new("/repo/a.rs"));
        h.stoat.toggle_diff_view();
        h
    }

    fn diff_view_on(h: &mut TestHarness) -> bool {
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .diff_view
    }

    fn latched(h: &TestHarness) -> bool {
        let panes = &h.stoat.active_workspace().panes;
        panes.pane(panes.focus()).diff_mode
    }

    #[test]
    fn a_latched_pane_opens_the_next_modified_file_as_a_diff() {
        let mut h = latched_harness();
        h.open_file(Path::new("/repo/b.rs"));

        assert!(
            diff_view_on(&mut h),
            "navigation while latched carries review mode to the new editor"
        );
        assert_eq!(
            h.stoat
                .active_workspace()
                .editors
                .iter()
                .filter(|(_, e)| e.diff_view)
                .count(),
            1,
            "and only the one editor the pane now shows"
        );
    }

    /// The latch decides review mode per file, so a file with nothing against
    /// HEAD reads as a plain editor without disarming the session.
    #[test]
    fn a_clean_file_shows_plain_while_the_latch_stays_armed() {
        let mut h = latched_harness();
        h.open_file(Path::new("/repo/clean.rs"));

        assert_eq!(
            (diff_view_on(&mut h), latched(&h)),
            (false, true),
            "a clean file opens plain with the latch still on"
        );

        h.open_file(Path::new("/repo/b.rs"));
        assert!(
            diff_view_on(&mut h),
            "so hopping back to a modified file re-enters the diff"
        );
    }

    /// Under the index base a file whose every change is staged has marks and
    /// no hunks, and its left column still has those marks to show.
    #[test]
    fn a_latched_pane_opens_a_file_with_only_staged_changes_as_a_diff() {
        let mut h = latched_harness();
        h.fake_fs()
            .insert_file(Path::new("/repo/staged.rs"), b"fn new() {}\n");
        {
            let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            builder.head_file("staged.rs", "fn old() {}\n");
            builder.index_file("staged.rs", "fn new() {}\n");
        }
        h.open_file(Path::new("/repo/staged.rs"));

        assert!(diff_view_on(&mut h), "the staged change opens the diff");
    }

    #[test]
    fn toggling_off_over_a_clean_file_disarms_the_latch() {
        let mut h = latched_harness();
        h.open_file(Path::new("/repo/clean.rs"));
        h.stoat.toggle_diff_view();

        assert!(
            !latched(&h),
            "the toggle exits from a latched pane showing a plain file"
        );

        h.open_file(Path::new("/repo/b.rs"));
        assert!(
            !diff_view_on(&mut h),
            "and a later modified file stays plain"
        );
    }

    /// A file present in HEAD but gone from the working tree diffed into one
    /// hunk per run of removed lines, so a file whose content came in two
    /// blocks tallied as two removals in the statusline. A removed file is one
    /// removal.
    ///
    /// The file is seeded into HEAD alone, with nothing written to the fake fs,
    /// because that absence is what makes the open substitute a lone newline
    /// and the diff read as a removal.
    #[test]
    fn a_removed_file_is_one_deleted_hunk() {
        let head = "fn one() {}\n\nfn two() {}\n";
        let (h, buffer_id) = removed_file_harness(head, false);

        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");

        // A removal occupies no buffer rows, so the zero-width hunk never
        // answers a range query. Read the tree itself instead.
        assert_eq!(
            dm.hunks()
                .map(|h| (h.status, h.base_byte_range.clone()))
                .collect::<Vec<_>>(),
            vec![(DiffHunkStatus::Deleted, 0..head.len())],
            "the removal is one hunk spanning the whole base"
        );
        assert_eq!(
            dm.staged_counts(),
            (0, 1),
            "and the statusline tallies it as a single unstaged change"
        );
    }

    #[test]
    fn a_staged_removal_reads_staged() {
        let (h, buffer_id) = removed_file_harness("fn one() {}\n\nfn two() {}\n", true);

        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");

        assert_eq!(
            dm.staged_counts(),
            (1, 0),
            "git rm already put the removal in the index"
        );
    }

    /// A harness holding one repo file that HEAD has and the working tree does
    /// not, with its diff map installed.
    ///
    /// `staged` also drops the index entry, which is what `git rm` leaves
    /// behind. Without it the index keeps HEAD's content, as a plain `rm`
    /// leaves it.
    fn removed_file_harness(head: &str, staged: bool) -> (TestHarness, BufferId) {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        {
            let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            builder.head_file("gone.rs", head);
            if staged {
                builder.remove_index_file("gone.rs");
            }
        }
        h.open_file(Path::new("/repo/gone.rs"));

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        h.settle_diff_map(buffer_id);

        (h, buffer_id)
    }

    /// Stand up `/repo/a.rs` with the three texts a checked-out review has:
    /// the base commit's content, the checked-out commit's content in HEAD,
    /// and the working tree's own. Returns the harness with `base` installed
    /// and the buffer's diff computed against it.
    fn based_harness(base: Option<DiffBase>, rev: &str, head: &str, working: &str) -> DiffMap {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        {
            let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            builder.commit("base1", &[("a.rs", rev)]);
            builder.head_file("a.rs", head);
            builder.unstaged_file("a.rs", working);
        }
        h.open_file(Path::new("/repo/a.rs"));
        h.stoat.active_workspace_mut().set_diff_base(base);

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        h.settle_diff_map(buffer_id);

        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard.diff_map.clone().expect("diff map populated")
    }

    #[test]
    fn a_rev_base_diffs_against_the_named_commit() {
        let dm = based_harness(
            Some(DiffBase::parent_of("c1", Some("base1".into()))),
            "old\n",
            "mid\n",
            "new\n",
        );
        assert_eq!(
            dm.base_text().map(|t| t.as_str()),
            Some("old\n"),
            "the base is the named commit, not HEAD"
        );
    }

    /// The rev is what the review is against and HEAD is the commit checked
    /// out over it, so the same staged machinery splits the two apart.
    #[test]
    fn a_rev_base_reads_in_commit_hunks_staged_and_worktree_edits_unstaged() {
        let dm = based_harness(
            Some(DiffBase::parent_of("c1", Some("base1".into()))),
            "a\nx\nb\n",
            "A\nx\nb\n",
            "A\nx\nB\n",
        );
        assert_eq!(
            dm.staged_counts(),
            (1, 1),
            "the commit's own hunk reads staged and the worktree's reads unstaged"
        );
    }

    #[test]
    fn a_rev_base_with_no_sha_diffs_against_nothing() {
        let dm = based_harness(
            Some(DiffBase::parent_of("c1", None)),
            "old\n",
            "mid\n",
            "new\n",
        );
        assert_eq!(
            dm.base_text().map(|t| t.as_str()),
            Some(""),
            "a root commit's parent is the empty tree"
        );
    }

    #[test]
    fn a_memory_base_diffs_against_the_supplied_text() {
        let files = std::iter::once((
            PathBuf::from("/repo/a.rs"),
            Arc::new("proposed\n".to_string()),
        ))
        .collect();
        let dm = based_harness(Some(DiffBase::Memory { files }), "old\n", "mid\n", "new\n");
        assert_eq!(
            dm.base_text().map(|t| t.as_str()),
            Some("proposed\n"),
            "the supplied text is the base"
        );
        assert_eq!(
            dm.staged_counts(),
            (0, 1),
            "and nothing has applied an agent's proposal, so the hunk is unstaged"
        );
    }

    #[test]
    fn a_memory_base_falls_back_for_a_path_it_does_not_carry() {
        let dm = based_harness(
            Some(DiffBase::Memory {
                files: HashMap::new(),
            }),
            "old\n",
            "mid\n",
            "new\n",
        );
        assert_eq!(
            dm.base_text().map(|t| t.as_str()),
            Some("mid\n"),
            "an untouched file keeps the working tree's own base"
        );
    }

    #[test]
    fn every_base_names_both_sides() {
        let memory = DiffBase::Memory {
            files: HashMap::new(),
        };
        let pair = |base_path: &str, path: &str| DiffBase::Pair {
            path: path.into(),
            base_path: base_path.into(),
            text: Arc::new(String::new()),
        };
        let sides = [
            diff_sides(None),
            diff_sides(Some(&DiffBase::Head)),
            diff_sides(Some(&DiffBase::named("main", "abc1234def5678".into()))),
            diff_sides(Some(&DiffBase::named("abc12", "abc1234def5678".into()))),
            diff_sides(Some(&DiffBase::parent_of(
                "def5678abc1234",
                Some("abc1234def5678".into()),
            ))),
            diff_sides(Some(&DiffBase::parent_of("def5678abc1234", None))),
            diff_sides(Some(&memory)),
            diff_sides(Some(&pair("/p/a.txt", "/p/b.txt"))),
            diff_sides(Some(&pair("/p/x/conf.toml", "/p/y/conf.toml"))),
        ];
        assert_eq!(
            sides
                .each_ref()
                .map(|(left, right)| (left.as_str(), right.as_str())),
            [
                ("index", "working tree"),
                ("HEAD", "working tree"),
                ("main abc1234", "working tree"),
                ("abc1234", "working tree"),
                ("parent abc1234", "commit def5678"),
                ("empty", "commit def5678"),
                ("original", "proposal"),
                ("a.txt", "b.txt"),
                ("x/conf.toml", "y/conf.toml"),
            ],
        );
    }

    /// An amend renames the reviewed commit only where the base names the
    /// commit that the amend rewrote.
    #[test]
    fn an_amend_renames_only_the_commit_it_rewrote() {
        let mut h = TestHarness::with_size(80, 24);
        let ws = h.stoat.active_workspace_mut();
        ws.set_diff_base(Some(DiffBase::parent_of("c1", Some("c0".into()))));

        let sides = ["c9", "c1"].map(|old_sha| {
            ws.follow_amended_commit(old_sha, "c2");
            diff_sides(ws.diff_base()).1
        });

        assert_eq!(
            sides,
            ["commit c1", "commit c2"],
            "an amend of another commit leaves the name"
        );
    }

    #[test]
    fn setting_a_diff_base_drops_the_cached_blobs() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        {
            let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            builder.head_file("a.rs", "old\n");
            builder.unstaged_file("a.rs", "new\n");
        }
        h.open_file(Path::new("/repo/a.rs"));

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        h.settle_diff_map(buffer_id);
        assert!(
            h.stoat
                .active_workspace()
                .diff
                .holds(buffer_id, Some(Path::new("/repo/a.rs"))),
            "the blobs are cached before the base moves"
        );

        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::parent_of("c1", None)));
        assert!(
            !h.stoat
                .active_workspace()
                .diff
                .holds(buffer_id, Some(Path::new("/repo/a.rs"))),
            "a base change drops blobs read against the base it replaced"
        );
    }

    #[test]
    fn a_crlf_head_leaves_an_lf_buffer_of_equal_content_unchanged() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\r\nb\r\n", "a\nb\n")]);
        h.open_file(Path::new("/repo/a.txt"));

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        h.settle_diff_map(buffer_id);

        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        let starts: Vec<u32> = dm
            .hunks_in_range(0..u32::MAX)
            .iter()
            .map(|hunk| hunk.buffer_start_line)
            .collect();
        assert_eq!(
            starts,
            Vec::<u32>::new(),
            "a HEAD blob differing from the buffer only in its line terminators \
             carries no change",
        );
    }

    #[test]
    fn diff_job_highlights_the_base_text() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.rs", "fn main() {}\n", "fn other() {}\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();

        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().unwrap();
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        let spans = dm
            .base_highlights_for_line(0)
            .expect("the base's keyword line is highlighted");
        assert!(
            !spans.is_empty(),
            "base line 0 carries tree-sitter token spans"
        );
    }

    #[test]
    fn diff_job_leaves_base_unhighlighted_without_a_language() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("notes.unknownext", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/notes.unknownext"));
        h.settle_diff_jobs();

        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().unwrap();
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        assert!(
            dm.base_highlights_for_line(0).is_none(),
            "a file with no language leaves the base unhighlighted"
        );
    }

    #[test]
    fn diff_job_leaves_a_buffer_outside_any_repo_without_a_diff_map() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.set_diff_warm_auto(true);
        let path = h.write_file("loose.txt", "x\ny\n");
        h.open_file(&path);
        h.settle_diff_jobs();

        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        assert!(
            buffer.read().expect("poisoned").diff_map.is_none(),
            "a buffer outside any repo gets no diff map"
        );
    }

    /// A repo at `/repo` with one tracked change, and `fresh.rs` on disk that
    /// no blob holds.
    fn repo_with_fresh_file() -> TestHarness {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("tracked.rs", "one\n", "ONE\n")]);
        h.fake_fs().insert_file("/repo/fresh.rs", b"a\nb\n");
        h.stoat.set_diff_warm_auto(true);
        h
    }

    /// Opens `path`, settles its diff job, and reads the map the job installs.
    fn opened_diff_map<T>(
        h: &mut TestHarness,
        path: &Path,
        read: impl FnOnce(Option<&DiffMap>) -> T,
    ) -> T {
        h.open_file(path);
        h.settle_diff_jobs();

        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer = ws
            .buffers
            .get(ws.editors[editor_id].buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("poisoned");
        read(guard.diff_map.as_ref())
    }

    /// No blob holds an untracked file, so it diffs against empty text, and
    /// every line is one addition that nothing has staged.
    #[test]
    fn diff_job_gives_an_untracked_buffer_one_added_hunk() {
        let mut h = repo_with_fresh_file();
        let read = opened_diff_map(&mut h, Path::new("/repo/fresh.rs"), |diff_map| {
            let diff_map = diff_map.expect("diff map populated");
            let hunks: Vec<_> = diff_map
                .hunks_in_range(0..u32::MAX)
                .into_iter()
                .map(|hunk| (hunk.buffer_line_range.clone(), hunk.status, hunk.staged()))
                .collect();
            (
                diff_map.base_text().map(|text| text.to_string()),
                hunks,
                diff_map.staged_marks.is_empty(),
            )
        });
        assert_eq!(
            read,
            (
                Some(String::new()),
                vec![(0..2, DiffHunkStatus::Added, false)],
                true
            ),
            "the file diffs against empty text as one unstaged addition with no staged mark",
        );
    }

    #[test]
    fn diff_job_leaves_an_ignored_untracked_buffer_without_a_diff_map() {
        let mut h = repo_with_fresh_file();
        h.fake_git().add_repo("/repo").ignored("fresh.rs");
        assert!(
            opened_diff_map(&mut h, Path::new("/repo/fresh.rs"), |diff_map| diff_map
                .is_none()),
            "an ignored file is no work, so it gets no diff map",
        );
    }

    /// The workspace's repo answers for every buffer, so a file outside its
    /// workdir resolves against it too.
    #[test]
    fn diff_job_leaves_a_buffer_outside_the_workdir_without_a_diff_map() {
        let mut h = repo_with_fresh_file();
        let path = h.write_file("loose.txt", "x\ny\n");
        assert!(
            opened_diff_map(&mut h, &path, |diff_map| diff_map.is_none()),
            "a file outside the workdir is no work of the repo, so it gets no diff map",
        );
    }
}
