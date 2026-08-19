pub(crate) mod diff;
mod name;
pub(crate) mod persist;
pub(crate) mod registry;

#[cfg(test)]
use crate::diff_map::DiffMap;
use crate::{
    agent_status::AgentStatus,
    badge::BadgeTray,
    buffer::{BufferId, SharedBuffer},
    buffer_registry::BufferRegistry,
    code_index::{
        build::{reindex_buffer, IndexUpdate, ReindexTarget},
        nav::TrailState,
    },
    commit_list::CommitListState,
    conflict_session::ConflictSession,
    debounce::INDEX_EDIT_DEBOUNCE,
    display_map::syntax_theme::SyntaxStyles,
    editor_state::{EditorId, EditorState},
    host::GitHost,
    input_history::InputHistory,
    pane::{DockId, DockPanel, DockSide, FocusTarget, PaneTree, View},
    rebase::{ActiveRebase, RebaseState},
    render::layout::split_pane_status,
    review_session::ReviewSession,
    run::{RunId, RunState},
    syntax_parse::{parse_buffer_step, ParseJobOutput},
    term_session::{TermId, TermReturnFocus, TermSession},
    workspace::diff::{BaseHighlightCache, ChangedRangesMemo, ChangedRangesScan, DiffState},
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
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, UNIX_EPOCH},
};
use stoat_language::{LanguageRegistry, Tree};
use stoat_scheduler::{Executor, Task};
use stoat_text::Rope;
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    oneshot, Notify,
};

new_key_type! {
    pub struct WorkspaceId;
}

/// Largest buffer the run loop will try to parse itself before handing the
/// work to the blocking pool.
///
/// The captures walk after the tree-sitter parse honours no deadline, so the
/// only bound on it is the size of the file it walks. This is the size below
/// which that walk is short enough to sit inside a frame.
const INLINE_PARSE_MAX_BYTES: usize = 256 * 1024;

/// How long the run loop gives an inline parse before abandoning it.
///
/// An edit to an already-parsed buffer reuses its tree and finishes well
/// inside this. Anything that does not is a reparse worth moving off the run
/// loop, which the abort does at the cost of the time already spent.
const INLINE_PARSE_BUDGET: Duration = Duration::from_millis(1);

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
    /// [`scan_changed_ranges`] so diff-filtered navigation asks whether a
    /// symbol's definition overlaps a change.
    pub(crate) changed_ranges: HashMap<FileId, Vec<Range<usize>>>,
    /// Per-file diff memo, so repeated diff-filtered navigation over an
    /// unchanged tree recomputes nothing. The memo persists across refreshes,
    /// while [`Self::changed_ranges`] is replaced by each scan.
    changed_ranges_memo: ChangedRangesMemo,
    /// Count of memo misses (actual diffs run) landed by
    /// [`Self::install_changed_ranges`], so a test proves the memo spares the
    /// recompute on an unchanged tree.
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
    /// Walkthrough being played (if any). Opened by `WalkthroughOpen` and
    /// dropped only by `WalkthroughDone`, so moving away from a stop's file
    /// leaves the tour where it was.
    pub(crate) walkthrough: Option<crate::walkthrough::run::WalkthroughRun>,
    /// Active rebase plan (if any). Populated when the user enters
    /// `"rebase"` mode from the commit list; dropped on abort or after
    /// successful execution.
    pub(crate) rebase: Option<RebaseState>,
    /// In-flight rebase execution state. Present while the stepper is
    /// paused on reword/edit/conflict and during final execution;
    /// dropped when the plan completes or aborts.
    pub(crate) rebase_active: Option<ActiveRebase>,
    parse_jobs: HashMap<BufferId, ParseJob>,
    /// Buffers whose last parse captured only what was on screen.
    ///
    /// Their tree is current but their tokens describe the viewport alone, so
    /// the scheduling loop owes them one more parse however current their
    /// syntax version looks. Emptied when a whole-file walk lands.
    partial_token_buffers: std::collections::HashSet<BufferId>,
    /// How many buffer snapshots the parse driver cloned.
    ///
    /// A clone the gates then discard costs only time, so a test needs the
    /// count to say a settled buffer stops paying for one.
    #[cfg(test)]
    parse_snapshot_clones: std::cell::Cell<u64>,
    /// Everything the git-diff pipeline remembers between passes, which
    /// [`Self::drive_diff_jobs`] reads to decide what is owed a recompute.
    diff: DiffState,
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
            walkthrough: None,
            rebase: None,
            rebase_active: None,
            parse_jobs: HashMap::new(),
            partial_token_buffers: std::collections::HashSet::new(),
            #[cfg(test)]
            parse_snapshot_clones: std::cell::Cell::new(0),
            diff: DiffState::default(),
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
        self.diff.invalidate(id);
    }

    /// Drop every piece of per-buffer state this workspace holds for `id`,
    /// called when the buffer closes.
    ///
    /// Nothing else drops these. The parse, diff, and index drivers key their
    /// caches and in-flight jobs by buffer, and each keeps its entry until
    /// something replaces it, so a closed buffer's entries otherwise sit there
    /// for the rest of the session. Four of the collections hold a `Task`, and
    /// dropping it cancels work whose result nothing reads any more.
    ///
    /// `path` releases the file's cached HEAD and index blobs, which are the
    /// bulk of what a long browsing session accumulates. [`Self::invalidate_diff`]
    /// keeps them on purpose, since an edit does not move the base, but a close
    /// leaves nothing to reuse them.
    pub(crate) fn release_buffer(&mut self, id: BufferId, path: Option<&Path>) {
        self.parse_jobs.remove(&id);
        self.partial_token_buffers.remove(&id);
        self.index_jobs.remove(&id);
        self.index_debounce.remove(&id);
        self.diff.release(id, path);
    }

    /// Whether any state [`Self::release_buffer`] drops still exists for `id`,
    /// `path`'s cached diff base included.
    ///
    /// The collections are private to this module, so a close test outside it
    /// has no other way to say that nothing was left behind. Kept beside
    /// `release_buffer` so the two lists stay the same list.
    #[cfg(test)]
    pub(crate) fn holds_buffer_state(&self, id: BufferId, path: Option<&Path>) -> bool {
        self.parse_jobs.contains_key(&id)
            || self.partial_token_buffers.contains(&id)
            || self.index_jobs.contains_key(&id)
            || self.index_debounce.contains_key(&id)
            || self.diff.holds(id, path)
    }

    /// Force the next [`Self::drive_diff_jobs`] pass to recompute `id`'s diff
    /// map. See [`DiffState::invalidate`].
    pub(crate) fn invalidate_diff(&mut self, id: BufferId) {
        self.diff.invalidate(id);
    }

    /// Stale every buffer's diff map. See [`DiffState::invalidate_all`].
    pub(crate) fn invalidate_all_diffs(&mut self) {
        self.diff.invalidate_all();
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
        self.diff.current(id, version)
    }

    /// Compute and install `id`'s hunks on the current turn. See
    /// [`DiffState::install_now`].
    pub(crate) fn install_diff_map_now(
        &mut self,
        git_host: &Arc<dyn GitHost>,
        language_registry: &Arc<LanguageRegistry>,
        syntax_styles: &SyntaxStyles,
        base_cache: &BaseHighlightCache,
        id: BufferId,
    ) {
        self.diff.install_now(
            &self.buffers,
            &self.git_root,
            git_host,
            language_registry,
            syntax_styles,
            base_cache,
            id,
        );
    }

    /// Files with staged and with unstaged changes across the repo, as
    /// `(staged, unstaged)`, or `None` before the first diff lands and for a
    /// root outside any repo.
    ///
    /// Refreshed by the diff pipeline rather than read on demand, since the
    /// answer costs a full `git status` walk and the status bar asks every
    /// frame.
    pub(crate) fn repo_change_counts(&self) -> Option<(usize, usize)> {
        self.diff.repo_change_counts
    }

    /// Install `diff_map` on `id` as if a job had produced it. See
    /// [`DiffState::install_test`].
    #[cfg(test)]
    pub(crate) fn install_test_diff_map(&mut self, id: BufferId, diff_map: DiffMap) {
        self.diff.install_test(&self.buffers, id, diff_map);
    }

    /// Populate visible git-tracked buffers' diff maps. See
    /// [`DiffState::drive`].
    ///
    /// The visible set is collected here, since which buffers are on screen is
    /// a question about panes and editors rather than about diffs.
    pub(crate) fn drive_diff_jobs(
        &mut self,
        executor: &Executor,
        git_host: &Arc<dyn GitHost>,
        language_registry: &Arc<LanguageRegistry>,
        syntax_styles: &SyntaxStyles,
        base_cache: &BaseHighlightCache,
        redraw_notify: &Arc<Notify>,
    ) {
        let mut visible = std::mem::take(&mut self.visible_buffers);
        self.collect_visible_buffer_ids(&mut visible);

        self.diff.drive(
            &self.buffers,
            &self.git_root,
            &visible,
            executor,
            git_host,
            language_registry,
            syntax_styles,
            base_cache,
            redraw_notify,
        );
        self.visible_buffers = visible;
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
            self.install_parse_output(out, executor, redraw_notify, &mut installed);
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

            // A buffer whose tokens cover only its viewport is not settled,
            // however current its tree is, so the version gate alone would
            // strand it there.
            if self.buffers.syntax_version(buffer_id) == Some(cur_version)
                && !self.partial_token_buffers.contains(&buffer_id)
            {
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

            // Only a parse with nothing to carry from takes the whole-file
            // capture walk, so only that one is worth narrowing.
            //
            // A buffer already owed a full walk is never narrowed again. It has
            // no spans either, so narrowing on that alone would re-run the
            // viewport forever and the rest of the file would never be styled.
            let viewport = (prior_spans.is_none()
                && !self.partial_token_buffers.contains(&buffer_id))
            .then(|| self.visible_rows(buffer_id))
            .flatten();

            // A small buffer parses on the run loop first. The tree-sitter parse
            // honors the deadline and the captures walk after it does not, so
            // what bounds that walk is the size cap for a first parse, which
            // narrows to the viewport anyway, and the abort inside for a parse
            // carrying tokens, which faces the whole rope whenever its
            // narrowed re-query gives up.
            //
            // Landing here rather than a frame later is what the keystroke
            // feels. An abort leaves the prior state untouched, which is the
            // contract that lets the pool attempt below carry on as if this
            // never ran.
            let mut prior = prior;
            let mut prior_map = prior_map;
            if snapshot.visible_text.len() <= INLINE_PARSE_MAX_BYTES {
                let deadline = executor.now() + INLINE_PARSE_BUDGET;
                let inline = parse_buffer_step(
                    buffer_id,
                    snapshot.clone(),
                    &lang,
                    &mut prior,
                    &mut prior_map,
                    prior_spans.as_deref(),
                    prior_anchors.as_ref(),
                    syntax_styles,
                    Some((deadline, executor)),
                    viewport.clone(),
                );
                if let Some(out) = inline {
                    self.install_parse_output(out, executor, redraw_notify, &mut installed);
                    continue;
                }
            }

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
                    viewport,
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

    /// Take one parse's result into the registry and every editor showing its
    /// buffer.
    ///
    /// Shared by the two ways a parse arrives, a completed background job and
    /// one the run loop ran inline, so the two cannot drift into installing
    /// different things.
    fn install_parse_output(
        &mut self,
        out: ParseJobOutput,
        executor: &Executor,
        redraw_notify: &Arc<Notify>,
        installed: &mut Vec<(BufferId, Option<Range<u32>>)>,
    ) {
        installed.push((out.buffer_id, out.changed_token_rows.clone()));
        self.buffers.store_syntax(out.buffer_id, out.syntax);
        self.buffers.store_syntax_map(out.buffer_id, out.syntax_map);

        // A viewport-only walk paints what is on screen but describes
        // nothing beyond it, so its spans are not kept. The next parse
        // would carry everything off screen forward as unchanged when it
        // was never captured. Without them that parse takes the whole-file
        // walk, which is exactly the follow-up this owes.
        if out.captured.is_some() {
            self.partial_token_buffers.insert(out.buffer_id);
        } else {
            self.partial_token_buffers.remove(&out.buffer_id);
            self.buffers
                .store_token_spans(out.buffer_id, out.token_spans.clone());
        }
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

    /// The display rows an editor is currently showing of `buffer_id`.
    ///
    /// The first editor found on it, since a buffer shown twice is rare and
    /// either viewport is a reasonable place to style first. [`None`] when no
    /// editor shows it or none has been laid out yet, which is the preview and
    /// just-opened cases, and leaves the caller to walk the whole file.
    fn visible_rows(&self, buffer_id: BufferId) -> Option<Range<u32>> {
        self.editors
            .values()
            .find(|editor| editor.buffer_id == buffer_id)
            .and_then(|editor| {
                let rows = editor.viewport_rows?;
                Some(editor.scroll_row..editor.scroll_row.saturating_add(rows))
            })
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
            self.enqueue_reindex(executor, index_update_tx, redraw_notify, buffer_id, false);
        }
    }

    /// The buffer's current text paired with the parse tree describing it, or
    /// `None` once the buffer is gone.
    ///
    /// The tree comes back only when its version matches the text's. That is
    /// the common case, since a reindex is asked for after the parse for this
    /// version landed, and extraction then skips re-parsing the file. An edit
    /// since that parse leaves the versions apart, and a tree built from other
    /// text puts every symbol range somewhere else, so that case parses again.
    fn reindex_inputs(&self, buffer_id: BufferId) -> Option<(Rope, Option<Tree>)> {
        let (text, version) = self.buffers.get(buffer_id).map(|shared| {
            let guard = shared.read().expect("buffer poisoned");
            (guard.snapshot.visible_text.clone(), guard.snapshot.version)
        })?;

        let tree = self
            .buffers
            .syntax(buffer_id)
            .filter(|syntax| syntax.version == version)
            .map(|syntax| syntax.tree.clone());

        Some((text, tree))
    }

    /// Spawn a live re-index of `buffer_id` from its current text.
    ///
    /// `persist` writes the shard and manifest entry when the update lands,
    /// which a save asks for and an edit does not. Skips buffers with no file
    /// path, no resolved language, or no text left to read. The spawned job is
    /// stored so it is not cancelled, replacing any prior one for the buffer.
    ///
    /// Drops the buffer's pending edit debounce, since this extract covers the
    /// same text that timer fires to extract.
    pub(crate) fn enqueue_reindex(
        &mut self,
        executor: &Executor,
        index_update_tx: &UnboundedSender<IndexUpdate>,
        redraw_notify: &Arc<Notify>,
        buffer_id: BufferId,
        persist: bool,
    ) {
        self.index_debounce.remove(&buffer_id);

        let Some(path) = self.buffers.path_for(buffer_id).map(|p| p.to_path_buf()) else {
            return;
        };
        let Some(language) = self.buffers.language_for(buffer_id) else {
            return;
        };
        let Some((text, tree)) = self.reindex_inputs(buffer_id) else {
            return;
        };
        let target = ReindexTarget {
            git_root: self.git_root.clone(),
            workspace: self.id,
            language,
            path,
            text,
            tree,
            persist,
        };
        let task = reindex_buffer(
            executor,
            index_update_tx.clone(),
            redraw_notify.clone(),
            target,
        );
        self.index_jobs.insert(buffer_id, task);
    }

    /// A copy of the diff memo for a scan that runs off the run loop.
    ///
    /// The scan decides per file whether the ranges it already has still hold.
    /// It runs with no borrow on the workspace, so it carries its own copy of
    /// what the memo knew when it started.
    pub(crate) fn changed_ranges_memo_snapshot(&self) -> ChangedRangesMemo {
        self.changed_ranges_memo.clone()
    }

    /// Replace [`Self::changed_ranges`] with what `scan` found, and take on the
    /// entries it had to diff.
    ///
    /// A scan armed before an edit lands after it, and installs ranges measured
    /// against text the tree has moved past. The next scan corrects them, which
    /// is the same staleness the synchronous path had against any change made
    /// during the scan itself.
    pub(crate) fn install_changed_ranges(&mut self, scan: ChangedRangesScan) {
        self.changed_ranges = scan.ranges;
        for (fid, base_hash, buffer_hash, ranges) in scan.computed {
            #[cfg(test)]
            {
                self.changed_ranges_recomputes += 1;
            }
            self.changed_ranges_memo
                .insert(fid, (base_hash, buffer_hash, ranges));
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

#[cfg(test)]
mod tests {
    use super::{ParseJob, Workspace, INLINE_PARSE_MAX_BYTES};
    use crate::{buffer::BufferId, pane::View, test_harness::TestHarness};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_language::LanguageRegistry;
    use stoat_scheduler::{Task, TestScheduler};
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

    /// A buffer under the byte cap parses on the run loop, so its highlights
    /// are there on the frame the edit landed rather than the one after.
    ///
    /// Driving once without ticking the scheduler is what separates the two
    /// paths. A spawned job cannot have run yet, so a buffer settled at this
    /// point can only have parsed inline.
    #[test]
    fn a_small_buffer_parses_on_the_run_loop() {
        let (mut h, id) = harness_with_file("/small", "small.rs", b"fn f() {}\n");
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        assert!(
            !ws.parse_jobs.contains_key(&id),
            "a buffer parsed inline spawns no background job"
        );
        assert!(
            ws.buffers.syntax_version(id).is_some(),
            "and its highlights are already installed"
        );
    }

    /// An edit that restyles most of the file leaves the narrowed re-query with
    /// nothing to save, and the walk that answers instead covers the whole rope
    /// under no clock at all. Under the cap that walk is still a stall the
    /// keystroke feels, so the parse goes to the pool rather than run it inline.
    #[test]
    fn an_inline_parse_whose_requery_gives_up_goes_to_the_pool() {
        let source = "fn f() { let a = 1; }\n".repeat(200);
        let (mut h, id) = harness_with_file("/requery", "wide.rs", source.as_bytes());
        h.stoat.drive_background();
        let ws = h.stoat.active_workspace();
        assert!(
            ws.buffers.syntax_version(id).is_some() && !ws.partial_token_buffers.contains(&id),
            "the file is styled whole, so the next parse has tokens to carry",
        );

        // Rewriting the last three quarters of the file, which is what a paste
        // over a wide selection does. The re-query's covers then reach past
        // half the rope and it gives up rather than narrow anything.
        //
        // Written through the registry rather than through the harness, whose
        // edit helper renders a frame and drives the parse with it.
        {
            let rewritten = "fn g() { let b = 2; }\n".repeat(150);
            let buffer = h.stoat.active_workspace().buffers.get(id).expect("buffer");
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(source.len() / 4..source.len(), &rewritten);
        }
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        assert!(
            ws.parse_jobs.contains_key(&id),
            "the parse it cannot narrow spawns a background job",
        );

        // The scheduler tick resolves the blocking task, and the second drive
        // is the poll that moves its output into the registry.
        h.settle();
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        assert!(
            !ws.parse_jobs.contains_key(&id),
            "the finished job leaves the map",
        );
        assert!(
            ws.buffers.syntax_version(id).is_some(),
            "and the tokens land through the job poll",
        );
    }

    /// Past the cap the captures walk is too long to sit inside a frame, so the
    /// work goes to the pool and lands through the job poll as before.
    #[test]
    fn a_large_buffer_still_parses_off_the_run_loop() {
        let source = "fn f() {}\n".repeat(INLINE_PARSE_MAX_BYTES / 10 + 1);
        assert!(
            source.len() > INLINE_PARSE_MAX_BYTES,
            "the fixture must exceed the cap",
        );
        let (mut h, id) = harness_with_file("/large", "large.rs", source.as_bytes());

        // Driven once without ticking the scheduler, so a spawned job stays
        // pending and observable.
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        assert!(
            ws.parse_jobs.contains_key(&id),
            "a buffer past the cap spawns a background parse job"
        );
        assert!(
            ws.buffers.syntax_version(id).is_none(),
            "and is not parsed on the run loop"
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

    /// Open `name` under `root` with `contents` and return the harness beside
    /// the buffer id it opened.
    fn harness_with_file(root: &str, name: &str, contents: &[u8]) -> (TestHarness, BufferId) {
        use crate::action_handlers::dispatch;
        use stoat_action::OpenFile;

        let mut h = TestHarness::with_size(24, 4);
        let root = PathBuf::from(root);
        h.fake_fs().insert_file(root.join(name), contents);
        h.stoat.active_workspace_mut().git_root = root.clone();

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join(name),
            },
        );

        let id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&root.join(name))
            .expect("the buffer opened");
        (h, id)
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
