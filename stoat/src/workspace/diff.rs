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

use crate::{
    buffer::BufferId,
    buffer_registry::{self, BufferRegistry},
    code_index::build::file_id,
    diff::{self, ReviewFileInput},
    diff_cache::ContentHash,
    diff_map::{
        changes_to_hunks, line_starts, mark_staged, merge_structural_detail, BaseHighlights,
        DiffHunk, DiffHunkStatus, DiffMap,
    },
    display_map::syntax_theme::SyntaxStyles,
    host::{FsHost, GitHost, GitRepo},
};
use codegraph::FileId;
use std::{
    collections::HashMap,
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use stoat_language::{
    extract_highlights, parse, structural_diff, structural_diff::TreeCache, HighlightSpan,
    Language, LanguageRegistry,
};
use stoat_scheduler::{Executor, Task};
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

/// What a workspace's buffers diff against, when that is not the working
/// tree's own HEAD-plus-index.
///
/// Reviewing a commit checks it out and points the diff at the commit's
/// parent, so the base moves off HEAD while the buffers stay the working
/// tree. An agent's proposed edits have no commit behind them at all, so they
/// carry their base as text.
// The constructors (`:diff <rev>`, the review walk, the rebase edit pause, the
// commits view, and the agent-edit entry point) do not exist yet. The base
// resolution below lands ahead of them, and `#[allow(dead_code)]` covers the
// gap until the first one arrives.
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) enum DiffBase {
    /// The tree at `sha`. `None` is the empty tree, which is what a root
    /// commit's parent amounts to.
    Rev { sha: Option<String> },
    /// Base text supplied directly, keyed by absolute path. A path the map
    /// does not carry falls back to the working tree's own base, so an
    /// untouched file still diffs normally.
    Memory {
        files: HashMap<PathBuf, Arc<String>>,
    },
}

/// The per-buffer bookkeeping that decides when a diff is owed, and what the
/// last one read.
///
/// Every entry is keyed by buffer except the blob cache, which is keyed by
/// path, because a blob belongs to a file rather than to whichever buffer
/// happens to hold it open.
#[derive(Default)]
pub(crate) struct DiffState {
    /// In-flight diff-map population jobs, one per buffer. Held so the spawned
    /// blocking diff is not cancelled before it installs its [`DiffMap`] on the
    /// buffer.
    pub(super) jobs: HashMap<BufferId, DiffJob>,
    /// Buffer edit version each buffer's `diff_map` was last populated for.
    ///
    /// Records no-repo and untracked buffers too (with a cleared map) so they
    /// are not retried every frame, and drives re-population when a buffer is
    /// edited past the recorded version.
    pub(super) versions: HashMap<BufferId, u64>,
    /// Each diffed file's HEAD and index blobs, keyed by path.
    ///
    /// Saves the repo mutex and a blob decompression on every recompute, which
    /// is otherwise paid per keystroke. Cleared by [`Self::invalidate_all`],
    /// which the `.git` watcher drives, so an entry cannot outlive the git
    /// state it was read from.
    pub(super) base_text: HashMap<PathBuf, DiffBaseText>,
    /// What buffers diff against, `None` for the ordinary working-tree diff.
    ///
    /// Set through [`super::Workspace::set_diff_base`], which invalidates
    /// every buffer, since a base change moves every diff in the workspace.
    pub(super) base_override: Option<DiffBase>,
    /// Parsed trees the structural refinement reuses across settles.
    ///
    /// A settle re-parses the same base and the same buffer prefix the last one
    /// read, so the memo turns the per-keystroke refinement into one parse of
    /// what actually changed. Shared with the blocking job, which is where the
    /// parses run.
    pub(super) tree_cache: TreeCache,
    /// Files with staged and with unstaged changes across the whole repo, as
    /// the status bar's repo-wide tally reads them.
    ///
    /// `None` until a diff lands, and for a workspace root outside any repo.
    /// Refreshed wherever a diff map does, since the same events move both: an
    /// edit, a save, a staging action, and a write under `.git`.
    pub(super) repo_change_counts: Option<(usize, usize)>,
    /// The version each buffer is currently settling on, and when that version
    /// was first seen. Read by [`Self::settled`].
    settle: HashMap<BufferId, (u64, Instant)>,
    /// The redraw timers [`Self::settled`] arms, held so they are not cancelled
    /// on drop. Replaced per buffer, so a burst keeps one.
    settle_timers: HashMap<BufferId, Task<()>>,
}

impl DiffState {
    /// Force the next drive to recompute `id`'s diff map by dropping its
    /// recorded version and any in-flight job.
    ///
    /// Used after a git-index mutation so the buffer re-diffs. The recompute
    /// stays HEAD-relative, so the hunks are unchanged until the base becomes
    /// index-aware.
    pub(super) fn invalidate(&mut self, id: BufferId) {
        self.jobs.remove(&id);
        self.versions.remove(&id);
    }

    /// Stale every buffer's diff map by dropping all recorded versions and
    /// in-flight jobs.
    ///
    /// Used after git state moves under the editor. An external rebase or
    /// checkout changes HEAD, so a map computed against the old one describes a
    /// base that no longer exists, whatever the buffer's own version says.
    ///
    /// In-flight jobs are dropped as [`Self::invalidate`] drops them, since
    /// their results would carry the same stale base. The next drive recomputes
    /// only visible buffers, so hidden ones re-diff lazily when next shown.
    pub(super) fn invalidate_all(&mut self) {
        self.jobs.clear();
        self.versions.clear();
        // The cached blobs were read from the git state this call is reacting
        // to having changed, so they are exactly what must not be reused.
        self.base_text.clear();
    }

    /// Whether `id`'s installed map was computed for buffer version `version`.
    pub(super) fn current(&self, id: BufferId, version: u64) -> bool {
        self.versions.get(&id) == Some(&version)
    }

    /// Drop everything held for a closing buffer, and `path`'s blobs with it.
    ///
    /// Two of the collections hold a `Task`, and dropping one cancels work
    /// whose result nothing reads any more. [`Self::invalidate`] keeps the
    /// blobs on purpose, since an edit does not move the base, where a close
    /// leaves nothing to reuse them.
    pub(super) fn release(&mut self, id: BufferId, path: Option<&Path>) {
        self.jobs.remove(&id);
        self.versions.remove(&id);
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
            || self.versions.contains_key(&id)
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
                if now.duration_since(*since) < DIFF_SETTLE {
                    return false;
                }
                self.settle.remove(&buffer_id);
                true
            },
            _ => {
                self.settle.insert(buffer_id, (version, now));
                let timer_executor = executor.clone();
                let task = executor.spawn_with_redraw(redraw_notify.clone(), async move {
                    timer_executor.timer(DIFF_SETTLE).await;
                });
                self.settle_timers.insert(buffer_id, task);
                false
            },
        }
    }

    /// Compute and install `id`'s hunks synchronously, bypassing the background
    /// job so they are available on the current turn.
    ///
    /// The diff view's left-column colors are not part of what lands here. They
    /// cost a parse of the whole base file, which is the expensive half of a
    /// diff map and no part of what a caller needing hunks this turn is after.
    /// For a file with a language, the buffer's version is therefore left
    /// unrecorded, so the next [`Self::drive_diff_jobs`] pass recomputes the map
    /// with its colors and installs it a settle window later.
    ///
    /// That replacement rests on the hunks left here carrying no anchors, which
    /// is what tells [`DiffMap::renders_same_as`] the recomputed map decorates
    /// differently. Anchor them here and it suppresses the install instead, and
    /// the colors never arrive.
    ///
    /// A no-op for a buffer without a path.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn install_now(
        &mut self,
        buffers: &BufferRegistry,
        git_root: &Path,
        git_host: &Arc<dyn GitHost>,
        language_registry: &Arc<LanguageRegistry>,
        syntax_styles: &SyntaxStyles,
        base_cache: &BaseHighlightCache,
        id: BufferId,
    ) {
        let Some(path) = buffers.path_for(id).map(Path::to_path_buf) else {
            return;
        };
        let Some(shared) = buffers.get(id) else {
            return;
        };
        let (version, text) = {
            let guard = shared.read().expect("buffer poisoned");
            (
                guard.snapshot.version,
                guard.snapshot.visible_text.to_string(),
            )
        };

        // The language is what gates the base-text parse inside, so withholding
        // it is how the colors are skipped.
        let computed = compute_diff_map(
            &**git_host,
            git_root,
            &path,
            &text,
            None,
            syntax_styles,
            base_cache,
            self.base_text.get(&path).cloned(),
            self.base_override.as_ref(),
            // The sync open stays line-only so it lands in one frame. The
            // structural refinement arrives at the background settle.
            None,
        );
        let diff_map = computed.map(|(diff_map, base)| {
            self.base_text.insert(path.clone(), base);
            diff_map
        });
        if let Some(shared) = buffers.get(id) {
            shared.write().expect("buffer poisoned").diff_map = diff_map;
        }

        if let Some(repo) = git_host.discover(git_root) {
            self.repo_change_counts = Some(repo.change_counts());
        }

        // A file with no language has no colors to wait for, so its map is
        // already whole and nothing has to recompute it.
        if language_registry.for_path(&path).is_none() {
            self.versions.insert(id, version);
        }
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
    }

    /// Populate visible git-tracked buffers' diff maps on a background thread.
    ///
    /// Polls in-flight jobs and installs their diff maps, then spawns a job for
    /// each visible git-tracked buffer whose diff is stale.
    ///
    /// Mirrors [`Self::drive_parse_jobs`] with at most one job per buffer,
    /// coalescing rapid edits by re-queuing only after the in-flight job
    /// completes. A buffer with no path, no repo, or no HEAD content records its
    /// version with a cleared map, so it is not retried until the next edit.
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
    ) {
        let waker = futures::task::noop_waker();
        let mut completed: Vec<DiffJobOutput> = Vec::new();
        self.jobs.retain(|_, job| {
            let mut cx = Context::from_waker(&waker);
            match Pin::new(&mut job.task).poll(&mut cx) {
                Poll::Ready(out) => {
                    completed.push(out);
                    false
                },
                Poll::Pending => true,
            }
        });
        for out in completed {
            if let Some(counts) = out.repo_change_counts {
                self.repo_change_counts = Some(counts);
            }
            if let Some(base) = out.base {
                self.base_text.insert(out.path, base);
            }
            if let Some(shared) = buffers.get(out.buffer_id) {
                let mut guard = shared.write().expect("buffer poisoned");
                // A recompute landing on the same hunks is not news. Every
                // decoration consumer keys off the map's version, and the
                // minimap's edge sweep re-derives the whole file from it, so
                // installing an identical map costs that for nothing. A write
                // under .git re-diffs every visible buffer against content that
                // did not move, which is where this earns its keep.
                let same = match (&guard.diff_map, &out.diff_map) {
                    (Some(installed), Some(fresh)) => installed.renders_same_as(fresh),
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    guard.diff_map = out.diff_map;
                }
            }
            // Recorded either way, so an unchanged result still counts as
            // diffed and the buffer is not tried again next frame.
            self.versions.insert(out.buffer_id, out.target_version);
        }

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
                .jobs
                .get(&buffer_id)
                .is_some_and(|job| job.target_version == cur_version)
            {
                continue;
            }
            if !self.settled(executor, redraw_notify, buffer_id, cur_version) {
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
            let task = executor.spawn_blocking({
                let git_host = git_host.clone();
                let git_root = git_root.to_path_buf();
                let redraw = redraw_notify.clone();
                let syntax_styles = syntax_styles.clone();
                let base_cache = base_cache.clone();
                let path = path.clone();
                let base_override = self.base_override.clone();
                let tree_cache = self.tree_cache.clone();
                move || {
                    // Materialize the rope only now that the diff is confirmed
                    // stale and a job is committed, off the event-loop thread.
                    let buffer_text = buffer_snapshot.visible_text.to_string();
                    let computed = compute_diff_map(
                        &*git_host,
                        &git_root,
                        &path,
                        &buffer_text,
                        language.as_ref(),
                        &syntax_styles,
                        &base_cache,
                        cached_base,
                        base_override.as_ref(),
                        Some(&tree_cache),
                    )
                    .map(|(mut diff_map, base)| {
                        diff_map.anchor_hunks(&buffer_snapshot);
                        (diff_map, base)
                    });
                    let repo_change_counts = git_host
                        .discover(&git_root)
                        .map(|repo| repo.change_counts());
                    redraw.notify_one();
                    let (diff_map, base) = match computed {
                        Some((diff_map, base)) => (Some(diff_map), Some(base)),
                        None => (None, None),
                    };
                    DiffJobOutput {
                        buffer_id,
                        path,
                        target_version: cur_version,
                        diff_map,
                        base,
                        repo_change_counts,
                    }
                }
            });
            self.jobs.insert(
                buffer_id,
                DiffJob {
                    target_version: cur_version,
                    task,
                },
            );
        }
    }
}

pub(super) struct DiffJob {
    pub(super) target_version: u64,
    pub(super) task: Task<DiffJobOutput>,
}

pub(super) struct DiffJobOutput {
    pub(super) buffer_id: BufferId,
    /// The file the job diffed, so its base text can be filed under the same
    /// key the next job for it will look under.
    pub(super) path: PathBuf,
    pub(super) target_version: u64,
    pub(super) diff_map: Option<DiffMap>,
    /// The blobs the diff ran against, `None` when the repo or the file's HEAD
    /// content could not be read and there was nothing to diff.
    pub(super) base: Option<DiffBaseText>,
    /// The repo-wide tally read while the job held the repo, `None` outside a
    /// repo. Read here rather than on the run loop, since it costs a full
    /// `git status` walk.
    pub(super) repo_change_counts: Option<(usize, usize)>,
}

/// A file's HEAD and index blobs as git last reported them.
///
/// Neither can change without a write under `.git`, which is watched, so a
/// diff recomputed for a keystroke can reuse what the last one read rather than
/// taking the repo mutex and decompressing the same bytes again.
#[derive(Clone)]
pub(super) struct DiffBaseText {
    head: Arc<String>,
    index: Arc<String>,
    /// Fingerprints of the two blobs, taken once here.
    ///
    /// The index is usually a blob of its own that happens to hold HEAD's
    /// bytes, and every recompute asks whether the two agree so it can reuse
    /// one diff for both. Reading them to answer that costs a pass over the
    /// file per settle, where a blob only changes when something writes under
    /// `.git`.
    head_hash: [u8; 32],
    index_hash: [u8; 32],
}

/// Memoized diff-view base-text work, shared across the blocking jobs that
/// build diff maps.
///
/// Two layers, because they go stale on different inputs. Both grow without
/// bound, which is what an editor session's finite set of base texts, languages,
/// and themes makes acceptable.
#[derive(Default)]
pub(crate) struct BaseHighlightMemo {
    /// Tree-sitter highlight spans for a base text, keyed by its content hash
    /// and language name, so an unchanged base is parsed once across edits.
    /// Theme-independent.
    parses: HashMap<(ContentHash, String), Arc<Vec<HighlightSpan>>>,
    /// Those spans resolved to styles and split per base line. Keyed
    /// additionally by the [`SyntaxStyles`] generation, since the resolution is
    /// what the theme changes.
    buckets: HashMap<(ContentHash, String, u64), Arc<BaseHighlights>>,
}

pub(crate) type BaseHighlightCache = Arc<Mutex<BaseHighlightMemo>>;

/// Compute a buffer's [`DiffMap`] against `base_override`, or against the
/// working tree's own HEAD-plus-index without one.
///
/// [`None`] when the file is outside a repo, or when an unoverridden base has
/// no HEAD content to diff against.
///
/// Both `discover` and `head_content` do git and filesystem IO, so this must
/// run on a blocking thread.
///
/// Two differs answer two questions. The language-agnostic line pass owns the
/// extents and the staged marks, matching [`changed_byte_ranges`], so the
/// gutter and staging read line boundaries. With a `language` and a
/// `tree_memo`, a tree-sitter pass then owns the token detail, so a reindented
/// block washes what it actually changed rather than every line's prefix.
/// Without either, or when the tree pass falls back to lines, the line pass's
/// char-refined spans stand.
#[allow(clippy::too_many_arguments)]
pub(super) fn compute_diff_map(
    git: &dyn GitHost,
    git_root: &Path,
    path: &Path,
    buffer_text: &str,
    language: Option<&Arc<Language>>,
    syntax_styles: &SyntaxStyles,
    base_cache: &BaseHighlightCache,
    cached_base: Option<DiffBaseText>,
    base_override: Option<&DiffBase>,
    tree_memo: Option<&TreeCache>,
) -> Option<(DiffMap, DiffBaseText)> {
    // Reading the blobs is what costs. It takes the repo mutex and then
    // decompresses bytes that a keystroke cannot have changed. The pair is
    // handed back either way so the caller can file it for the next one.
    let base = match cached_base {
        Some(base) => base,
        None => resolve_base(git, git_root, path, base_override)?,
    };
    let base_text = &*base.head;
    let index_text = &*base.index;

    let result = structural_diff::diff(base_text, buffer_text);

    // Which buffer lines the index and the buffer disagree on, which is what
    // marks a hunk staged. With nothing staged the index holds HEAD's bytes, so
    // that question has the same answer as the diff just run, and converting
    // one result twice beats diffing the file twice.
    let index_changed: Vec<Range<u32>> = {
        let hunks = if base.index_hash == base.head_hash {
            changes_to_hunks(&result.changes, base_text, buffer_text)
        } else {
            let index_result = structural_diff::diff(index_text, buffer_text);
            changes_to_hunks(&index_result.changes, index_text, buffer_text)
        };
        hunks
            .into_iter()
            .map(|hunk| hunk.buffer_line_range)
            .collect()
    };

    // A whole-file removal is one change, not one per item the differ found
    // inside it. Left to the structural pass, a file holding two functions
    // tallies as two removals in the statusline.
    let mut diff_map = if buffer_removed(buffer_text) && !base_text.is_empty() {
        let anchor = 0..0;
        // The removal is staged once the index side is gone too. The line
        // differ answers this wrong. Against an empty index it reads the
        // removed buffer's lone newline as an added line.
        let unstaged_lines = match buffer_removed(index_text) {
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
            Some(base.head.clone()),
        )
    } else {
        let mut hunks = changes_to_hunks(&result.changes, base_text, buffer_text);
        mark_staged(&mut hunks, &index_changed);

        // The tree pass answers only what the washes should mark. A parse
        // failure or a fall back to lines leaves the line pass's char-refined
        // spans in place, which is what the view showed before this ran.
        if let Some(language) = language
            && let Some(tree) = structural_diff::diff_with_language_cancellable(
                language,
                base_text,
                buffer_text,
                None,
                tree_memo,
            )
            && !tree.fell_back_to_line_diff
        {
            merge_structural_detail(&mut hunks, &tree.changes, buffer_text);
        }
        DiffMap::from_hunks(hunks, Some(base.head.clone()))
    };
    if let Some(language) = language {
        diff_map.set_base_highlights(compute_base_highlights(
            base_text,
            language,
            syntax_styles,
            base_cache,
        ));
    }
    Some((diff_map, base.clone()))
}

/// Read the two blobs a diff measures `path` against, per the workspace's base.
///
/// The head side is what the diff hunks describe. The index side is what marks
/// them staged, being the content a hunk is already applied to.
///
/// Under a [`DiffBase::Rev`] the rev is the review base and HEAD is the commit
/// checked out over it, so a hunk already in that commit reads staged and a
/// worktree-only edit reads unstaged. Either side missing the file reads as
/// empty text rather than abandoning the diff, so a file the commit adds still
/// diffs against nothing.
///
/// Under a [`DiffBase::Memory`] both sides are the supplied text, which leaves
/// every hunk unstaged. Nothing has applied an agent's proposal anywhere.
fn resolve_base(
    git: &dyn GitHost,
    git_root: &Path,
    path: &Path,
    base_override: Option<&DiffBase>,
) -> Option<DiffBaseText> {
    let repo = git.discover(git_root)?;

    let (head, index) = match base_override {
        Some(DiffBase::Rev { sha }) => {
            let rev = match sha {
                Some(sha) => repo.content_at(sha, path).unwrap_or_default(),
                None => String::new(),
            };
            (
                Arc::new(rev),
                Arc::new(repo.head_content(path).unwrap_or_default()),
            )
        },
        Some(DiffBase::Memory { files }) => match files.get(path) {
            Some(text) => (text.clone(), text.clone()),
            None => working_tree_base(&*repo, path)?,
        },
        None => working_tree_base(&*repo, path)?,
    };

    let head_hash = buffer_registry::fingerprint_bytes(&head);
    let index_hash = buffer_registry::fingerprint_bytes(&index);
    Some(DiffBaseText {
        head,
        index,
        head_hash,
        index_hash,
    })
}

/// The working tree's own base for `path`, as HEAD and the index.
///
/// [`None`] when HEAD does not carry the file, which is what leaves an
/// untracked buffer without a diff map.
fn working_tree_base(repo: &dyn GitRepo, path: &Path) -> Option<(Arc<String>, Arc<String>)> {
    let head = Arc::new(repo.head_content(path)?);
    // A tracked file with no index entry is staged for deletion, so its index
    // side is empty text. HEAD's bytes there mark the removal unstaged.
    let index = match repo.index_content(path) {
        Some(index) => Arc::new(index),
        None => Arc::new(String::new()),
    };
    Some((head, index))
}

/// Whether `buffer_text` stands for a file that is no longer in the working
/// tree.
///
/// A removed file still opens. The read misses and the open path substitutes a
/// lone newline, so the buffer holds `"\n"` rather than nothing. A test for an
/// empty string alone therefore misses every real removal and catches only a
/// file that exists holding zero bytes.
fn buffer_removed(buffer_text: &str) -> bool {
    buffer_text.is_empty() || buffer_text == "\n"
}

/// Highlight `base_text` for the diff view's left column, per base line.
///
/// Memoized against `cache` twice over. An unchanged base text is parsed once,
/// and its spans are resolved and bucketed once per theme, so a keystroke burst
/// that leaves the base alone costs two hash lookups rather than a walk over
/// every span with a style clone per line it touches.
pub(crate) fn compute_base_highlights(
    base_text: &str,
    language: &Arc<Language>,
    syntax_styles: &SyntaxStyles,
    cache: &BaseHighlightCache,
) -> Arc<BaseHighlights> {
    let content: ContentHash = blake3::hash(base_text.as_bytes()).into();
    let name = language.name.to_string();
    let bucket_key = (content, name.clone(), syntax_styles.generation);

    let parse_key = (content, name);
    let hit = {
        let guard = cache.lock().expect("base highlight cache poisoned");
        if let Some(bucketed) = guard.buckets.get(&bucket_key) {
            return bucketed.clone();
        }
        guard.parses.get(&parse_key).cloned()
    };

    // Parsed outside the lock, which a miss holds only long enough to look up.
    // A changeset warms one diff job per changed file on the blocking pool, and
    // parsing under the lock queues every one of them behind whichever job got
    // there first.
    //
    // Two jobs missing at once both parse, which is the price. Same content and
    // same language means the same spans, so whichever lands first is kept and
    // the other is dropped.
    let spans = match hit {
        Some(spans) => spans,
        None => {
            let parsed = Arc::new(
                parse(language, base_text, None)
                    .map(|tree| extract_highlights(language, &tree, base_text))
                    .unwrap_or_default(),
            );
            cache
                .lock()
                .expect("base highlight cache poisoned")
                .parses
                .entry(parse_key)
                .or_insert(parsed)
                .clone()
        },
    };

    // Bucketed outside the lock, so one job's O(spans) resolve does not hold up
    // every other buffer's diff.
    let bucketed = Arc::new(bucket_base_highlights(&spans, base_text, syntax_styles));
    cache
        .lock()
        .expect("base highlight cache poisoned")
        .buckets
        .insert(bucket_key, bucketed.clone());
    bucketed
}

/// Resolve highlight spans to styles and bucket them per base line as line-local
/// byte ranges. A span crossing a newline is clipped to each line it touches.
fn bucket_base_highlights(
    spans: &[HighlightSpan],
    base_text: &str,
    syntax_styles: &SyntaxStyles,
) -> BaseHighlights {
    let starts = line_starts(base_text);
    let line_of = |byte: usize| starts.partition_point(|&s| s <= byte).saturating_sub(1);

    let mut per_line: BaseHighlights = vec![Vec::new(); starts.len()];
    for span in spans {
        let Some(style_id) = syntax_styles.id_for_highlight(span.id) else {
            continue;
        };
        let style = syntax_styles.interner[style_id].clone();

        let first = line_of(span.byte_range.start);
        let last = line_of(
            span.byte_range
                .end
                .saturating_sub(1)
                .max(span.byte_range.start),
        );
        for line in first..=last {
            let line_start = starts[line];
            let line_end = starts.get(line + 1).copied().unwrap_or(base_text.len());
            let s = span.byte_range.start.max(line_start) - line_start;
            let e = span.byte_range.end.min(line_end) - line_start;
            if s < e {
                per_line[line].push((s..e, style.clone()));
            }
        }
    }
    per_line
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
        changed_byte_ranges, compute_base_highlights, scan_changed_ranges, BaseHighlightMemo,
        DiffBase, DIFF_SETTLE,
    };
    use crate::{
        buffer::BufferId,
        diff_map::{DiffHunkStatus, DiffMap},
        display_map::syntax_theme::SyntaxStyles,
        host::DiffStatus,
        pane::View,
        review::ReviewFileInput,
        test_harness::TestHarness,
        theme::Theme,
        workspace::Workspace,
    };
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use stoat_language::LanguageRegistry;

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

        let first = compute_base_highlights(base, &language, &styles, &cache);
        let second = compute_base_highlights(base, &language, &styles, &cache);
        assert!(
            Arc::ptr_eq(&first, &second),
            "the same style table serves the bucketed map it already built",
        );

        let rebuilt = SyntaxStyles::from_theme(&Theme::empty());
        let third = compute_base_highlights(base, &language, &rebuilt, &cache);
        assert!(
            !Arc::ptr_eq(&first, &third),
            "a rebuilt style table cannot reuse a resolve made against the old one",
        );
        assert_eq!(
            *first, *third,
            "and the miss is conservative: the same theme resolves the same way",
        );
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
        // Only the two tokens the block itself added. The line differ washes
        // nine spans here, one per rewritten indent.
        assert_eq!(
            spans,
            [(1, 2)],
            "the reindented statements wash nothing, and only `if x` marks"
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
        let (h, buffer_id) = {
            let mut h = TestHarness::with_size(80, 24);
            h.stage_review_scenario("/repo", &[("a.unknownext", "alpha\n", "alPHa\n")]);
            h.stoat.set_diff_warm_auto(true);
            h.open_file(Path::new("/repo/a.unknownext"));
            h.settle_diff_jobs();
            let id = h.stoat.focused_editor_ids().expect("focused editor").1;
            (h, id)
        };
        let spans = detail_spans_of(&h, buffer_id);
        assert!(
            spans.iter().any(|(_, count)| *count > 0),
            "the line pass still refines the changed characters: {spans:?}"
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

    #[test]
    fn diff_job_marks_hunks_staged_from_the_index() {
        let mut h = TestHarness::with_size(80, 24);
        // HEAD a/b/c/d; working changes line 1 (b->B) and line 3 (d->D). The
        // index holds only the line-1 change, so line 1 is staged, line 3 not.
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/f.txt"));
        h.settle_diff_jobs();

        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");

        let flags: Vec<(u32, bool)> = dm
            .hunks_in_range(0..u32::MAX)
            .iter()
            .map(|hunk| (hunk.buffer_start_line, hunk.staged()))
            .collect();
        assert_eq!(
            flags,
            vec![(1, true), (3, false)],
            "the index-staged line-1 hunk is staged, the line-3 hunk is not"
        );
    }

    /// Opening the view used to parse the whole base file on the thread that
    /// handled the key, for colors nothing reads until the first frame after.
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
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
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
        let git_host = h.stoat.git_host.clone();
        let language_registry = h.stoat.language_registry.clone();
        let syntax_styles = h.stoat.syntax_styles.clone();
        let base_cache = h.stoat.base_highlights_cache.clone();
        h.stoat.active_workspace_mut().install_diff_map_now(
            &git_host,
            &language_registry,
            &syntax_styles,
            &base_cache,
            buffer_id,
        );

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
        let git_host = h.stoat.git_host.clone();
        let language_registry = h.stoat.language_registry.clone();
        let syntax_styles = h.stoat.syntax_styles.clone();
        let base_cache = h.stoat.base_highlights_cache.clone();
        h.stoat.active_workspace_mut().install_diff_map_now(
            &git_host,
            &language_registry,
            &syntax_styles,
            &base_cache,
            buffer_id,
        );

        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard.diff_map.clone().expect("diff map populated")
    }

    #[test]
    fn a_rev_base_diffs_against_the_named_commit() {
        let dm = based_harness(
            Some(DiffBase::Rev {
                sha: Some("base1".into()),
            }),
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
            Some(DiffBase::Rev {
                sha: Some("base1".into()),
            }),
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
        let dm = based_harness(Some(DiffBase::Rev { sha: None }), "old\n", "mid\n", "new\n");
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
        let git_host = h.stoat.git_host.clone();
        let language_registry = h.stoat.language_registry.clone();
        let syntax_styles = h.stoat.syntax_styles.clone();
        let base_cache = h.stoat.base_highlights_cache.clone();
        h.stoat.active_workspace_mut().install_diff_map_now(
            &git_host,
            &language_registry,
            &syntax_styles,
            &base_cache,
            buffer_id,
        );
        assert!(
            h.stoat
                .active_workspace()
                .diff
                .holds(buffer_id, Some(Path::new("/repo/a.rs"))),
            "the blobs are cached before the base moves"
        );

        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::Rev { sha: None }));
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
        let git_host = h.stoat.git_host.clone();
        let language_registry = h.stoat.language_registry.clone();
        let syntax_styles = h.stoat.syntax_styles.clone();
        let base_cache = h.stoat.base_highlights_cache.clone();
        h.stoat.active_workspace_mut().install_diff_map_now(
            &git_host,
            &language_registry,
            &syntax_styles,
            &base_cache,
            buffer_id,
        );

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
    fn diff_job_leaves_untracked_buffer_without_a_diff_map() {
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
}
