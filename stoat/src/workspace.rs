mod name;
pub(crate) mod persist;
pub(crate) mod registry;

use crate::{
    agent_status::AgentStatus,
    app::{parse_buffer_step, ParseJobOutput, INDEX_EDIT_DEBOUNCE},
    badge::BadgeTray,
    buffer::{BufferId, SharedBuffer},
    buffer_registry::{self, BufferRegistry},
    code_index::{
        build::{file_id, reindex_buffer, IndexUpdate, ReindexTarget},
        nav::TrailState,
    },
    commit_list::CommitListState,
    conflict_session::ConflictSession,
    diff,
    diff_cache::ContentHash,
    diff_map::{changes_to_hunks, line_starts, BaseHighlights, DiffMap},
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
    term_session::{TermId, TermReturnFocus, TermSession},
};
use codegraph::{CodeGraph, FileId};
pub use persist::find_resume_anchor;
pub(crate) use persist::{anchor_state_dir, list_workspace_files, state_path_for, write_state};
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
    time::{Duration, Instant, UNIX_EPOCH},
};
use stoat_language::{
    extract_highlights, parse, structural_diff, HighlightSpan, Language, LanguageRegistry, Tree,
};
use stoat_scheduler::{Executor, Task};
use stoat_text::Rope;
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot, Notify,
};

new_key_type! {
    pub struct WorkspaceId;
}

/// How long a buffer must hold one version before its diff is recomputed.
///
/// A diff walks the whole file on a blocking thread and the next keystroke
/// invalidates it, so a typing burst is worth one diff at the end rather than
/// one per edit. Short enough that a reader pausing mid-thought still sees the
/// gutter catch up.
pub(crate) const DIFF_SETTLE: Duration = Duration::from_millis(250);

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

/// One tab in a workspace, owning a pane layout.
///
/// The active tab's tree is not stored here. It lives in [`Workspace::panes`],
/// where every rendering, focus, and layout site already reads it, so exactly
/// the entry at [`Workspace::active_tab`] holds `None`. Switching parks the
/// outgoing tree in its slot and takes the incoming one out.
///
/// A parked tree's [`EditorId`]s and [`TermId`]s stay valid while parked,
/// because editors, terms, runs, and buffers belong to the workspace rather
/// than to a tree.
pub(crate) struct Tab {
    pub(crate) parked: Option<PaneTree>,
    /// A user-set title overriding the one derived from the tab's focused pane.
    /// `None` falls back to [`Workspace::tab_title`]'s derived name. Set and
    /// cleared by the `RenameTab` action and persisted with the workspace.
    pub(crate) name: Option<String>,
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
    /// The active tab's pane layout. Every render, focus, and navigation site
    /// reads this and never sees the parked tabs.
    pub panes: PaneTree,
    /// Ordered tabs, at least one. The entry at [`Self::active_tab`] parks
    /// nothing because its tree is in [`Self::panes`].
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active_tab: usize,
    /// The tab to return to on a toggle, a most-recently-used depth of one.
    /// `None` before the first switch, and cleared when the tab it names is
    /// closed.
    pub(crate) last_tab: Option<usize>,
    pub(crate) docks: SlotMap<DockId, DockPanel>,
    pub(crate) focus: FocusTarget,
    pub(crate) buffers: BufferRegistry,
    pub(crate) editors: SlotMap<EditorId, EditorState>,
    pub(crate) runs: SlotMap<RunId, RunState>,
    pub(crate) terms: SlotMap<TermId, TermSession>,
    /// The app's redraw handle, so editors created after construction reach it.
    ///
    /// Held here rather than passed per call because the two places editors are
    /// born, [`Self::new`]'s scratch tree and [`Self::seeded_editor`], have no
    /// access to [`crate::app::Stoat`]. An editor's display map needs it to wake
    /// the run loop when a background rewrap settles, which no version change
    /// announces.
    pub(crate) redraw_notify: Arc<Notify>,
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
    /// Active three-way conflict resolve session (if any). Owned at the
    /// workspace level beside [`Self::review`]. Dropped on `CloseConflict`.
    pub(crate) conflict: Option<ConflictSession>,
    /// Active commit-listing state (if any). Parallel to [`Self::review`]:
    /// populated while the user is in `"commits"` mode and dropped on
    /// `CloseCommits`.
    pub(crate) commits: Option<CommitListState>,
    /// Active commit-by-commit review walk (if any). Outlives the review
    /// session it opens, so closing a diff leaves the walk in place and only
    /// `ReviewDone` ends it.
    pub(crate) review_walk: Option<crate::review_walk::ReviewWalk>,
    /// Active rebase plan (if any). Populated when the user enters
    /// `"rebase"` mode from the commit list; dropped on abort or after
    /// successful execution.
    pub(crate) rebase: Option<RebaseState>,
    /// In-flight rebase execution state. Present while the stepper is
    /// paused on reword/edit/conflict and during final execution;
    /// dropped when the plan completes or aborts.
    pub(crate) rebase_active: Option<ActiveRebase>,
    parse_jobs: HashMap<BufferId, ParseJob>,
    /// How many buffer snapshots the parse driver cloned.
    ///
    /// A clone the gates then discard costs only time, so a test needs the
    /// count to say a settled buffer stops paying for one.
    #[cfg(test)]
    parse_snapshot_clones: std::cell::Cell<u64>,
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
    /// Each diffed file's HEAD and index blobs, keyed by path.
    ///
    /// Saves the repo mutex and a blob decompression on every recompute, which
    /// is otherwise paid per keystroke. Cleared by [`Self::invalidate_all_diffs`],
    /// which the `.git` watcher drives, so an entry cannot outlive the git state
    /// it was read from.
    diff_base_text: HashMap<PathBuf, DiffBaseText>,
    /// The version each buffer is currently settling on, and when that version
    /// was first seen. Read by [`Self::diff_settled`].
    diff_settle: HashMap<BufferId, (u64, Instant)>,
    /// The redraw timers [`Self::diff_settled`] arms, held so they are not
    /// cancelled on drop. Replaced per buffer, so a burst keeps one.
    diff_settle_timers: HashMap<BufferId, Task<()>>,
    /// The buffer ids [`Self::collect_visible_buffer_ids`] last collected.
    ///
    /// Held rather than returned because both the parse and the diff driver
    /// collect them every redraw frame. Carries nothing between frames, and each
    /// user takes it out for the walk so its borrow never overlaps the rest of
    /// the workspace.
    visible_buffers: Vec<BufferId>,
    /// In-flight live-reindex jobs, one per buffer, held so the spawned
    /// extraction is not cancelled. Replaced when the buffer reparses.
    index_jobs: HashMap<BufferId, Task<()>>,
    /// Timers waiting out a buffer's typing before its symbols are extracted.
    ///
    /// A parse replaces the buffer's timer, so a burst leaves one survivor and
    /// costs one extract. Dropping a timer cancels it, which is the whole point:
    /// once an extract reaches the blocking pool nothing can call it back, so
    /// the collapsing has to happen before it is spawned.
    index_debounce: HashMap<BufferId, Task<()>>,
    /// Buffers whose debounce elapsed, waiting for the next
    /// [`Self::drive_parse_jobs`] to extract them.
    index_fire_tx: UnboundedSender<BufferId>,
    index_fire_rx: UnboundedReceiver<BufferId>,
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

/// A parse running for one buffer.
///
/// Carries no target version, unlike [`DiffJob`]. A buffer's next parse is
/// whatever is stale when this one lands, so which version it was spawned for
/// decides nothing once it is in flight.
struct ParseJob {
    task: Task<Option<ParseJobOutput>>,
}

struct DiffJob {
    target_version: u64,
    task: Task<DiffJobOutput>,
}

struct DiffJobOutput {
    buffer_id: BufferId,
    /// The file the job diffed, so its base text can be filed under the same
    /// key the next job for it will look under.
    path: PathBuf,
    target_version: u64,
    diff_map: Option<DiffMap>,
    /// The blobs the diff ran against, `None` when the repo or the file's HEAD
    /// content could not be read and there was nothing to diff.
    base: Option<DiffBaseText>,
}

/// A file's HEAD and index blobs as git last reported them.
///
/// Neither can change without a write under `.git`, which is watched, so a
/// diff recomputed for a keystroke can reuse what the last one read rather than
/// taking the repo mutex and decompressing the same bytes again.
#[derive(Clone)]
struct DiffBaseText {
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

/// A one-pane tree showing a fresh scratch buffer, the shape a workspace and
/// every new tab start in.
///
/// Takes the registry and slotmap rather than a `Workspace` so
/// [`Workspace::new`] can call it while building its own fields.
fn scratch_tree(
    buffers: &mut BufferRegistry,
    editors: &mut SlotMap<EditorId, EditorState>,
    executor: &Executor,
    redraw: &Arc<Notify>,
) -> PaneTree {
    let (buffer_id, buffer) = buffers.new_scratch();
    let editor_id = editors.insert(EditorState::new(
        buffer_id,
        buffer,
        executor.clone(),
        redraw.clone(),
    ));
    let mut panes = PaneTree::new(Rect::default());
    let focus = panes.focus();
    panes.pane_mut(focus).view = View::Editor(editor_id);
    panes
}

impl Workspace {
    pub(crate) fn new(git_root: PathBuf, executor: &Executor, redraw: Arc<Notify>) -> Self {
        let mut buffers = BufferRegistry::new();
        let mut editors = SlotMap::with_key();
        let panes = scratch_tree(&mut buffers, &mut editors, executor, &redraw);

        let uid = WorkspaceUid::now(executor);
        let name = name::default_workspace_name(uid);
        let (index_fire_tx, index_fire_rx) = mpsc::unbounded_channel();

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
            tabs: vec![Tab {
                parked: None,
                name: None,
            }],
            active_tab: 0,
            last_tab: None,
            docks: SlotMap::with_key(),
            focus: FocusTarget::SplitPane,
            buffers,
            editors,
            runs: SlotMap::with_key(),
            terms: SlotMap::with_key(),
            redraw_notify: redraw,
            code_graph: CodeGraph::new(),
            index_generation: 0,
            file_paths: HashMap::new(),
            changed_ranges: HashMap::new(),
            changed_ranges_memo: HashMap::new(),
            #[cfg(test)]
            changed_ranges_recomputes: 0,
            trail: None,
            review: None,
            conflict: None,
            commits: None,
            review_walk: None,
            rebase: None,
            rebase_active: None,
            parse_jobs: HashMap::new(),
            #[cfg(test)]
            parse_snapshot_clones: std::cell::Cell::new(0),
            diff_jobs: HashMap::new(),
            diff_versions: HashMap::new(),
            diff_base_text: HashMap::new(),
            diff_settle: HashMap::new(),
            diff_settle_timers: HashMap::new(),
            visible_buffers: Vec::new(),
            index_jobs: HashMap::new(),
            index_debounce: HashMap::new(),
            index_fire_tx,
            index_fire_rx,
            badges: BadgeTray::new(),
            agent: None,
            editor_bridge_waiters: HashMap::new(),
        }
    }

    /// Make tab `idx` active, parking the outgoing tree and taking the
    /// incoming one out. Returns whether the switch happened.
    ///
    /// A no-op for an out-of-range index or the already-active tab. Pane focus
    /// rides along inside each tree, and docks are workspace-level, so nothing
    /// outside the tree needs restoring.
    pub(crate) fn switch_tab(&mut self, idx: usize) -> bool {
        if idx == self.active_tab || idx >= self.tabs.len() {
            return false;
        }
        let Some(incoming) = self.tabs[idx].parked.take() else {
            return false;
        };

        let outgoing = std::mem::replace(&mut self.panes, incoming);
        self.tabs[self.active_tab].parked = Some(outgoing);
        self.last_tab = Some(self.active_tab);
        self.active_tab = idx;
        true
    }

    /// Append a tab showing a fresh scratch buffer and switch to it.
    pub(crate) fn new_tab(&mut self, executor: &Executor) {
        let redraw = self.redraw_notify.clone();
        let tree = scratch_tree(&mut self.buffers, &mut self.editors, executor, &redraw);
        self.tabs.push(Tab {
            parked: Some(tree),
            name: None,
        });
        self.switch_tab(self.tabs.len() - 1);
    }

    /// Remove tab `idx`, returning its pane tree for the caller to dispose of.
    ///
    /// `None` when the index is out of range or only one tab is left, since a
    /// workspace always has somewhere to show panes. Closing the active tab
    /// first moves to the most recently used tab, or the nearest neighbour when
    /// there is none.
    pub(crate) fn close_tab(&mut self, idx: usize) -> Option<PaneTree> {
        if idx >= self.tabs.len() || self.tabs.len() == 1 {
            return None;
        }

        if idx == self.active_tab {
            let target = match self.last_tab {
                Some(prev) if prev != idx && prev < self.tabs.len() => prev,
                _ => {
                    if idx + 1 < self.tabs.len() {
                        idx + 1
                    } else {
                        idx - 1
                    }
                },
            };
            self.switch_tab(target);
        }

        let removed = self.tabs.remove(idx).parked;

        // Indices above the hole all shift down by one, and a toggle target
        // that named the closed tab no longer refers to anything.
        if self.active_tab > idx {
            self.active_tab -= 1;
        }
        self.last_tab = match self.last_tab {
            Some(prev) if prev == idx => None,
            Some(prev) if prev > idx => Some(prev - 1),
            other => other,
        };

        // A terminal's return-focus record names a tab by index, so it shifts
        // with the same hole `last_tab` does, and a record pointing at the
        // closed tab has nowhere left to send Esc.
        for term in self.terms.values_mut() {
            match term.return_focus {
                Some(TermReturnFocus::Pane { tab, .. }) if tab == idx => term.return_focus = None,
                Some(TermReturnFocus::Pane { tab, pane }) if tab > idx => {
                    term.return_focus = Some(TermReturnFocus::Pane { tab: tab - 1, pane });
                },
                _ => {},
            }
        }

        removed
    }

    /// Switch back to the most recently used tab. Returns whether it happened.
    pub(crate) fn toggle_tab(&mut self) -> bool {
        match self.last_tab {
            Some(prev) => self.switch_tab(prev),
            None => false,
        }
    }

    /// What tab `idx` calls itself in the tab bar, taken from its focused
    /// pane's view.
    ///
    /// An editor names its file, or reads as scratch when it has none. The
    /// other views name their kind, since a terminal or run pane has nothing
    /// more specific to offer. An out-of-range index is empty.
    ///
    /// The kind names match the `pane` keymap predicate's, so what the bar
    /// shows and what a binding condition matches on read the same.
    ///
    /// A `RenameTab` override wins over the derived name, so a renamed tab keeps
    /// its title no matter what its focused pane shows.
    pub(crate) fn tab_title(&self, idx: usize) -> String {
        if let Some(name) = self.tabs.get(idx).and_then(|tab| tab.name.as_ref()) {
            return name.clone();
        }

        let tree = if idx == self.active_tab {
            Some(&self.panes)
        } else {
            self.tabs.get(idx).and_then(|tab| tab.parked.as_ref())
        };
        let Some(tree) = tree else {
            return String::new();
        };

        match &tree.pane(tree.focus()).view {
            View::Editor(id) => self
                .editors
                .get(*id)
                .and_then(|editor| self.buffers.path_for(editor.buffer_id))
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "scratch".to_string()),
            View::Agent(_) => "agent".to_string(),
            View::Terminal(_) => "term".to_string(),
            View::Run(_) => "run".to_string(),
            View::Label(_) => "pane".to_string(),
        }
    }

    /// Every pane tree in the workspace, the active one first.
    ///
    /// Use this for questions about whether anything still references an
    /// editor, term, or buffer, since a pane in a parked tab references it just
    /// as much as a visible one. Rendering, focus, and layout want
    /// [`Self::panes`] instead.
    pub(crate) fn pane_trees(&self) -> impl Iterator<Item = &PaneTree> {
        std::iter::once(&self.panes).chain(self.tabs.iter().filter_map(|t| t.parked.as_ref()))
    }

    /// Every pane tree in the workspace mutably, the active one first. The
    /// read-only [`Self::pane_trees`] explains when to reach for this.
    pub(crate) fn pane_trees_mut(&mut self) -> impl Iterator<Item = &mut PaneTree> {
        std::iter::once(&mut self.panes)
            .chain(self.tabs.iter_mut().filter_map(|t| t.parked.as_mut()))
    }

    /// Whether any pane, in any tab, shows `editor_id`.
    pub(crate) fn editor_referenced(&self, editor_id: EditorId) -> bool {
        self.pane_trees().any(|tree| {
            tree.split_panes()
                .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == editor_id))
        })
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

    /// Stale every buffer's diff map by dropping all recorded versions and
    /// in-flight jobs.
    ///
    /// Used after git state moves under the editor -- an external rebase or
    /// checkout changes HEAD, so a map computed against the old one describes a
    /// base that no longer exists, whatever the buffer's own version says.
    ///
    /// In-flight jobs are dropped as [`Self::invalidate_diff`] drops them, since
    /// their results would carry the same stale base. The next
    /// [`Self::drive_diff_jobs`] pass recomputes only visible buffers, so hidden
    /// ones re-diff lazily when they are next shown.
    pub(crate) fn invalidate_all_diffs(&mut self) {
        self.diff_jobs.clear();
        self.diff_versions.clear();
        // The cached blobs were read from the git state this call is reacting
        // to having changed, so they are exactly what must not be reused.
        self.diff_base_text.clear();
    }

    /// Whether `id`'s installed diff map was computed for the buffer's current
    /// content and the current git state.
    ///
    /// False for a buffer edited past its recorded version, one whose map was
    /// invalidated, and one that never had a map. Callers that need fresh hunks
    /// on the current turn pair this with [`Self::install_diff_map_now`], since
    /// the presence of a map alone does not prove it is current.
    pub(crate) fn diff_map_current(&self, id: BufferId) -> bool {
        let Some(shared) = self.buffers.get(id) else {
            return false;
        };
        let version = shared.read().expect("buffer poisoned").snapshot.version;
        self.diff_versions.get(&id) == Some(&version)
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
        let computed = compute_diff_map(
            &**git_host,
            &self.git_root,
            &path,
            &text,
            language.as_ref(),
            syntax_styles,
            base_cache,
            self.diff_base_text.get(&path).cloned(),
        );
        let diff_map = computed.map(|(diff_map, base)| {
            self.diff_base_text.insert(path.clone(), base);
            diff_map
        });
        if let Some(shared) = self.buffers.get(id) {
            shared.write().expect("buffer poisoned").diff_map = diff_map;
        }
        self.diff_versions.insert(id, version);
    }

    /// Install `diff_map` on `id` and record it as computed for the buffer's
    /// current version, so it reads as current to [`Self::diff_map_current`].
    ///
    /// Lets a test stand up a diff map without a git fixture behind it. Writing
    /// the map alone would leave it version-less, which every caller correctly
    /// treats as stale.
    #[cfg(test)]
    pub(crate) fn install_test_diff_map(&mut self, id: BufferId, diff_map: DiffMap) {
        let Some(shared) = self.buffers.get(id) else {
            return;
        };
        let version = {
            let mut guard = shared.write().expect("buffer poisoned");
            guard.diff_map = Some(diff_map);
            guard.snapshot.version
        };
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
        let mut editor = EditorState::new(buffer_id, buffer, executor, self.redraw_notify.clone());
        if let Some(channel) = self.buffers.tokens_for(buffer_id) {
            editor
                .display_map
                .set_semantic_token_channel(buffer_id, channel.clone());
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
    /// At most one job per buffer is in flight at a time. A buffer edited while
    /// its parse runs is not parsed again until that one lands, so the edit is
    /// picked up on the frame after. Anchors in the result are computed using
    /// the parsed snapshot, so they remain valid even if the buffer has been
    /// edited further while the parse was running.
    ///
    /// Returns each buffer whose tokens were installed, paired with the rows
    /// the parse changed, or `None` where that is unknown. Callers holding
    /// per-buffer views of the tokens use it to bound their own re-coloring;
    /// [`crate::minimap::MinimapContent::note_syntax_rows`] is the one that
    /// does today.
    pub(crate) fn drive_parse_jobs(
        &mut self,
        executor: &Executor,
        syntax_styles: &SyntaxStyles,
        redraw_notify: &Arc<Notify>,
        index_update_tx: &UnboundedSender<IndexUpdate>,
        retention: usize,
    ) -> Vec<(BufferId, Option<Range<u32>>)> {
        // Ahead of this pass's parses, so a buffer that has gone quiet is
        // extracted before anything arms its timer again.
        self.drain_index_debounce(executor, index_update_tx, redraw_notify);

        let mut installed: Vec<(BufferId, Option<Range<u32>>)> = Vec::new();
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
            installed.push((out.buffer_id, out.changed_token_rows.clone()));
            self.buffers.store_syntax(out.buffer_id, out.syntax);
            self.buffers.store_syntax_map(out.buffer_id, out.syntax_map);
            self.buffers
                .store_token_spans(out.buffer_id, out.token_spans.clone());
            self.buffers
                .store_tokens(out.buffer_id, out.token_channel.clone());
            for editor in self.editors.values_mut() {
                if editor.buffer_id == out.buffer_id {
                    editor
                        .display_map
                        .set_semantic_token_channel(out.buffer_id, out.token_channel.clone());
                }
            }
            self.arm_index_debounce(executor, redraw_notify, out.buffer_id);
        }

        let mut visible = std::mem::take(&mut self.visible_buffers);
        self.collect_visible_buffer_ids(&mut visible);

        // The staleness checks come first so a settled buffer costs one lock
        // acquisition and nothing else. Its snapshot would otherwise be cloned
        // and dropped every frame, which is two rope clones per visible buffer
        // for an answer the version already gave.
        for &buffer_id in &visible {
            let Some(shared) = self.buffers.get(buffer_id) else {
                continue;
            };
            let cur_version = shared.read().expect("buffer poisoned").snapshot.version;

            if self.buffers.syntax_version(buffer_id) == Some(cur_version) {
                continue;
            }
            // A job in flight owns this buffer's next parse whatever version it
            // targets, since only one runs per buffer at a time and the result
            // installs before another can start. A job left behind by an edit
            // it no longer covers is superseded on the frame after it lands.
            if self.parse_jobs.contains_key(&buffer_id) {
                continue;
            }
            let Some(lang) = self.buffers.language_for(buffer_id) else {
                continue;
            };

            #[cfg(test)]
            self.parse_snapshot_clones
                .set(self.parse_snapshot_clones.get() + 1);
            let snapshot = shared.read().expect("buffer poisoned").snapshot.clone();

            // Cloned rather than taken, so everything that reads the tree keeps
            // reading the last landed one while the job flies. Both clones are
            // refcount bumps. The spans are taken, since only the parse pipeline
            // reads them and at most one job per buffer is ever in flight.
            let prior = self.buffers.syntax(buffer_id).cloned();
            let prior_map = self.buffers.syntax_map(buffer_id).cloned();
            let prior_spans = self.buffers.take_token_spans(buffer_id);
            // The same parse's anchored tokens, index-aligned with the spans,
            // so the incremental path can hand a carried token back its anchor.
            let prior_anchors = self.buffers.tokens_for(buffer_id).cloned();

            // Every buffer parses here, whatever its size. The tree-sitter parse
            // honors a deadline, but the captures walk after it does not and is
            // unbounded O(file), so parsing inline would hold the run loop for
            // the whole file on a keystroke.
            //
            // The result installs through the job poll a frame later. Until then
            // the registry still holds the last landed tree and token set, so
            // auto-indent, textobjects, and the highlight readers answer from
            // one edit ago rather than from nothing, and the carried tokens are
            // anchored so they stay on their text. A buffer opened for the first
            // time has no such state, and renders unstyled until its job lands.
            //
            // A blocking-pool run rather than a plain spawn, because the parse is
            // one non-yielding stretch of CPU and the app runs a current_thread
            // runtime. A spawned future would poll it on the run-loop thread and
            // freeze the UI until the whole file was done.
            let styles = syntax_styles.clone();
            let parse = executor.spawn_blocking(move || {
                let mut prior = prior;
                let mut prior_map = prior_map;
                parse_buffer_step(
                    buffer_id,
                    snapshot,
                    &lang,
                    &mut prior,
                    &mut prior_map,
                    prior_spans.as_deref(),
                    prior_anchors.as_ref(),
                    &styles,
                    None,
                )
            });
            let task = executor.spawn_with_redraw(redraw_notify.clone(), parse);
            self.parse_jobs.insert(buffer_id, ParseJob { task });
        }

        // Cap retained highlight state, on the passes where a parse landed.
        // In-flight parse ids join the visible set so a completing job cannot
        // repopulate a just-evicted buffer.
        //
        // Tree-sitter state reaches the registry only through the install
        // above, so a pass that installed nothing has no new state to cap and
        // the walk over open buffers would find what the last one left. LSP
        // token sets are evictable too and install elsewhere, so a buffer
        // holding only those waits for some buffer's next parse.
        let mut protected = visible;
        protected.extend(self.parse_jobs.keys().copied());
        if !installed.is_empty() {
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
        // Handed back for its capacity alone. The eviction set grew past the
        // visible ids, and the next collect clears it before reading.
        self.visible_buffers = protected;

        installed
    }

    /// Buffer ids currently shown in a split-pane editor or held as a preview,
    /// deduplicated. Drives which buffers the background parse and diff jobs keep
    /// current.
    fn collect_visible_buffer_ids(&self, visible: &mut Vec<BufferId>) {
        visible.clear();
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
            if let Some(base) = out.base {
                self.diff_base_text.insert(out.path, base);
            }
            if let Some(shared) = self.buffers.get(out.buffer_id) {
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
            self.diff_versions.insert(out.buffer_id, out.target_version);
        }

        let mut visible = std::mem::take(&mut self.visible_buffers);
        self.collect_visible_buffer_ids(&mut visible);

        // The staleness checks come first so a settled buffer costs one lock
        // acquisition and nothing else. Its path would otherwise be cloned and
        // dropped every frame.
        for &buffer_id in &visible {
            let Some(shared) = self.buffers.get(buffer_id) else {
                continue;
            };
            let cur_version = shared.read().expect("buffer poisoned").snapshot.version;

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
            if !self.diff_settled(executor, redraw_notify, buffer_id, cur_version) {
                continue;
            }

            let Some(path) = self.buffers.path_for(buffer_id).map(Path::to_path_buf) else {
                continue;
            };
            // The whole snapshot rather than its rope, since the hunks are
            // anchored against the text they were diffed from.
            let buffer_snapshot = shared.read().expect("buffer poisoned").snapshot.clone();

            let language = language_registry.for_path(&path);
            let cached_base = self.diff_base_text.get(&path).cloned();
            let task = executor.spawn_blocking({
                let git_host = git_host.clone();
                let git_root = self.git_root.clone();
                let redraw = redraw_notify.clone();
                let syntax_styles = syntax_styles.clone();
                let base_cache = base_cache.clone();
                let path = path.clone();
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
                    )
                    .map(|(mut diff_map, base)| {
                        diff_map.anchor_hunks(&buffer_snapshot);
                        (diff_map, base)
                    });
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
        self.visible_buffers = visible;
    }

    /// Whether `buffer_id` has held `version` long enough to be worth diffing,
    /// opening the settle window on the version's first sighting.
    ///
    /// A diff costs a blocking thread and a walk of the whole file, and a
    /// keystroke invalidates whatever the last one produced, so a burst is
    /// worth one diff rather than one per edit. The window restarts on every
    /// version change, so it closes only once typing stops.
    ///
    /// Opening one arms a redraw timer, because the pass that spawns the job is
    /// a frame, and a reader who stops typing generates no more of those.
    fn diff_settled(
        &mut self,
        executor: &Executor,
        redraw_notify: &Arc<Notify>,
        buffer_id: BufferId,
        version: u64,
    ) -> bool {
        let now = executor.now();
        match self.diff_settle.get(&buffer_id) {
            Some((settling, since)) if *settling == version => {
                if now.duration_since(*since) < DIFF_SETTLE {
                    return false;
                }
                self.diff_settle.remove(&buffer_id);
                true
            },
            _ => {
                self.diff_settle.insert(buffer_id, (version, now));
                let timer_executor = executor.clone();
                let task = executor.spawn_with_redraw(redraw_notify.clone(), async move {
                    timer_executor.timer(DIFF_SETTLE).await;
                });
                self.diff_settle_timers.insert(buffer_id, task);
                false
            },
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

    /// Wait out `buffer_id`'s typing before extracting its symbols again.
    ///
    /// Replaces any timer already waiting on this buffer, so a burst of parses
    /// leaves one survivor and costs one extract. The buffer id travels rather
    /// than its text, because the text a keystroke armed with is already stale
    /// by the time the window elapses.
    fn arm_index_debounce(
        &mut self,
        executor: &Executor,
        redraw_notify: &Arc<Notify>,
        buffer_id: BufferId,
    ) {
        let tx = self.index_fire_tx.clone();
        let timer_executor = executor.clone();
        let task = executor.spawn_with_redraw(redraw_notify.clone(), async move {
            timer_executor.timer(INDEX_EDIT_DEBOUNCE).await;
            let _ = tx.send(buffer_id);
        });
        self.index_debounce.insert(buffer_id, task);
    }

    /// Extract the symbols of every buffer whose debounce has elapsed.
    ///
    /// Reads each buffer's rope here rather than carrying it through the timer,
    /// so the extract sees the text as it stands now.
    fn drain_index_debounce(
        &mut self,
        executor: &Executor,
        index_update_tx: &UnboundedSender<IndexUpdate>,
        redraw_notify: &Arc<Notify>,
    ) {
        let mut fired = Vec::new();
        while let Ok(buffer_id) = self.index_fire_rx.try_recv() {
            fired.push(buffer_id);
        }

        for buffer_id in fired {
            self.index_debounce.remove(&buffer_id);
            let snapshot = self.buffers.get(buffer_id).map(|shared| {
                let guard = shared.read().expect("buffer poisoned");
                (guard.snapshot.visible_text.clone(), guard.snapshot.version)
            });
            let Some((text, version)) = snapshot else {
                continue;
            };

            // The debounce fires after the parse for this version landed, so the
            // stored tree normally describes exactly this text and extraction can
            // skip re-parsing the file. An edit inside the window leaves the
            // versions apart, and a tree built from other text would put every
            // symbol range somewhere else, so that case parses as before.
            let tree = self
                .buffers
                .syntax(buffer_id)
                .filter(|syntax| syntax.version == version)
                .map(|syntax| syntax.tree.clone());

            self.enqueue_reindex(
                executor,
                index_update_tx,
                redraw_notify,
                buffer_id,
                text,
                tree,
            );
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
        tree: Option<Tree>,
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
            tree,
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
        // The walk reads the pane tree while the fit writes to the terms, so
        // both fields are borrowed apart rather than the ids being collected to
        // get one loop out of the way of the other.
        let Self { panes, terms, .. } = self;
        for (_, pane) in panes.split_panes() {
            let (View::Agent(id) | View::Terminal(id)) = pane.view else {
                continue;
            };
            if pane.area.width == 0 || pane.area.height == 0 {
                continue;
            }
            let (content, _) = split_pane_status(pane.area);
            if let Some(agent) = terms.get_mut(id) {
                agent.fit(content.height, content.width);
            }
        }
    }
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

/// Compute a buffer's HEAD-vs-worktree [`DiffMap`], or [`None`] when the file
/// is outside a repo or has no HEAD content to diff against.
///
/// Both `discover` and `head_content` do git and filesystem IO, so this must
/// run on a blocking thread. Uses the language-agnostic line diff, matching
/// [`changed_byte_ranges`].
#[allow(clippy::too_many_arguments)]
fn compute_diff_map(
    git: &dyn GitHost,
    git_root: &Path,
    path: &Path,
    buffer_text: &str,
    language: Option<&Arc<Language>>,
    syntax_styles: &SyntaxStyles,
    base_cache: &BaseHighlightCache,
    cached_base: Option<DiffBaseText>,
) -> Option<(DiffMap, DiffBaseText)> {
    // Reading the blobs is what costs. It takes the repo mutex and then
    // decompresses bytes that a keystroke cannot have changed. The pair is
    // handed back either way so the caller can file it for the next one.
    let base = match cached_base {
        Some(base) => base,
        None => {
            let repo = git.discover(git_root)?;
            let head = Arc::new(repo.head_content(path)?);
            // A file with no index entry shares HEAD's handle rather than a
            // copy of its bytes, which the fingerprint below then reads off.
            let index = match repo.index_content(path) {
                Some(index) => Arc::new(index),
                None => head.clone(),
            };
            let head_hash = buffer_registry::fingerprint_bytes(&head);
            let index_hash = match Arc::ptr_eq(&head, &index) {
                true => head_hash,
                false => buffer_registry::fingerprint_bytes(&index),
            };
            DiffBaseText {
                head,
                index,
                head_hash,
                index_hash,
            }
        },
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

    let mut diff_map = DiffMap::from_structural_changes_staged(
        result,
        base.head.clone(),
        buffer_text,
        &index_changed,
    );
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

/// Highlight `base_text` for the diff view's left column, per base line.
///
/// Memoized against `cache` twice over. An unchanged base text is parsed once,
/// and its spans are resolved and bucketed once per theme, so a keystroke burst
/// that leaves the base alone costs two hash lookups rather than a walk over
/// every span with a style clone per line it touches.
fn compute_base_highlights(
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
        changed_byte_ranges, compute_base_highlights, BaseHighlightMemo, ParseJob, Workspace,
    };
    use crate::{
        buffer::BufferId, display_map::syntax_theme::SyntaxStyles, host::DiffStatus, pane::View,
        review::ReviewFileInput, test_harness::TestHarness, theme::Theme,
    };
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use stoat_language::LanguageRegistry;
    use stoat_scheduler::{Task, TestScheduler};

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

    /// The editor showing in the workspace's focused pane, or `None` when the
    /// focused pane shows something else.
    fn focused_editor_id(ws: &Workspace) -> Option<crate::editor_state::EditorId> {
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => Some(id),
            _ => None,
        }
    }

    #[test]
    fn new_tab_parks_the_old_tree_and_opens_a_scratch_pane() {
        let mut h = TestHarness::with_size(80, 24);
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let first_editor = focused_editor_id(ws).expect("first tab shows an editor");

        ws.new_tab(&executor);

        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.active_tab, 1);
        assert!(
            ws.tabs[0].parked.is_some(),
            "the outgoing tree parks in its own slot"
        );
        assert!(
            ws.tabs[1].parked.is_none(),
            "the active tab parks nothing, its tree is in ws.panes"
        );
        assert_ne!(
            focused_editor_id(ws),
            Some(first_editor),
            "the new tab shows its own scratch editor"
        );
    }

    #[test]
    fn switch_tab_round_trips_the_editor_and_its_cursor() {
        let mut h = TestHarness::with_size(80, 24);
        crate::test_harness::editor::seed_focused_buffer(&mut h.stoat, "alpha\nbeta\ngamma\n");
        h.type_keys("j l l");
        let cursor = crate::test_harness::editor::primary_head_offset(&mut h.stoat);

        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let first_editor = focused_editor_id(ws).expect("editor in the first tab");
        let first_buffer = ws.editors[first_editor].buffer_id;

        ws.new_tab(&executor);
        assert!(ws.switch_tab(0), "switching back to the first tab");

        assert_eq!(ws.active_tab, 0);
        assert_eq!(focused_editor_id(ws), Some(first_editor), "same editor id");
        assert_eq!(
            ws.editors[first_editor].buffer_id, first_buffer,
            "same buffer"
        );
        assert_eq!(
            crate::test_harness::editor::primary_head_offset(&mut h.stoat),
            cursor,
            "the cursor is where it was left"
        );
    }

    #[test]
    fn toggle_tab_alternates_between_the_last_two() {
        let mut h = TestHarness::with_size(80, 24);
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();

        assert!(!ws.toggle_tab(), "nothing to toggle back to yet");

        ws.new_tab(&executor);
        ws.new_tab(&executor);
        assert_eq!(ws.active_tab, 2);

        assert!(ws.toggle_tab());
        assert_eq!(ws.active_tab, 1, "back to the previously active tab");
        assert!(ws.toggle_tab());
        assert_eq!(ws.active_tab, 2, "and forward again");
    }

    #[test]
    fn close_tab_lands_on_the_most_recent_and_fixes_indices() {
        let mut h = TestHarness::with_size(80, 24);
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        ws.new_tab(&executor);
        ws.new_tab(&executor);

        // Tab 0 was visited most recently before landing on 2.
        assert!(ws.switch_tab(0));
        assert!(ws.switch_tab(2));
        assert_eq!(ws.last_tab, Some(0));

        assert!(ws.close_tab(2).is_some(), "the closed tree is handed back");
        assert_eq!(ws.tabs.len(), 2);
        assert_eq!(ws.active_tab, 0, "closing the active tab lands on the MRU");
        assert_eq!(
            ws.last_tab, None,
            "the toggle target named the tab we just left, which no longer exists"
        );
        assert!(
            ws.tabs[0].parked.is_none(),
            "the surviving active tab parks nothing"
        );
    }

    #[test]
    fn close_tab_refuses_to_remove_the_last_one() {
        let mut h = TestHarness::with_size(80, 24);
        let ws = h.stoat.active_workspace_mut();
        assert!(ws.close_tab(0).is_none(), "a workspace keeps one tab");
        assert_eq!(ws.tabs.len(), 1);
    }

    /// An editor visible only in a parked tab is still in use. Collecting it
    /// would leave that tab pointing at a dangling id the moment it is shown
    /// again.
    #[test]
    fn an_editor_shown_only_in_a_parked_tab_survives_gc() {
        let mut h = TestHarness::with_size(80, 24);
        let executor = h.stoat.executor.clone();
        let ws = h.stoat.active_workspace_mut();
        let parked_editor = focused_editor_id(ws).expect("editor in the first tab");

        ws.new_tab(&executor);
        assert_ne!(focused_editor_id(ws), Some(parked_editor));

        crate::action_handlers::gc_editor_if_unreferenced(
            h.stoat.active_workspace_mut(),
            parked_editor,
        );

        assert!(
            h.stoat
                .active_workspace()
                .editors
                .contains_key(parked_editor),
            "the parked tab still references it"
        );
    }

    #[test]
    fn tab_title_prefers_a_rename_over_the_derived_name_and_clears_back() {
        let mut h = TestHarness::with_size(80, 24);
        let ws = h.stoat.active_workspace_mut();
        let derived = ws.tab_title(0);

        ws.tabs[0].name = Some("notes".to_string());
        assert_eq!(ws.tab_title(0), "notes", "the override wins");

        ws.tabs[0].name = None;
        assert_eq!(
            ws.tab_title(0),
            derived,
            "clearing falls back to the derived title"
        );
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
                h.stoat.active_workspace().diff_jobs.is_empty(),
                "a keystroke inside the settle window spawns no diff",
            );
        }

        let before = h.fake_git().blob_reads(&repo);
        h.advance_clock(super::DIFF_SETTLE + std::time::Duration::from_millis(1));
        h.stoat.drive_background();
        assert_eq!(
            h.stoat.active_workspace().diff_jobs.len(),
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
            h.stoat.active_workspace().diff_jobs.is_empty(),
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
        let mut ws = Workspace::new(PathBuf::new(), &executor, crate::test_notify());
        let (id, _) = ws.buffers.open(Path::new("/repo/foo.rs"), "fn main() {}");

        assert_eq!(ws.buffers.language_for(id).map(|l| l.name), None);

        ws.assign_languages_from_paths(&LanguageRegistry::standard());

        assert_eq!(ws.buffers.language_for(id).map(|l| l.name), Some("rust"));
    }

    #[test]
    fn reset_preview_syntax_cancels_in_flight_parse() {
        let executor = Arc::new(TestScheduler::new()).executor();
        let mut ws = Workspace::new(PathBuf::new(), &executor, crate::test_notify());
        let (id, _) = ws.buffers.new_scratch_preview();
        ws.parse_jobs.insert(
            id,
            ParseJob {
                task: Task::Ready(None),
            },
        );

        ws.reset_preview_syntax(id);

        assert!(
            !ws.parse_jobs.contains_key(&id),
            "swapping preview content drops the prior file's parse job"
        );
    }

    /// The parse driver runs for every visible buffer on every redraw, and in
    /// the steady state none of them needs parsing. A snapshot cloned before
    /// the version is consulted is two rope clones per buffer per frame, thrown
    /// away for an answer the version already gave.
    ///
    /// The stale buffer still clones, which is what says the count is measuring
    /// something.
    #[test]
    fn a_settled_buffer_clones_no_snapshot_to_decide_it_is_settled() {
        use crate::action_handlers::dispatch;
        use stoat_action::OpenFile;

        let mut h = TestHarness::with_size(24, 4);
        let root = PathBuf::from("/settled");
        h.fake_fs()
            .insert_file(root.join("settled.rs"), b"fn f() {}\n");
        h.stoat.active_workspace_mut().git_root = root.clone();

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("settled.rs"),
            },
        );

        // The open leaves the buffer unparsed, so the first pass has to clone.
        let before = h.stoat.active_workspace().parse_snapshot_clones.get();
        h.stoat.drive_background();
        assert!(
            h.stoat.active_workspace().parse_snapshot_clones.get() > before,
            "a buffer with no syntax yet is parsed, which needs its snapshot"
        );

        // Let the job land so the recorded syntax version catches up.
        for _ in 0..8 {
            h.tick();
            h.stoat.drive_background();
        }
        let ws = h.stoat.active_workspace();
        let id = ws
            .buffers
            .id_for_path(&root.join("settled.rs"))
            .expect("the buffer opened");
        assert!(
            ws.buffers.syntax_version(id).is_some(),
            "the parse landed, so the buffer is settled"
        );

        let settled = h.stoat.active_workspace().parse_snapshot_clones.get();
        for _ in 0..4 {
            h.stoat.drive_background();
        }
        assert_eq!(
            h.stoat.active_workspace().parse_snapshot_clones.get(),
            settled,
            "four redraws over a settled buffer clone nothing"
        );
    }

    /// The captures walk after a parse is unbounded O(file), so no buffer is
    /// small enough to be worth running it between keystrokes. A tiny one takes
    /// the same background path a large one does.
    #[test]
    fn a_small_buffer_parses_off_the_main_thread_too() {
        use crate::action_handlers::dispatch;
        use stoat_action::OpenFile;

        let mut h = TestHarness::with_size(24, 4);
        let root = PathBuf::from("/small");
        h.fake_fs()
            .insert_file(root.join("small.rs"), b"fn f() {}\n");
        h.stoat.active_workspace_mut().git_root = root.clone();

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("small.rs"),
            },
        );
        // Drive parse jobs once without ticking the scheduler, so the spawned
        // background job stays pending and observable.
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        let id = ws
            .buffers
            .id_for_path(&root.join("small.rs"))
            .expect("the buffer opened");
        assert!(
            ws.parse_jobs.contains_key(&id),
            "every buffer spawns a background parse job"
        );
        assert!(
            ws.buffers.syntax_version(id).is_none(),
            "and none is parsed inline on the main thread"
        );

        // The scheduler tick resolves the blocking task. The second
        // drive_background is the poll that moves its output into the registry.
        h.settle();
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        assert!(
            !ws.parse_jobs.contains_key(&id),
            "the finished job leaves the map"
        );
        assert!(
            ws.buffers.syntax_version(id).is_some(),
            "and its highlights land through the job poll"
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
