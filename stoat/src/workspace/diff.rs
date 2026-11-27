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
    action_handlers::movement::{ChangeDir, ChangeUnit},
    buffer::{BufferId, TextBufferSnapshot},
    buffer_registry::BufferRegistry,
    code_index::build::file_id,
    diff::{self, ReviewFileInput},
    diff_cache::ContentHash,
    diff_map::{
        changes_to_hunks, clear_prose, line_starts, mark_approved, mark_staged,
        merge_structural_detail, staged_marks, BaseHighlights, DiffHunk, DiffHunkStatus, DiffMap,
        MarkKind,
    },
    display_map::{highlights::HighlightStyle, syntax_theme::SyntaxStyles},
    editor_state::EditorId,
    host::{
        git::{HunkTallies, MergeState},
        FsHost, GitHost, GitRepo,
    },
    jumplist::JumpEntry,
    multi_buffer::MultiBuffer,
    review_approval::ApprovedRows,
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
    structural_diff::{self, BufferRef, DiffChange, MatchedPairs, MoveResidue, TreeCache},
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
pub const DIFF_SETTLE: Duration = Duration::from_millis(250);

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
pub struct ChangedRangesScan {
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
pub enum DiffBase {
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
    pub fn named(name: &str, sha: String) -> Self {
        Self::Rev {
            sha: Some(sha),
            origin: RevOrigin::Named(name.to_string()),
        }
    }

    /// The parent of `commit`, as the base of a review that checked `commit`
    /// out.
    ///
    /// A `parent` of `None` is the empty tree under a root commit.
    pub fn parent_of(commit: &str, parent: Option<String>) -> Self {
        Self::Rev {
            sha: parent,
            origin: RevOrigin::ParentOf(commit.to_string()),
        }
    }

    /// `base`, the merge base of `from` and `against`, as the base of a review
    /// of `from` against `against`.
    pub fn review(base: String, from: &str, against: &str) -> Self {
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
/// A diff view's status row names the two columns from it.
#[derive(Clone)]
pub enum RevOrigin {
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
pub struct PairBase {
    pub base_path: PathBuf,
    /// The content of `base_path`, which follows the base file's buffer while
    /// that buffer is open.
    pub text: Arc<String>,
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
pub enum WorktreeBase {
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
pub struct RepoTally {
    tallies: HunkTallies,
    /// The files changed since the revision base, `None` under any other base.
    rev_changed: Option<Vec<PathBuf>>,
    /// The name the status bar gives HEAD, the branch it is on or its short
    /// sha when detached. `None` on an unborn branch.
    head: Option<String>,
    /// How far HEAD's branch has moved from its upstream, `(ahead, behind)`.
    upstream: Option<(usize, usize)>,
    /// Whether a rebase is paused in the repository.
    rebasing: bool,
    /// The merge in progress, if one is.
    merge: Option<MergeState>,
}

/// The per-buffer bookkeeping that decides when a diff is owed, and what the
/// last one read.
///
/// Every entry is keyed by buffer except the blob cache and the pairs, which
/// are keyed by path, because a blob or a pair belongs to a file rather than
/// to whichever buffer happens to hold it open.
#[derive(Default)]
pub struct DiffState {
    /// In-flight diff-map population jobs, one per buffer. Held so the spawned
    /// blocking diff is not cancelled before it installs its [`DiffMap`] on the
    /// buffer.
    pub jobs: HashMap<BufferId, DiffJob>,
    /// Diffs that a caller waits on, one per buffer, with the landings to run
    /// once the hunks arrive.
    ///
    /// A diff open places the view and the cursor by the hunks, and resolving
    /// the base and running the line pass costs 5-50 ms. A want runs both on
    /// the blocking pool, so the loop paints the plain frame meanwhile. A
    /// changed-file hop wants the drive's whole job instead, since it lands on
    /// the runs the tree pass makes. A `.git` write marks a want superseded and
    /// never drops it, so its landings still run.
    pub wants: HashMap<BufferId, DiffWant>,
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
    pub(super) repo_hunk_tallies: Option<Arc<HunkTallies>>,
    /// The files changed since the revision base, absolute, as the last tally
    /// read them under one. `None` under any other base.
    ///
    /// The tally counts against HEAD, but under a revision base the changes
    /// view lists everything changed since that revision, a file committed
    /// since and clean in the tree included.
    pub(super) rev_changed: Option<Vec<PathBuf>>,
    /// The name the status bar gives HEAD as the last tally read it, the branch
    /// HEAD is on or its short sha when detached.
    ///
    /// Read with the tally, since a git re-read that moves HEAD stales the
    /// tally too.
    pub(super) repo_head: Option<String>,
    /// How far HEAD's branch had moved from its upstream at the last tally,
    /// `(ahead, behind)`, or `None` without an upstream.
    pub(super) upstream: Option<(usize, usize)>,
    /// Whether a rebase was paused in the repository at the last tally.
    pub(super) rebasing: bool,
    /// The merge in progress at the last tally, if one was.
    ///
    /// Read with the tally, since a merge's start, abort, and commit each move
    /// the files the tally counts.
    pub(super) merge: Option<MergeState>,
    /// The in-flight repo-wide tally, held so it is not cancelled before its
    /// answer lands.
    pub tally_job: Option<Task<Option<RepoTally>>>,
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
    /// The rows a range review approved, keyed by absolute path.
    ///
    /// A base change clears them, so they live as long as the review that set
    /// them. A diff of the file reads them as its staged state.
    pub(super) approved: HashMap<PathBuf, Arc<ApprovedRows>>,
    /// The files in the pairing across files, keyed by buffer.
    ///
    /// Each holds the residue of its last whole diff, so code that left one
    /// open file and arrived in another reads as moved in both. A buffer's
    /// entry goes when its buffer closes, when a diff of it lands with no
    /// residue, and when the base changes.
    shares: HashMap<BufferId, FileMoves>,
    /// Whether a closed buffer left the pairing, which the next drive re-pairs
    /// without it.
    ///
    /// A buffer closes where no buffer registry is at hand, so the re-pair
    /// waits for the drive.
    pairing_stale: bool,
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

    /// The approvals a diff of `path` reads under `diff_base`, `None` where
    /// the staged state comes from git.
    ///
    /// A range review answers even with no row approved, so its map reads
    /// approvals rather than the staged side. Any other base answers the rows
    /// stored for `path`, which a base change clears.
    fn approved_for(&self, path: &Path, diff_base: Option<&DiffBase>) -> Option<Arc<ApprovedRows>> {
        let review = matches!(
            diff_base,
            Some(DiffBase::Rev {
                origin: RevOrigin::Review { .. },
                ..
            })
        );
        self.approved
            .get(path)
            .cloned()
            .or_else(|| review.then(|| Arc::new(ApprovedRows::default())))
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
    pub fn invalidate(&mut self, id: BufferId, path: Option<&Path>) {
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
    /// flight and the approvals of the review the base replaces.
    ///
    /// A job in flight read the base that the change replaces. After a `.git`
    /// write, [`Self::invalidate_all`] keeps such a job, because its blobs are
    /// no staler than the map on screen. After a base change, its map shows a
    /// base the reader has left, so the job goes.
    pub(super) fn reset_for_new_base(&mut self) {
        self.jobs.clear();
        self.approved.clear();
        self.shares.clear();
        self.invalidate_all();
    }

    /// Mark the repo-wide tally as owed, so the next drive spawns one.
    ///
    /// This serves the events that move the tally without staling any buffer
    /// diff. A landed save is the case in point: the file's own hunks are
    /// unchanged by the write, but the repo's counts are not. A working-tree
    /// write that the watcher reports is another. Another tool's write moves
    /// the repo's counts, also for a file that no buffer holds.
    pub fn stale_tally(&mut self) {
        self.tally_current = false;
    }

    /// Whether `id`'s installed map was computed for buffer version `version`.
    pub fn current(&self, id: BufferId, version: u64) -> bool {
        self.versions.get(&id) == Some(&version)
    }

    /// Whether `id`'s installed map holds the hunks for buffer version
    /// `version` and waits for its colors.
    pub(super) fn hunks_installed(&self, id: BufferId, version: u64) -> bool {
        self.hunks_only.get(&id) == Some(&version)
    }

    /// Whether a hunks-only diff of `id` is on the pool.
    #[cfg(any(test, feature = "test-support"))]
    pub fn want_pending(&self, id: BufferId) -> bool {
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
    pub fn release(&mut self, id: BufferId, path: Option<&Path>) {
        self.jobs.remove(&id);
        self.wants.remove(&id);
        self.versions.remove(&id);
        self.hunks_only.remove(&id);
        self.diffed.remove(&id);
        self.settle.remove(&id);
        self.settle_timers.remove(&id);
        if self.shares.remove(&id).is_some() {
            self.pairing_stale = true;
        }
        if let Some(path) = path {
            self.base_text.remove(path);
        }
    }

    /// Whether anything is still held for `id`, or for `path`'s blobs.
    ///
    /// Kept beside [`Self::release`] so the two lists stay the same list.
    #[cfg(any(test, feature = "test-support"))]
    pub fn holds(&self, id: BufferId, path: Option<&Path>) -> bool {
        self.jobs.contains_key(&id)
            || self.wants.contains_key(&id)
            || self.versions.contains_key(&id)
            || self.hunks_only.contains_key(&id)
            || self.diffed.contains_key(&id)
            || self.settle.contains_key(&id)
            || self.settle_timers.contains_key(&id)
            || self.shares.contains_key(&id)
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

        let grammar = language_registry.for_path(&path);
        let has_grammar = grammar.is_some();
        // A hunks-only job takes no language, memo, tree, or anchors. Its
        // hunks lack anchors on purpose, which is what tells
        // `DiffMap::renders_same_as` that the drive's colored map decorates
        // differently. It still takes `has_grammar`, so its line-pass spans in
        // a file with a grammar read as code.
        let (language, buffer_tree) = match whole {
            true => (
                grammar,
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
            let approved = self.approved_for(&path, diff_base.as_ref());
            let cached_base = self.base_text.get(&path).cloned();
            let tree_cache = whole.then(|| self.tree_cache.clone());
            move || {
                let out = run_diff_job(
                    &*git_host,
                    &git_root,
                    path,
                    diff_base.as_ref(),
                    cached_base,
                    approved,
                    &buffer_snapshot,
                    target_version,
                    id,
                    language.as_ref(),
                    has_grammar,
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
        redraw_notify: &Arc<Notify>,
    ) -> Vec<(BufferId, Vec<DiffLanding>)> {
        let waker = stoat_scheduler::notify_waker(redraw_notify);
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
    #[cfg(any(test, feature = "test-support"))]
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
        if self.pairing_stale {
            self.pairing_stale = false;
            self.install_paired(buffers, None);
        }
        self.refresh_pair_bases(buffers, executor, redraw_notify);

        let mut completed: Vec<(DiffJobOutput, bool, bool, Instant)> = Vec::new();
        if !self.jobs.is_empty() {
            let waker = stoat_scheduler::notify_waker(redraw_notify);
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
        }
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
            let has_grammar = language.is_some();
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
                let approved = self.approved_for(&path, diff_base.as_ref());
                let tree_cache = self.tree_cache.clone();
                move || {
                    let out = run_diff_job(
                        &*git_host,
                        &git_root,
                        path,
                        diff_base.as_ref(),
                        cached_base,
                        approved,
                        &buffer_snapshot,
                        cur_version,
                        buffer_id,
                        language.as_ref(),
                        has_grammar,
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
            let waker = stoat_scheduler::notify_waker(redraw_notify);
            let mut cx = Context::from_waker(&waker);
            if let Poll::Ready(tally) = Pin::new(job).poll(&mut cx) {
                // A `None` answer means the root is outside a repo now, which
                // says nothing about the last tally that was read inside one.
                if let Some(tally) = tally {
                    self.repo_hunk_tallies = Some(Arc::new(tally.tallies));
                    self.rev_changed = tally.rev_changed;
                    self.repo_head = tally.head;
                    self.upstream = tally.upstream;
                    self.rebasing = tally.rebasing;
                    self.merge = tally.merge;
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
                        head: repo
                            .head_branch()
                            .or_else(|| repo.resolve_rev("HEAD").map(|sha| short_sha(&sha))),
                        upstream: repo.upstream_divergence(),
                        rebasing: repo.rebase_in_progress(),
                        merge: repo.merge_state(),
                        rev_changed: rev.map(|sha| {
                            repo.changed_files_from(&sha)
                                .into_iter()
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
    pub fn tally_pending(&self) -> bool {
        !self.tally_current || self.tally_job.is_some()
    }

    /// Install `out`'s map on its buffer, and file the blobs it read unless a
    /// later write superseded the job that read them.
    ///
    /// A superseded map still paints, since it read its blobs no earlier than
    /// the map on screen.
    ///
    /// A map that comes with a residue joins the pairing across files, which
    /// installs it with the moves it finds. A map without one leaves the
    /// pairing, and the other files re-pair without it.
    fn land(&mut self, buffers: &BufferRegistry, out: DiffJobOutput, superseded: bool) {
        if !superseded {
            // Filed whether or not it resolved, since a miss is what an
            // ignored buffer keeps re-deriving otherwise.
            self.base_text.insert(out.path, out.base);
        }
        match (out.diff_map, out.moves) {
            (Some(plain), Some(share)) => {
                let moves = FileMoves {
                    plain,
                    share,
                    applied: Vec::new(),
                };
                self.shares.insert(out.buffer_id, moves);
                self.install_paired(buffers, Some(out.buffer_id));
            },
            (diff_map, _) => {
                self.install(buffers, out.buffer_id, diff_map);
                if self.shares.remove(&out.buffer_id).is_some() {
                    self.install_paired(buffers, None);
                }
            },
        }
    }

    /// Install `diff_map` on `id`'s buffer, unless it renders the same as the
    /// map already there.
    fn install(&mut self, buffers: &BufferRegistry, id: BufferId, diff_map: Option<DiffMap>) {
        let Some(shared) = buffers.get(id) else {
            return;
        };
        let mut guard = shared.write().expect("buffer poisoned");
        // A recompute landing on the same hunks is not news. Every decoration
        // consumer keys off the map's version, and the minimap's edge sweep
        // re-derives the whole file from it, so installing an identical map
        // costs that for nothing. A write under .git re-diffs every visible
        // buffer against content that did not move, which is where this earns
        // its keep.
        let same = match (&guard.diff_map, &diff_map) {
            (Some(installed), Some(fresh)) => installed.renders_same_as(fresh),
            (None, None) => true,
            _ => false,
        };
        if !same {
            guard.diff_map = diff_map;
            self.maps_epoch += 1;
        }
    }

    /// Pair the residues of every file that shares one, and install the map of
    /// each file whose moves changed, and of `landed`, with its moves.
    ///
    /// The files pair in path order, so one set of files pairs the same way
    /// whichever diff landed last. The pairing runs on the event thread. A
    /// residue holds only the nodes above unpaired tokens, so an ordinary edit
    /// adds a few nodes, and a new or a removed file its whole tree.
    fn install_paired(&mut self, buffers: &BufferRegistry, landed: Option<BufferId>) {
        let mut ids: Vec<BufferId> = self.shares.keys().copied().collect();
        ids.sort_by(|a, b| {
            self.shares[a]
                .share
                .buffer
                .path
                .cmp(&self.shares[b].share.buffer.path)
        });
        let pairing = {
            let files: Vec<(&BufferRef, &MoveResidue)> = ids
                .iter()
                .map(|id| {
                    let share = &self.shares[id].share;
                    (&share.buffer, &share.residue)
                })
                .collect();
            structural_diff::pair_residues(&files)
        };

        for (id, moved) in ids.into_iter().zip(pairing) {
            let diff_map = {
                let Some(moves) = self.shares.get_mut(&id) else {
                    continue;
                };
                if moved == moves.applied && landed != Some(id) {
                    continue;
                }
                let diff_map = match moved.is_empty() {
                    true => moves.plain.clone(),
                    false => moves.plain.with_moved_changes(&moved, &moves.share.text),
                };
                moves.applied = moved;
                diff_map
            };
            self.install(buffers, id, Some(diff_map));
        }
    }
}

/// A file in the pairing across files, as [`DiffState::shares`] holds it.
struct FileMoves {
    /// The map that the file's diff job built, before any move across files.
    plain: DiffMap,
    share: MoveShare,
    /// The moved changes of the last pairing, which the installed map shows.
    applied: Vec<DiffChange>,
}

pub struct DiffJob {
    pub task: Task<DiffJobOutput>,
    /// A `.git` write, a stage, or a pair base refresh landed after the job
    /// read its blobs, so its map still paints and its version and blobs do not
    /// count.
    pub superseded: bool,
    /// The buffer held the text its last diff read, so only the base moved,
    /// through a git write or a switch of base. Such a job skips the settle
    /// window, and its landing logs how long it ran, which measures how long a
    /// stage takes to reach the gutter.
    pub base_moved: bool,
    pub started: Instant,
}

pub struct DiffJobOutput {
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
    /// What the diff offers the pairing across files, `None` when it ran no
    /// tree pass or measured against another file.
    pub(super) moves: Option<MoveShare>,
}

/// One file's offer to the pairing across files.
///
/// A diff of one file does not see that its removed code arrived in another
/// file. The workspace pairs the residues of the open buffers'
/// diffs, and each file's map takes the moves that the pairing finds.
pub(super) struct MoveShare {
    /// The file, as the moves in other files name it.
    buffer: BufferRef,
    /// The buffer text that the diff ran against, which the moved spans index.
    text: Arc<String>,
    residue: MoveResidue,
}

/// A diff on the blocking pool that a caller waits on, and what runs once it
/// lands.
pub struct DiffWant {
    target_version: u64,
    task: Task<DiffJobOutput>,
    /// What the callers that wanted the hunks do with them, in the order they
    /// asked.
    pub landings: Vec<DiffLanding>,
    /// A `.git` write landed after the want read its blobs, so its map still
    /// paints, and its blobs and its marker do not count.
    superseded: bool,
    /// The job is the drive's, with the tree pass, the colors, and anchors, so
    /// its map counts as current once it lands.
    pub whole: bool,
}

/// What a caller that wanted a buffer's hunks does once they land.
///
/// A diff open places the view and the cursor by the hunks, so each landing is
/// the step that waited for them. One that acts on an editor names it, since
/// the focus moves on its own clock while the hunks are on the pool.
#[derive(Debug, Clone)]
pub enum DiffLanding {
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
    ChangeWalk {
        dir: ChangeDir,
        count: u32,
        unit: ChangeUnit,
    },
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
    /// rows its hunk covers, for a map that offers no stop. `unit` names the
    /// walk that armed the hop, whose stops the landing reads.
    FileHop {
        dir: ChangeDir,
        unit: ChangeUnit,
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
pub(crate) fn settle_timer(executor: &Executor, redraw_notify: &Arc<Notify>) -> Task<()> {
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
/// `approved` holds a range review's approved rows, which mark the hunks
/// staged in place of the base's staged side.
///
/// A diff with a residue offers it in [`DiffJobOutput::moves`], unless the
/// file measures against another file, whose moves name no file of the
/// changeset.
#[allow(clippy::too_many_arguments)]
fn run_diff_job(
    git_host: &dyn GitHost,
    git_root: &Path,
    path: PathBuf,
    diff_base: Option<&DiffBase>,
    cached_base: Option<Option<DiffBaseText>>,
    approved: Option<Arc<ApprovedRows>>,
    buffer_snapshot: &TextBufferSnapshot,
    target_version: u64,
    buffer_id: BufferId,
    language: Option<&Arc<Language>>,
    has_grammar: bool,
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
    // The approvals go on a copy, so the base filed for reuse never holds
    // rows that the next press moves.
    let computed = base.as_ref().and_then(|base| {
        compute_diff_map(
            &buffer_text,
            language,
            has_grammar,
            syntax_styles,
            base_cache,
            &base.clone().with_approved(approved),
            tree_memo,
            buffer_tree,
        )
    });
    let (diff_map, residue) = match computed {
        Some(ComputedDiff { mut map, residue }) => {
            if anchor {
                map.anchor_hunks(buffer_snapshot);
            }
            (Some(map), residue)
        },
        None => (None, None),
    };

    let moves = residue
        .filter(|_| !matches!(diff_base, Some(DiffBase::Pair { .. })))
        .map(|residue| MoveShare {
            buffer: BufferRef {
                path: path.clone(),
                fingerprint: blake3::hash(buffer_text.as_bytes()).into(),
            },
            text: Arc::new(buffer_text),
            residue,
        });
    DiffJobOutput {
        buffer_id,
        path,
        target_version,
        diff_map,
        base,
        moves,
    }
}

/// The names of what the diff view's left and right columns hold under
/// `base`, as `(left, right)`.
///
/// A commit review names the commit's parent on the left and the commit on the
/// right, because the review checks that commit out under the buffers. A
/// revision the reader named keeps its spelling beside the short sha it
/// resolved to. A pair names its two files.
pub fn diff_sides(base: Option<&DiffBase>) -> (String, String) {
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

/// The first seven chars of `sha`, the length the diff status row and the
/// base prompt show.
pub(crate) fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The repository's state as the status bar's repo segment reads it.
///
/// The counts are the tally's. `staged` and `unstaged` count the hunks of
/// tracked files, and `untracked` counts the untracked files apart from them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoStatus {
    /// The branch HEAD is on, or its short sha when detached. `None` on an
    /// unborn branch.
    pub head: Option<String>,
    /// Hunks the index carries over HEAD.
    pub staged: usize,
    /// Hunks the working tree carries over the index in tracked files.
    pub unstaged: usize,
    pub untracked: usize,
    /// Commits HEAD's branch holds that its upstream lacks, zero without an
    /// upstream.
    pub ahead: usize,
    /// Commits the upstream holds that HEAD's branch lacks.
    pub behind: usize,
    /// Whether a rebase is paused in the repository.
    pub rebasing: bool,
    /// The name of the branch a merge in progress brings in, or the merge
    /// head's short sha when no local branch points at it. `None` with no
    /// merge in progress.
    pub merge_head: Option<String>,
}

/// Where a diff view stands in the repository's review, as the bar names it.
///
/// `file_at` of `file_total` places the focused file among the changed files,
/// in the tally's path order. The focused file always counts, since the view
/// shows it whether or not the tally lists it. `hunk_at` of `hunk_total`
/// places the cursor among every hunk in the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HunkPosition {
    pub file_at: usize,
    pub file_total: usize,
    /// `None` for a cursor above the focused file's first hunk, which is the
    /// one place the walk has no hunk behind it.
    pub hunk_at: Option<usize>,
    pub hunk_total: usize,
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
pub fn repo_hunk_position(ws: &Workspace) -> Option<HunkPosition> {
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
        hunk_at: live.hunk_index_at(cursor_row).map(|k| before + k),
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
pub struct DiffBaseText {
    /// The text the left column shows and the hunks describe.
    pub base: Arc<String>,
    /// The text a hunk must already sit in to read staged.
    pub staged_text: Arc<String>,
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
    /// The rows a range review approved, which mark hunks staged in place of
    /// the staged text, `None` where git decides.
    ///
    /// Attached per diff through [`Self::with_approved`] and never cached,
    /// since a press moves the rows while the blobs do not change.
    approved: Option<Arc<ApprovedRows>>,
}

impl DiffBaseText {
    fn new(base: Arc<String>, staged_text: Arc<String>, staged_from: Option<Arc<String>>) -> Self {
        let base_hash = structural_diff::fingerprint_bytes(&base);
        let staged_hash = structural_diff::fingerprint_bytes(&staged_text);
        Self {
            base,
            staged_text,
            staged_from,
            base_hash,
            staged_hash,
            approved: None,
        }
    }

    /// These texts, where `approved` marks the hunks staged.
    fn with_approved(mut self, approved: Option<Arc<ApprovedRows>>) -> Self {
        self.approved = approved;
        self
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
/// marks. When `base` holds a range review's approved rows, those rows decide
/// which hunks read staged, and the map reads [`MarkKind::Approved`].
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
/// With `has_grammar`, the file has a grammar, and the line pass's spans read
/// as code wherever they stand. Without it they read as prose, which the diff
/// view marks more heavily. `language` alone does not say which, because a
/// hunks-only job passes none for a file that has a grammar.
///
/// `buffer_tree` is a tree already parsed from `buffer_text`, which the tree
/// pass takes in place of parsing the buffer a second time. It must describe
/// exactly those bytes. A tree from another grammar is discarded downstream.
///
/// A tree pass also answers its residue, the subtrees that it paired with
/// nothing, which the pairing across files reads. A removed file runs the tree
/// pass for its residue alone, so it offers every subtree that it held. A file
/// with no hunk, no `language`, or a side over the byte limit answers no
/// residue.
#[allow(clippy::too_many_arguments)]
pub(super) fn compute_diff_map(
    buffer_text: &str,
    language: Option<&Arc<Language>>,
    has_grammar: bool,
    syntax_styles: &SyntaxStyles,
    base_cache: &BaseHighlightCache,
    base: &DiffBaseText,
    tree_memo: Option<&TreeCache>,
    buffer_tree: Option<&Tree>,
) -> Option<ComputedDiff> {
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
    let mut residue = None;
    let mut diff_map = if buffer_removed(buffer_text) && had_content {
        if base_text.is_empty() {
            // The removal is already in the base, so the staged mark the
            // text behind it yields below is all there is to show.
            DiffMap::from_hunks([], Some(base.base.clone()))
        } else {
            let anchor = 0..0;
            // The removal is staged once the staged side is gone too. The
            // line differ answers this wrong. Against an empty staged text it
            // reads the removed buffer's lone newline as an added line. Under
            // a review, the approval of the anchor's row decides.
            let staged = match &base.approved {
                Some(approved) => approved.covers(&(0..1)),
                None => buffer_removed(staged_text),
            };
            let unstaged_lines = match staged {
                true => Vec::new(),
                false => vec![anchor.clone()],
            };
            // The removal keeps one hunk, but its residue offers every subtree
            // that the file held, so code that it lost to another open file
            // reads as moved there.
            residue = language
                .and_then(|language| {
                    structural_diff::prepare_diff(
                        language,
                        base_text,
                        buffer_text,
                        None,
                        tree_memo,
                        buffer_tree,
                    )
                })
                .map(|prepared| {
                    structural_diff::finalize_single_with_residue(&prepared, MatchedPairs::Skip).1
                });
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
                    relocated_rows: Vec::new(),
                }],
                Some(base.base.clone()),
            )
        }
    } else {
        let mut hunks = changes_to_hunks(&result.changes, base_text, buffer_text);
        if has_grammar {
            clear_prose(&mut hunks);
        }
        mark_staged(&mut hunks, &staged_changed);
        if let Some(approved) = &base.approved {
            mark_approved(&mut hunks, approved.ranges());
        }

        // The tree pass answers only what the washes mark. It details only the
        // hunks the line pass found, so a file with no hunk skips it. A parse
        // failure or a fall back to lines leaves the line pass's char-refined
        // spans in place, which is what the view showed before this ran.
        if !hunks.is_empty()
            && let Some(language) = language
            && let Some(prepared) = structural_diff::prepare_diff(
                language,
                base_text,
                buffer_text,
                None,
                tree_memo,
                buffer_tree,
            )
        {
            let (tree, tree_residue) =
                structural_diff::finalize_single_with_residue(&prepared, MatchedPairs::Skip);
            if !tree.fell_back_to_line_diff {
                merge_structural_detail(
                    &mut hunks,
                    &tree.changes,
                    &tree.unsearched,
                    base_text,
                    buffer_text,
                );
            }
            residue = Some(tree_residue);
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
    if base.approved.is_some() {
        diff_map = diff_map.with_mark_kind(MarkKind::Approved);
    }
    Some(ComputedDiff {
        map: diff_map,
        residue,
    })
}

/// A buffer's diff map, with the residue of its tree pass, as
/// [`compute_diff_map`] answers them.
pub(super) struct ComputedDiff {
    pub(super) map: DiffMap,
    /// The subtrees that the tree pass paired with nothing, for the pairing
    /// across files. `None` when no tree pass ran.
    pub(super) residue: Option<MoveResidue>,
}

/// Read the texts a diff measures `path` against from the repo `git_root` is
/// in, per the workspace's base.
///
/// A pair base answers for its file with no repository. Otherwise [`None`]
/// when `git_root` is in no repo, or as [`base_texts`] answers.
pub fn resolve_base(
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
pub fn scan_changed_ranges(
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
        let base_hash = structural_diff::fingerprint_bytes(input.base_text.as_str());
        let buffer_hash = structural_diff::fingerprint_bytes(input.buffer_text.as_str());

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
        BaseHighlightCache, BaseHighlightMemo, DiffBase, DiffBaseText,
    };
    use crate::{
        diff_map::{BaseHighlights, DiffHunkStatus, DiffMap, MarkKind, ProseChange, StagedMark},
        display_map::syntax_theme::{self, SyntaxStyles},
        review::ReviewFileInput,
        review_approval::ApprovedRows,
        test_fixture::workspace_diff::span_prose,
        theme::Theme,
    };
    use ratatui::style::Color;
    use std::{
        collections::HashMap,
        ops::Range,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use stoat_language::{parse, structural_diff::TreeCache, Language, LanguageRegistry};

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
            true,
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
                true,
                &styles,
                &cache,
                &base,
                None,
                buffer_tree,
            )
            .expect("a tracked file has a diff map")
            .map
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
            true,
            &styles,
            &Arc::new(Mutex::new(BaseHighlightMemo::default())),
            &diff_base(text, text, None),
            Some(&TreeCache::default()),
            None,
        )
        .expect("a tracked file has a diff map")
        .map;

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
        line_pass_diff(buffer, base, false)
    }

    /// [`plain_diff`] in a file that has a grammar when `has_grammar` is set.
    fn line_pass_diff(buffer: &str, base: &DiffBaseText, has_grammar: bool) -> DiffMap {
        compute_diff_map(
            buffer,
            None,
            has_grammar,
            &SyntaxStyles::from_theme(&Theme::empty()),
            &BaseHighlightCache::default(),
            base,
            None,
            None,
        )
        .expect("a diff map")
        .map
    }

    /// A removed file keeps one hunk, and still offers the code that it held
    /// to the pairing across files.
    #[test]
    fn a_removed_file_offers_its_code_as_residue() {
        let language = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .expect("rust language");
        let base = "fn a() { one(1); }\nfn b() { two(2); }\n";
        let computed = compute_diff_map(
            "\n",
            Some(&language),
            true,
            &SyntaxStyles::from_theme(&Theme::empty()),
            &BaseHighlightCache::default(),
            &diff_base(base, base, None),
            None,
            None,
        )
        .expect("a tracked file has a diff map");

        let statuses: Vec<DiffHunkStatus> = computed.map.hunks().map(|hunk| hunk.status).collect();
        assert_eq!(
            (statuses, computed.residue.is_some()),
            (vec![DiffHunkStatus::Deleted], true),
            "one removal, and a residue that holds the removed functions",
        );
    }

    /// The line pass reads no grammar and calls every change prose. Only a
    /// file with no grammar keeps that claim, so a hunk that the tree pass
    /// leaves to the line pass marks code as code.
    #[test]
    fn a_line_pass_span_is_prose_only_in_a_file_with_no_grammar() {
        let base = "let total = alpha + beta;\n";
        let base = diff_base(base, base, None);
        let prose = |has_grammar| {
            span_prose(&line_pass_diff(
                "let total = alpha + gamma;\n",
                &base,
                has_grammar,
            ))
        };

        assert_eq!(
            [prose(false), prose(true)],
            [vec![ProseChange::Part], vec![ProseChange::None]],
            "only a file with no grammar marks the changed part of a line as prose",
        );
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

    /// `base` read on both sides, as a range review reads it, with `rows`
    /// approved, or none.
    fn reviewed(base: &str, rows: Option<Range<u32>>) -> DiffBaseText {
        let mut approved = ApprovedRows::default();
        if let Some(rows) = rows {
            approved.approve(rows);
        }
        diff_base(base, base, None).with_approved(Some(Arc::new(approved)))
    }

    /// Each hunk's first row and whether it reads staged, with the map's mark
    /// kind.
    fn approval_marks(dm: &DiffMap) -> (Vec<(u32, bool)>, MarkKind) {
        (
            dm.hunks()
                .map(|hunk| (hunk.buffer_start_line, hunk.staged()))
                .collect(),
            dm.mark_kind(),
        )
    }

    /// Under a range review the approved rows decide which hunks read staged,
    /// and with none approved every hunk reads unstaged. A base with no
    /// approvals keeps git's meaning for the staged state.
    #[test]
    fn approved_rows_mark_the_hunks_they_cover() {
        let (base, buffer) = ("a\nb\nc\nd\ne\n", "a\nB\nc\nD\ne\n");
        assert_eq!(
            [
                approval_marks(&plain_diff(buffer, &reviewed(base, Some(1..2)))),
                approval_marks(&plain_diff(buffer, &reviewed(base, None))),
                approval_marks(&plain_diff(buffer, &diff_base(base, base, None))),
            ],
            [
                (vec![(1, true), (3, false)], MarkKind::Approved),
                (vec![(1, false), (3, false)], MarkKind::Approved),
                (vec![(1, false), (3, false)], MarkKind::Staged),
            ],
        );
    }

    /// A deletion has no row of its own, so it approves through the row it
    /// sits above, and a file removed whole approves through row 0.
    #[test]
    fn a_deletion_approves_through_its_anchor_row() {
        let deleted = |rows: Option<Range<u32>>| {
            approval_marks(&plain_diff("a\nc\n", &reviewed("a\nb\nc\n", rows))).0
        };
        let removed = |rows: Option<Range<u32>>| {
            approval_marks(&plain_diff("\n", &reviewed("x\ny\n", rows))).0
        };
        assert_eq!(
            [
                deleted(Some(1..2)),
                deleted(None),
                removed(Some(0..1)),
                removed(None)
            ],
            [
                vec![(1, true)],
                vec![(1, false)],
                vec![(0, true)],
                vec![(0, false)]
            ],
        );
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
        syntax_theme::install_highlight_maps(&registry, &styles);
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
}
