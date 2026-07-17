mod name;
mod persist;

use crate::{
    agent_status::AgentStatus,
    app::{parse_buffer_async, parse_buffer_step, ParseJobOutput},
    badge::BadgeTray,
    buffer::{BufferId, SharedBuffer},
    buffer_registry::{self, BufferRegistry},
    code_index::{
        build::{file_id, reindex_buffer, IndexUpdate, ReindexTarget},
        nav::TrailState,
    },
    commit_list::CommitListState,
    diff,
    diff_cache::ContentHash,
    diff_map::{line_starts, BaseHighlights, DiffMap},
    display_map::syntax_theme::SyntaxStyles,
    editor_state::{EditorId, EditorState},
    host::{FsHost, GitHost},
    input_history::InputHistory,
    pane::{DockId, DockPanel, DockSide, FocusTarget, PaneTree, View},
    rebase::{ActiveRebase, RebaseState},
    render::layout::split_pane_status,
    review::ReviewFileInput,
    review_session::ReviewSession,
    run::{RunId, RunState},
    term_session::{TermId, TermSession},
};
use codegraph::{CodeGraph, FileId};
pub use persist::find_resume_anchor;
pub(crate) use persist::{anchor_state_dir, list_workspace_files, state_path_for};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use slotmap::{new_key_type, SlotMap};
use std::{
    collections::HashMap,
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::UNIX_EPOCH,
};
use stoat_language::{
    extract_highlights, parse, structural_diff, HighlightSpan, Language, LanguageRegistry,
};
use stoat_scheduler::{Executor, Task};
use stoat_text::{Point, Rope};
use tokio::sync::{mpsc::UnboundedSender, oneshot, Notify};

new_key_type! {
    pub struct WorkspaceId;
}

/// Largest buffer, in bytes, that [`Workspace::drive_parse_jobs`] parses
/// synchronously on the event-loop thread.
///
/// Only the tree-sitter step honors the 1ms deadline. The full reparse and
/// captures walk that follow it are unbounded O(file), so past this cap a
/// buffer is parsed on the background pool instead of blocking a keystroke.
const SYNC_PARSE_MAX_BYTES: usize = 256 * 1024;

/// Stable-across-restart workspace identifier. [`WorkspaceId`] is a SlotMap
/// key whose generation is recycled each run, so it can't serve as an on-disk
/// filename. [`WorkspaceUid`] is assigned once at construction time from the
/// wall clock and serialized with the workspace's persisted state, so a
/// workspace keeps the same filename across sessions. The nanosecond timestamp
/// also gives a natural creation-order sort that complements mtime-based
/// "most recent" selection on load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceUid(pub u64);

impl WorkspaceUid {
    pub(crate) fn now(executor: &Executor) -> Self {
        let nanos = executor
            .system_now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self(nanos)
    }
}

impl std::fmt::Display for WorkspaceUid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// A self-contained editing context: its own buffers, editors, pane layout, and
/// git root. Workspaces are owned by the root [`crate::app::Stoat`]
/// and can run in the background; switching between workspaces is a render-target
/// swap rather than a lifecycle transition.
///
/// **`BufferId` is workspace-scoped.** [`BufferRegistry`] allocates ids from a
/// per-registry counter, so buffer ids from two different workspaces can collide.
/// Never pass a [`BufferId`] outside of its owning workspace.
///
/// **`EditorId` is workspace-scoped.** Each workspace owns its own
/// [`SlotMap<EditorId, EditorState>`], and [`PaneTree`] stores [`EditorId`]s from
/// that specific slotmap via [`View::Editor`]. A pane tree and its editor slotmap
/// must never be split across workspaces.
pub struct Workspace {
    /// Patched by [`crate::app::Stoat`] immediately after slotmap insertion.
    /// Reads between [`Workspace::new`] and that patch see [`WorkspaceId::default`].
    pub id: WorkspaceId,
    /// Stable identifier for this workspace across restarts. Assigned once in
    /// [`Workspace::new`] and preserved by [`crate::workspace::persist`] on
    /// save/load. Doubles as the on-disk filename.
    pub(crate) uid: WorkspaceUid,
    /// User-facing display name. Defaults to a deterministic
    /// adjective+animal pair derived from [`Self::uid`] (see
    /// [`crate::workspace::name::default_workspace_name`]). Empty string opts
    /// the renderer into the `git_root.file_name()` fallback used by tests.
    pub(crate) name: String,
    pub git_root: PathBuf,
    /// The workspace's resolved project environment, loaded from direnv.
    pub(crate) env: crate::project_env::WorkspaceEnv,
    /// Whether the background diff-cache warm has run for this workspace's
    /// current root. Set once by [`crate::diff_warm::ensure_diff_warm`], reset
    /// when the cwd changes so the new root warms afresh.
    pub(crate) diff_warmed: bool,
    /// Persisted name of the finder scope this workspace last closed in, so
    /// `space p` reopens where the user left off. Holds `"all"`, `"modified"`,
    /// or a named-scope key, and is `None` until a finder closes here. Buffers
    /// is never recorded (a dedicated picker, not a sticky mode). Resolved back
    /// to a scope at open time and validated against the current config, so a
    /// name whose scope has since been removed falls back to the default.
    pub(crate) last_finder_scope: Option<String>,
    /// Fish-style recall history of executed command-palette lines, walked by
    /// bare Up/Down in the palette. Persisted per workspace.
    pub(crate) palette_history: InputHistory,
    pub panes: PaneTree,
    pub(crate) docks: SlotMap<DockId, DockPanel>,
    pub(crate) focus: FocusTarget,
    pub(crate) buffers: BufferRegistry,
    pub(crate) editors: SlotMap<EditorId, EditorState>,
    pub(crate) runs: SlotMap<RunId, RunState>,
    pub(crate) terms: SlotMap<TermId, TermSession>,
    /// In-RAM symbol-and-call graph for this workspace, merged from the
    /// per-file shards the cold build and incremental reindex produce.
    pub(crate) code_graph: CodeGraph,
    /// Bumped each time a shard is merged into [`Self::code_graph`], so a
    /// consumer can tell whether the graph changed since it last read it.
    pub(crate) index_generation: u64,
    /// Workspace-relative path for each indexed [`FileId`], so navigation can
    /// recover a symbol's file from its graph id. The graph keys files by a
    /// one-way hash, so this is the only way back to a path.
    pub(crate) file_paths: HashMap<FileId, PathBuf>,
    /// Byte ranges changed against HEAD for each file with a working-tree
    /// diff, in the working-tree text's byte space. Rebuilt by
    /// [`Self::refresh_changed_ranges`] so diff-filtered navigation can ask
    /// whether a symbol's definition overlaps a change.
    pub(crate) changed_ranges: HashMap<FileId, Vec<Range<usize>>>,
    /// Per-file diff memo keyed by [`FileId`], holding the base and buffer
    /// content hashes the ranges were computed from alongside the ranges
    /// themselves. [`Self::refresh_changed_ranges`] reuses an entry whenever
    /// both hashes still match, so repeated diff-filtered navigation over an
    /// unchanged tree recomputes nothing. The memo persists across refreshes,
    /// while [`Self::changed_ranges`] is cleared and rebuilt each call.
    changed_ranges_memo: HashMap<FileId, (ContentHash, ContentHash, Vec<Range<usize>>)>,
    /// Count of memo misses (actual diffs run) in
    /// [`Self::refresh_changed_ranges`], so a test can prove the memo spares
    /// the recompute on an unchanged tree.
    #[cfg(test)]
    changed_ranges_recomputes: u64,
    /// The active call-graph trail, if the user has marked a start. Holds
    /// the marked anchors and, once both ends are set, the cached path that
    /// [`crate::code_index::nav`]'s trail actions step along.
    pub(crate) trail: Option<TrailState>,
    /// Active review session (if any). Owned at the workspace level because
    /// a review spans files and can be viewed by multiple panes in future
    /// multi-pane review flows. Dropped on `CloseReview`.
    pub(crate) review: Option<ReviewSession>,
    /// Active commit-listing state (if any). Parallel to [`Self::review`]:
    /// populated while the user is in `"commits"` mode and dropped on
    /// `CloseCommits`.
    pub(crate) commits: Option<CommitListState>,
    /// Active rebase plan (if any). Populated when the user enters
    /// `"rebase"` mode from the commit list; dropped on abort or after
    /// successful execution.
    pub(crate) rebase: Option<RebaseState>,
    /// In-flight rebase execution state. Present while the stepper is
    /// paused on reword/edit/conflict and during final execution;
    /// dropped when the plan completes or aborts.
    pub(crate) rebase_active: Option<ActiveRebase>,
    parse_jobs: HashMap<BufferId, ParseJob>,
    /// In-flight diff-map population jobs, one per buffer, mirroring
    /// [`Self::parse_jobs`]. Held so the spawned blocking diff is not cancelled
    /// before it installs its [`DiffMap`] on the buffer.
    diff_jobs: HashMap<BufferId, DiffJob>,
    /// Buffer edit version each buffer's `diff_map` was last populated for.
    ///
    /// Records no-repo and untracked buffers too (with a cleared map) so they
    /// are not retried every frame, and drives re-population when a buffer is
    /// edited past the recorded version.
    diff_versions: HashMap<BufferId, u64>,
    /// In-flight live-reindex jobs, one per buffer, held so the spawned
    /// extraction is not cancelled. Replaced when the buffer reparses.
    index_jobs: HashMap<BufferId, Task<()>>,
    pub(crate) badges: BadgeTray,
    /// Status of the owned Claude subshell for this workspace's session, or
    /// `None` until one is spawned. Owned here so the render process reads it
    /// on paint without touching the agent's IPC path. The per-session hook
    /// server drives it via [`AgentStatus::apply`].
    pub(crate) agent: Option<AgentStatus>,
    /// Open temp-file editors an owned agent is blocked on, keyed by the
    /// buffer hosting each one.
    ///
    /// When Claude shells out to `$EDITOR`, the agent socket opens the temp
    /// file as a buffer and parks the connection on a oneshot. The sender
    /// lives here until the buffer or its pane closes, at which point either
    /// close path fires it to unblock the waiting agent. It is not persisted,
    /// because a oneshot cannot outlive the process.
    pub(crate) editor_bridge_waiters: HashMap<BufferId, oneshot::Sender<()>>,
}

struct ParseJob {
    target_version: u64,
    task: Task<Option<ParseJobOutput>>,
}

struct DiffJob {
    target_version: u64,
    task: Task<DiffJobOutput>,
}

struct DiffJobOutput {
    buffer_id: BufferId,
    target_version: u64,
    diff_map: Option<DiffMap>,
}

impl Workspace {
    pub(crate) fn new(git_root: PathBuf, executor: &Executor) -> Self {
        let mut buffers = BufferRegistry::new();
        let (buffer_id, buffer) = buffers.new_scratch();
        let mut editors = SlotMap::with_key();
        let editor_id = editors.insert(EditorState::new(buffer_id, buffer, executor.clone()));
        let mut panes = PaneTree::new(Rect::default());
        let initial_focus = panes.focus();
        panes.pane_mut(initial_focus).view = View::Editor(editor_id);

        let uid = WorkspaceUid::now(executor);
        let name = name::default_workspace_name(uid);

        Self {
            id: WorkspaceId::default(),
            uid,
            name,
            git_root,
            env: crate::project_env::WorkspaceEnv::default(),
            diff_warmed: false,
            last_finder_scope: None,
            palette_history: InputHistory::default(),
            panes,
            docks: SlotMap::with_key(),
            focus: FocusTarget::SplitPane(initial_focus),
            buffers,
            editors,
            runs: SlotMap::with_key(),
            terms: SlotMap::with_key(),
            code_graph: CodeGraph::new(),
            index_generation: 0,
            file_paths: HashMap::new(),
            changed_ranges: HashMap::new(),
            changed_ranges_memo: HashMap::new(),
            #[cfg(test)]
            changed_ranges_recomputes: 0,
            trail: None,
            review: None,
            commits: None,
            rebase: None,
            rebase_active: None,
            parse_jobs: HashMap::new(),
            diff_jobs: HashMap::new(),
            diff_versions: HashMap::new(),
            index_jobs: HashMap::new(),
            badges: BadgeTray::new(),
            agent: None,
            editor_bridge_waiters: HashMap::new(),
        }
    }

    /// Stable identifier for this session across restarts.
    ///
    /// Keys the workspace's on-disk state file and its per-session agent hook
    /// socket, so external tooling addresses a live session by this value.
    pub fn uid(&self) -> WorkspaceUid {
        self.uid
    }

    /// True when this workspace is structurally indistinguishable from the
    /// state produced by [`Self::new`]: one empty scratch buffer, one editor,
    /// one un-split pane, and no auxiliary state (docks, review,
    /// commits, rebase, runs). Used by [`crate::app::Stoat::save_workspace`]
    /// to skip persisting workspaces the user opened but never used, so the
    /// on-disk directory does not fill up with empty session files now that
    /// each launch without `--continue` spawns a fresh workspace.
    pub(crate) fn is_fresh(&self) -> bool {
        self.review.is_none()
            && self.commits.is_none()
            && self.rebase.is_none()
            && self.rebase_active.is_none()
            && self.runs.is_empty()
            && self.terms.is_empty()
            && self.docks.is_empty()
            && self.editors.len() == 1
            && self.panes.split_panes().count() == 1
            && self.buffers.only_empty_scratch()
    }

    /// Clear the preview buffer's syntax and cancel any in-flight parse for it.
    ///
    /// The file finder reuses one preview buffer id for every file it shows, so
    /// an unfinished parse of the previously-previewed file would otherwise
    /// complete and paint its anchored tokens onto the swapped-in content.
    /// Removing the parse job drops its task, which cancels the parse, so the
    /// stale result is never applied.
    pub(crate) fn reset_preview_syntax(&mut self, id: BufferId) {
        self.buffers.clear_syntax(id);
        self.parse_jobs.remove(&id);
        self.diff_jobs.remove(&id);
        self.diff_versions.remove(&id);
    }

    /// Force the next [`Self::drive_diff_jobs`] pass to recompute `id`'s diff
    /// map by dropping its recorded version and any in-flight job.
    ///
    /// Used after a git-index mutation so the buffer re-diffs. The recompute
    /// stays HEAD-relative, so the hunks are unchanged until the base becomes
    /// index-aware.
    pub(crate) fn invalidate_diff(&mut self, id: BufferId) {
        self.diff_jobs.remove(&id);
        self.diff_versions.remove(&id);
    }

    /// Compute and install `id`'s diff map synchronously, bypassing the
    /// background job so its hunks are available on the current turn.
    ///
    /// Records the buffer's version so [`Self::drive_diff_jobs`] does not
    /// redundantly recompute the same map. A no-op for a buffer without a path.
    pub(crate) fn install_diff_map_now(
        &mut self,
        git_host: &Arc<dyn GitHost>,
        language_registry: &Arc<LanguageRegistry>,
        syntax_styles: &SyntaxStyles,
        base_cache: &BaseHighlightCache,
        id: BufferId,
    ) {
        let Some(path) = self.buffers.path_for(id).map(Path::to_path_buf) else {
            return;
        };
        let Some(shared) = self.buffers.get(id) else {
            return;
        };
        let (version, text) = {
            let guard = shared.read().expect("buffer poisoned");
            (
                guard.snapshot.version,
                guard.snapshot.visible_text.to_string(),
            )
        };

        let language = language_registry.for_path(&path);
        let diff_map = compute_diff_map(
            &**git_host,
            &self.git_root,
            &path,
            &text,
            language.as_ref(),
            syntax_styles,
            base_cache,
        );
        if let Some(shared) = self.buffers.get(id) {
            shared.write().expect("buffer poisoned").diff_map = diff_map;
        }
        self.diff_versions.insert(id, version);
    }

    /// Build a fresh [`EditorState`] for `buffer_id`, seeded with the buffer's
    /// retained tree-sitter and LSP tokens when the registry holds them.
    ///
    /// A re-shown buffer therefore paints styled on its first frame. Without the
    /// seed the fresh editor starts with empty highlight caches, and
    /// [`Self::drive_parse_jobs`] skips a version-current buffer, so it would
    /// otherwise stay unstyled until the next edit forces a reparse. The LSP
    /// tokens are seeded only while their cached version still matches the
    /// buffer, since a stale set would misplace the highlights.
    pub(crate) fn seeded_editor(
        &self,
        buffer_id: BufferId,
        buffer: SharedBuffer,
        executor: Executor,
    ) -> EditorState {
        let current_version = buffer.read().expect("buffer poisoned").snapshot.version;
        let mut editor = EditorState::new(buffer_id, buffer, executor);
        if let Some((tokens, interner)) = self.buffers.tokens_for(buffer_id) {
            editor
                .display_map
                .set_semantic_token_highlights(buffer_id, tokens, interner);
        }
        if let Some((version, tokens, interner)) = self.buffers.lsp_tokens_for(buffer_id)
            && version == current_version
        {
            editor
                .display_map
                .set_lsp_token_highlights(buffer_id, tokens, interner);
        }
        editor
    }

    /// Drive background parse jobs: poll any in-flight tasks for completion,
    /// install their results, then spawn new jobs for visible buffers whose
    /// stored syntax version is stale.
    ///
    /// At most one job per buffer is in flight at a time. If a buffer advances
    /// past the in-flight job's `target_version`, the new job is queued only
    /// after the old one completes. Anchors in the result are computed using
    /// the parsed snapshot, so they remain valid even if the buffer has been
    /// edited further while the parse was running.
    pub(crate) fn drive_parse_jobs(
        &mut self,
        executor: &Executor,
        syntax_styles: &SyntaxStyles,
        redraw_notify: &Arc<Notify>,
        index_update_tx: &UnboundedSender<IndexUpdate>,
        retention: usize,
    ) {
        let waker = futures::task::noop_waker();
        let mut completed: Vec<ParseJobOutput> = Vec::new();
        self.parse_jobs.retain(|_, job| {
            let mut cx = Context::from_waker(&waker);
            match Pin::new(&mut job.task).poll(&mut cx) {
                Poll::Ready(Some(out)) => {
                    completed.push(out);
                    false
                },
                Poll::Ready(None) => false,
                Poll::Pending => true,
            }
        });
        for out in completed {
            self.buffers.store_syntax(out.buffer_id, out.syntax);
            self.buffers.store_syntax_map(out.buffer_id, out.syntax_map);
            self.buffers.store_tokens(
                out.buffer_id,
                out.tokens.clone(),
                syntax_styles.interner.clone(),
            );
            for editor in self.editors.values_mut() {
                if editor.buffer_id == out.buffer_id {
                    editor.display_map.set_semantic_token_highlights(
                        out.buffer_id,
                        out.tokens.clone(),
                        syntax_styles.interner.clone(),
                    );
                }
            }
            let text = self.buffers.get(out.buffer_id).map(|shared| {
                shared
                    .read()
                    .expect("buffer poisoned")
                    .snapshot
                    .visible_text
                    .clone()
            });
            if let Some(text) = text {
                self.enqueue_reindex(
                    executor,
                    index_update_tx,
                    redraw_notify,
                    out.buffer_id,
                    text,
                );
            }
        }

        let visible = self.visible_buffer_ids();

        for &buffer_id in &visible {
            let Some(lang) = self.buffers.language_for(buffer_id) else {
                continue;
            };
            let Some(shared) = self.buffers.get(buffer_id) else {
                continue;
            };
            let snapshot = {
                let guard = shared.read().expect("buffer poisoned");
                guard.snapshot.clone()
            };
            let cur_version = snapshot.version;

            if self.buffers.syntax_version(buffer_id) == Some(cur_version) {
                continue;
            }
            if let Some(job) = self.parse_jobs.get(&buffer_id) {
                if job.target_version == cur_version {
                    continue;
                }
                continue;
            }

            let mut prior = self.buffers.take_syntax(buffer_id);
            let mut prior_map = self.buffers.take_syntax_map(buffer_id);

            // Only the tree-sitter step honors the deadline. The full reparse
            // and captures walk that follow it are unbounded O(file), so a
            // large buffer skips the synchronous fast path and parses on the
            // background pool instead of blocking the keystroke.
            let sync_out = (snapshot.len() <= SYNC_PARSE_MAX_BYTES)
                .then(|| {
                    let deadline = executor.now() + std::time::Duration::from_millis(1);
                    parse_buffer_step(
                        buffer_id,
                        snapshot.clone(),
                        &lang,
                        &mut prior,
                        &mut prior_map,
                        syntax_styles,
                        Some((deadline, executor)),
                    )
                })
                .flatten();
            if let Some(out) = sync_out {
                self.buffers.store_syntax(out.buffer_id, out.syntax);
                self.buffers.store_syntax_map(out.buffer_id, out.syntax_map);
                self.buffers.store_tokens(
                    out.buffer_id,
                    out.tokens.clone(),
                    syntax_styles.interner.clone(),
                );
                for editor in self.editors.values_mut() {
                    if editor.buffer_id == out.buffer_id {
                        editor.display_map.set_semantic_token_highlights(
                            out.buffer_id,
                            out.tokens.clone(),
                            syntax_styles.interner.clone(),
                        );
                    }
                }
                self.enqueue_reindex(
                    executor,
                    index_update_tx,
                    redraw_notify,
                    buffer_id,
                    snapshot.visible_text.clone(),
                );
                continue;
            }

            let styles = syntax_styles.clone();
            let task = executor.spawn_with_redraw(
                redraw_notify.clone(),
                parse_buffer_async(buffer_id, snapshot, lang, prior, prior_map, styles),
            );
            self.parse_jobs.insert(
                buffer_id,
                ParseJob {
                    target_version: cur_version,
                    task,
                },
            );
        }

        // Cap retained highlight state. In-flight parse ids join the visible
        // set so a completing job cannot repopulate a just-evicted buffer.
        let mut protected = visible;
        protected.extend(self.parse_jobs.keys().copied());
        let evicted = self.buffers.evict_hidden_highlights(&protected, retention);
        if !evicted.is_empty() {
            tracing::debug!(
                target: "stoat::app",
                evicted = evicted.len(),
                cap = retention,
                "evicted hidden highlight state"
            );
        }
    }

    /// Buffer ids currently shown in a split-pane editor or held as a preview,
    /// deduplicated. Drives which buffers the background parse and diff jobs keep
    /// current.
    fn visible_buffer_ids(&self) -> Vec<BufferId> {
        let mut visible: Vec<BufferId> = Vec::new();
        for (_, pane) in self.panes.split_panes() {
            match pane.view {
                View::Editor(editor_id) => {
                    if let Some(editor) = self.editors.get(editor_id)
                        && !visible.contains(&editor.buffer_id)
                    {
                        visible.push(editor.buffer_id);
                    }
                },
                View::Label(_) | View::Run(_) | View::Agent(_) | View::Terminal(_) => {},
            }
        }
        for id in self.buffers.preview_buffer_ids() {
            if !visible.contains(&id) {
                visible.push(id);
            }
        }
        visible
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
    pub(crate) fn drive_diff_jobs(
        &mut self,
        executor: &Executor,
        git_host: &Arc<dyn GitHost>,
        language_registry: &Arc<LanguageRegistry>,
        syntax_styles: &SyntaxStyles,
        base_cache: &BaseHighlightCache,
        redraw_notify: &Arc<Notify>,
    ) {
        let waker = futures::task::noop_waker();
        let mut completed: Vec<DiffJobOutput> = Vec::new();
        self.diff_jobs.retain(|_, job| {
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
            if let Some(shared) = self.buffers.get(out.buffer_id) {
                shared.write().expect("buffer poisoned").diff_map = out.diff_map;
            }
            self.diff_versions.insert(out.buffer_id, out.target_version);
        }

        let git_root = self.git_root.clone();
        for buffer_id in self.visible_buffer_ids() {
            let Some(path) = self.buffers.path_for(buffer_id).map(Path::to_path_buf) else {
                continue;
            };
            let Some(shared) = self.buffers.get(buffer_id) else {
                continue;
            };
            let (cur_version, buffer_rope) = {
                let guard = shared.read().expect("buffer poisoned");
                (guard.snapshot.version, guard.snapshot.visible_text.clone())
            };

            if self.diff_versions.get(&buffer_id) == Some(&cur_version) {
                continue;
            }
            if self
                .diff_jobs
                .get(&buffer_id)
                .is_some_and(|job| job.target_version == cur_version)
            {
                continue;
            }

            let language = language_registry.for_path(&path);
            let task = executor.spawn_blocking({
                let git_host = git_host.clone();
                let git_root = git_root.clone();
                let redraw = redraw_notify.clone();
                let syntax_styles = syntax_styles.clone();
                let base_cache = base_cache.clone();
                move || {
                    // Materialize the rope only now that the diff is confirmed
                    // stale and a job is committed, off the event-loop thread.
                    let buffer_text = buffer_rope.to_string();
                    let diff_map = compute_diff_map(
                        &*git_host,
                        &git_root,
                        &path,
                        &buffer_text,
                        language.as_ref(),
                        &syntax_styles,
                        &base_cache,
                    );
                    redraw.notify_one();
                    DiffJobOutput {
                        buffer_id,
                        target_version: cur_version,
                        diff_map,
                    }
                }
            });
            self.diff_jobs.insert(
                buffer_id,
                DiffJob {
                    target_version: cur_version,
                    task,
                },
            );
        }
    }

    /// Detect and assign a language to every path-bearing buffer that
    /// lacks one, resolving the path's extension through `registry`.
    ///
    /// Session restore (via [`Self::restore_state`]) rebuilds buffers
    /// with no language, and the parse pipeline only highlights buffers
    /// that have one, so this runs once after a restore to re-detect
    /// them. Idempotent and safe to call unconditionally -- buffers that
    /// already have a language are left untouched, so buffers opened
    /// during the session are unaffected.
    pub(crate) fn assign_languages_from_paths(&mut self, registry: &LanguageRegistry) {
        for (id, path) in self.buffers.buffers_needing_language() {
            if let Some(lang) = registry.for_path(&path) {
                self.buffers.set_language(id, lang);
            }
        }
    }

    /// Spawn a live re-index of `buffer_id` from its current `text`.
    ///
    /// Skips buffers with no file path or no resolved language. The spawned
    /// job is stored so it is not cancelled, replacing any prior one for the
    /// buffer.
    fn enqueue_reindex(
        &mut self,
        executor: &Executor,
        index_update_tx: &UnboundedSender<IndexUpdate>,
        redraw_notify: &Arc<Notify>,
        buffer_id: BufferId,
        text: Rope,
    ) {
        let Some(path) = self.buffers.path_for(buffer_id).map(|p| p.to_path_buf()) else {
            return;
        };
        let Some(language) = self.buffers.language_for(buffer_id) else {
            return;
        };
        let target = ReindexTarget {
            git_root: self.git_root.clone(),
            workspace: self.id,
            language,
            path,
            text,
        };
        let task = reindex_buffer(
            executor,
            index_update_tx.clone(),
            redraw_notify.clone(),
            target,
        );
        self.index_jobs.insert(buffer_id, task);
    }

    /// Rebuild [`Self::changed_ranges`] from the working tree.
    ///
    /// Scans the changed files, diffs each against HEAD, and records the
    /// byte ranges its hunks cover in the working-tree text, keyed by the
    /// graph [`FileId`]. Clears prior state, so an empty map means no
    /// working-tree diff.
    pub(crate) fn refresh_changed_ranges(
        &mut self,
        git: &dyn GitHost,
        fs: &dyn FsHost,
        langs: &LanguageRegistry,
    ) {
        self.changed_ranges.clear();
        let Some((_workdir, inputs)) = diff::scan_working_tree(git, fs, langs, &self.git_root)
        else {
            return;
        };
        for input in &inputs {
            let fid = file_id(&input.rel_path);
            let base_hash = buffer_registry::fingerprint_bytes(input.base_text.as_str());
            let buffer_hash = buffer_registry::fingerprint_bytes(input.buffer_text.as_str());

            let ranges = match self.changed_ranges_memo.get(&fid) {
                Some((cached_base, cached_buffer, cached))
                    if *cached_base == base_hash && *cached_buffer == buffer_hash =>
                {
                    cached.clone()
                },
                _ => {
                    let computed = changed_byte_ranges(input);
                    #[cfg(test)]
                    {
                        self.changed_ranges_recomputes += 1;
                    }
                    self.changed_ranges_memo
                        .insert(fid, (base_hash, buffer_hash, computed.clone()));
                    computed
                },
            };

            if !ranges.is_empty() {
                self.changed_ranges.insert(fid, ranges);
            }
        }
    }

    pub(crate) fn layout(&mut self, total_area: Rect) {
        self.panes.resize(total_area);

        // Inset the dock vertically so it reads as an edge-attached popover rather
        // than a full-height pane. One row of breathing space top and bottom puts
        // the dock at ~95% of the workspace height on typical terminals.
        let vertical_margin: u16 = 1;
        let dock_y = total_area.y + vertical_margin;
        let dock_height = total_area
            .height
            .saturating_sub(vertical_margin.saturating_mul(2));

        for dock in self.docks.values_mut() {
            let width = dock.effective_width().min(total_area.width);
            if width == 0 || dock_height == 0 {
                dock.area = Rect::default();
                continue;
            }
            let x = match dock.side {
                DockSide::Left => total_area.x,
                DockSide::Right => total_area.x + total_area.width - width,
            };
            dock.area = Rect::new(x, dock_y, width, dock_height);
        }

        self.fit_terms_to_panes();
    }

    /// Resize every hosted agent's emulator and PTY to its pane's content area,
    /// so an agent reflows whenever the layout that frames it changes.
    ///
    /// Runs on every [`Self::layout`], but [`TermSession::fit`] skips agents
    /// already at the right size, so a steady layout issues no PTY resizes. The
    /// content area excludes the status row via [`split_pane_status`], matching
    /// the rectangle the renderer composites the emulator into.
    fn fit_terms_to_panes(&mut self) {
        let targets: Vec<(TermId, u16, u16)> = self
            .panes
            .split_panes()
            .filter_map(|(_, pane)| match pane.view {
                View::Agent(id) | View::Terminal(id) => {
                    let (content, _) = split_pane_status(pane.area);
                    Some((id, content.height, content.width))
                },
                _ => None,
            })
            .collect();

        for (id, rows, cols) in targets {
            if let Some(agent) = self.terms.get_mut(id) {
                agent.fit(rows, cols);
            }
        }
    }
}

/// Compute a buffer's HEAD-vs-worktree [`DiffMap`], or [`None`] when the file
/// is outside a repo or has no HEAD content to diff against.
///
/// Both `discover` and `head_content` do git and filesystem IO, so this must
/// run on a blocking thread. Uses the language-agnostic line diff, matching
/// [`changed_byte_ranges`].
/// Memoized tree-sitter parses of base texts for the diff view's left column,
/// keyed by base content hash and language name so an unchanged base is parsed
/// once across edits. Values are theme-independent, so styles resolve per build.
pub(crate) type BaseHighlightCache =
    Arc<Mutex<HashMap<(ContentHash, String), Arc<Vec<HighlightSpan>>>>>;

fn compute_diff_map(
    git: &dyn GitHost,
    git_root: &Path,
    path: &Path,
    buffer_text: &str,
    language: Option<&Arc<Language>>,
    syntax_styles: &SyntaxStyles,
    base_cache: &BaseHighlightCache,
) -> Option<DiffMap> {
    let repo = git.discover(git_root)?;
    let base_text = repo.head_content(path)?;

    let index_text = repo
        .index_content(path)
        .unwrap_or_else(|| base_text.clone());
    let index_changed: Vec<Range<u32>> = {
        let index_result = structural_diff::diff(&index_text, buffer_text);
        let index_map = DiffMap::from_structural_changes(index_result, &index_text, buffer_text);
        index_map
            .hunks_in_range(0..u32::MAX)
            .into_iter()
            .map(|hunk| hunk.buffer_line_range.clone())
            .collect()
    };

    let result = structural_diff::diff(&base_text, buffer_text);
    let mut diff_map =
        DiffMap::from_structural_changes_staged(result, &base_text, buffer_text, &index_changed);
    if let Some(language) = language {
        diff_map.set_base_highlights(compute_base_highlights(
            &base_text,
            language,
            syntax_styles,
            base_cache,
        ));
    }
    Some(diff_map)
}

/// Highlight `base_text` for the diff view's left column, memoizing the parse in
/// `cache`. Styles resolve against the current `syntax_styles` on every call so
/// a theme change still takes effect on the next build.
fn compute_base_highlights(
    base_text: &str,
    language: &Arc<Language>,
    syntax_styles: &SyntaxStyles,
    cache: &BaseHighlightCache,
) -> Arc<BaseHighlights> {
    let key = (
        blake3::hash(base_text.as_bytes()).into(),
        language.name.to_string(),
    );
    let spans = {
        let mut guard = cache.lock().expect("base highlight cache poisoned");
        guard
            .entry(key)
            .or_insert_with(|| {
                let spans = parse(language, base_text, None)
                    .map(|tree| extract_highlights(language, &tree, base_text))
                    .unwrap_or_default();
                Arc::new(spans)
            })
            .clone()
    };
    Arc::new(bucket_base_highlights(&spans, base_text, syntax_styles))
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
/// Uses the line diff rather than the language-aware structural diff. The only
/// consumer tests whole-line overlap, and treating moved code as a delete plus
/// an add yields the same or a strictly larger changed set for that test, at a
/// fraction of the cost.
fn changed_byte_ranges(input: &ReviewFileInput) -> Vec<Range<usize>> {
    let result = structural_diff::diff(&input.base_text, &input.buffer_text);
    let diff_map = DiffMap::from_structural_changes(result, &input.base_text, &input.buffer_text);
    let rope = Rope::from(input.buffer_text.as_str());
    diff_map
        .hunks_in_range(0..u32::MAX)
        .into_iter()
        .map(|hunk| {
            let lines = &hunk.buffer_line_range;
            let start = rope.point_to_offset(Point {
                row: lines.start,
                column: 0,
            });
            let end = rope.point_to_offset(Point {
                row: lines.end,
                column: 0,
            });
            start..end
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{changed_byte_ranges, ParseJob, Workspace, SYNC_PARSE_MAX_BYTES};
    use crate::{host::DiffStatus, pane::View, review::ReviewFileInput, test_harness::TestHarness};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_language::LanguageRegistry;
    use stoat_scheduler::{Task, TestScheduler};

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
            h.stoat.active_workspace().diff_jobs.is_empty(),
            "a drive over an already-diffed buffer spawns no new job",
        );
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
            .map(|hunk| (hunk.buffer_start_line, hunk.staged))
            .collect();
        assert_eq!(
            flags,
            vec![(1, true), (3, false)],
            "the index-staged line-1 hunk is staged, the line-3 hunk is not"
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

    #[test]
    fn refresh_changed_ranges_memoizes_across_unchanged_refreshes() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n")],
        );

        let git = h.stoat.git_host.clone();
        let fs = h.stoat.fs_host.clone();
        let langs = h.stoat.language_registry.clone();
        let ws = h.stoat.active_workspace_mut();

        ws.refresh_changed_ranges(git.as_ref(), fs.as_ref(), &langs);
        assert_eq!(
            ws.changed_ranges_recomputes, 1,
            "the first refresh diffs the changed file once"
        );
        assert!(
            !ws.changed_ranges.is_empty(),
            "the working-tree change is recorded"
        );

        ws.refresh_changed_ranges(git.as_ref(), fs.as_ref(), &langs);
        assert_eq!(
            ws.changed_ranges_recomputes, 1,
            "a second refresh over the unchanged tree reuses the memo, no re-diff"
        );
        assert!(
            !ws.changed_ranges.is_empty(),
            "the recorded change survives the memo hit"
        );
    }

    #[test]
    fn assign_languages_from_paths_detects_rust() {
        let executor = Arc::new(TestScheduler::new()).executor();
        let mut ws = Workspace::new(PathBuf::new(), &executor);
        let (id, _) = ws.buffers.open(Path::new("/repo/foo.rs"), "fn main() {}");

        assert_eq!(ws.buffers.language_for(id).map(|l| l.name), None);

        ws.assign_languages_from_paths(&LanguageRegistry::standard());

        assert_eq!(ws.buffers.language_for(id).map(|l| l.name), Some("rust"));
    }

    #[test]
    fn reset_preview_syntax_cancels_in_flight_parse() {
        let executor = Arc::new(TestScheduler::new()).executor();
        let mut ws = Workspace::new(PathBuf::new(), &executor);
        let (id, _) = ws.buffers.new_scratch_preview();
        ws.parse_jobs.insert(
            id,
            ParseJob {
                target_version: 1,
                task: Task::Ready(None),
            },
        );

        ws.reset_preview_syntax(id);

        assert!(
            !ws.parse_jobs.contains_key(&id),
            "swapping preview content drops the prior file's parse job"
        );
    }

    #[test]
    fn oversized_buffer_parses_off_the_main_thread() {
        use crate::action_handlers::dispatch;
        use stoat_action::OpenFile;

        let mut h = TestHarness::with_size(24, 4);
        let root = PathBuf::from("/big");
        let big: String = "fn f() {}\n".repeat(SYNC_PARSE_MAX_BYTES / 10 + 100);
        h.fake_fs().insert_file(root.join("big.rs"), big.as_bytes());
        h.stoat.active_workspace_mut().git_root = root.clone();

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("big.rs"),
            },
        );
        // Drive parse jobs once without ticking the scheduler, so the spawned
        // background job stays pending and observable.
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        let id = ws
            .buffers
            .id_for_path(&root.join("big.rs"))
            .expect("the big buffer opened");
        assert!(
            ws.parse_jobs.contains_key(&id),
            "a buffer past the sync-parse cap spawns a background parse job"
        );
        assert!(
            ws.buffers.syntax_version(id).is_none(),
            "and is not parsed inline on the main thread"
        );
    }

    #[test]
    fn highlight_retention_evicts_least_recently_shown() {
        use crate::action_handlers::dispatch;
        use stoat_action::OpenFile;

        let mut h = TestHarness::with_size(24, 4);
        h.stoat.settings.highlight_retention = Some(1);
        let root = PathBuf::from("/retention");
        h.stoat.active_workspace_mut().git_root = root.clone();
        for name in ["a.rs", "b.rs", "c.rs"] {
            h.fake_fs().insert_file(root.join(name), b"fn f() {}\n");
        }

        // Open and render each so its syntax parses. The last render also runs
        // the eviction that caps retention once a and b are both hidden.
        for name in ["a.rs", "b.rs", "c.rs"] {
            dispatch(
                &mut h.stoat,
                &OpenFile {
                    path: root.join(name),
                },
            );
            h.snapshot();
        }

        let ws = h.stoat.active_workspace();
        let syntax_of = |name: &str| {
            ws.buffers
                .id_for_path(&root.join(name))
                .and_then(|id| ws.buffers.syntax_version(id))
        };
        assert!(
            syntax_of("a.rs").is_none(),
            "the least-recently-shown hidden buffer is evicted"
        );
        assert!(
            syntax_of("b.rs").is_some(),
            "the newest hidden buffer stays within the cap"
        );
        assert!(
            syntax_of("c.rs").is_some(),
            "the visible buffer is never evicted"
        );
    }
}
