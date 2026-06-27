mod name;
mod persist;

use crate::{
    agent_session::{AgentId, AgentSession},
    agent_status::AgentStatus,
    app::{parse_buffer_async, parse_buffer_step, ParseJobOutput},
    badge::BadgeTray,
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    code_index::build::{file_id, reindex_buffer, IndexUpdate, ReindexTarget},
    commit_list::CommitListState,
    diff,
    diff_map::DiffMap,
    display_map::syntax_theme::SyntaxStyles,
    editor_state::{EditorId, EditorState},
    host::{FsHost, GitHost},
    pane::{DockId, DockPanel, DockSide, FocusTarget, PaneTree, View},
    rebase::{ActiveRebase, RebaseState},
    render::layout::split_pane_status,
    review::ReviewFileInput,
    review_session::ReviewSession,
    run::{RunId, RunState},
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
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::UNIX_EPOCH,
};
use stoat_language::{structural_diff, LanguageRegistry};
use stoat_scheduler::{Executor, Task};
use stoat_text::{Point, Rope};
use tokio::sync::{mpsc::UnboundedSender, oneshot, Notify};

new_key_type! {
    pub struct WorkspaceId;
}

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
    pub panes: PaneTree,
    pub(crate) docks: SlotMap<DockId, DockPanel>,
    pub(crate) focus: FocusTarget,
    pub(crate) buffers: BufferRegistry,
    pub(crate) editors: SlotMap<EditorId, EditorState>,
    pub(crate) runs: SlotMap<RunId, RunState>,
    pub(crate) agents: SlotMap<AgentId, AgentSession>,
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
            panes,
            docks: SlotMap::with_key(),
            focus: FocusTarget::SplitPane(initial_focus),
            buffers,
            editors,
            runs: SlotMap::with_key(),
            agents: SlotMap::with_key(),
            code_graph: CodeGraph::new(),
            index_generation: 0,
            file_paths: HashMap::new(),
            changed_ranges: HashMap::new(),
            review: None,
            commits: None,
            rebase: None,
            rebase_active: None,
            parse_jobs: HashMap::new(),
            index_jobs: HashMap::new(),
            badges: BadgeTray::new(),
            agent: None,
            editor_bridge_waiters: HashMap::new(),
        }
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
            && self.agents.is_empty()
            && self.docks.is_empty()
            && self.editors.len() == 1
            && self.panes.split_panes().count() == 1
            && self.buffers.only_empty_scratch()
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
                    .to_string()
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
                View::Label(_) | View::Run(_) | View::Agent(_) => {},
            }
        }
        for id in self.buffers.preview_buffer_ids() {
            if !visible.contains(&id) {
                visible.push(id);
            }
        }

        for buffer_id in visible {
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

            let deadline = executor.now() + std::time::Duration::from_millis(1);
            if let Some(out) = parse_buffer_step(
                buffer_id,
                snapshot.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                syntax_styles,
                Some((deadline, executor)),
            ) {
                self.buffers.store_syntax(out.buffer_id, out.syntax);
                self.buffers.store_syntax_map(out.buffer_id, out.syntax_map);
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
                    snapshot.visible_text.to_string(),
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
        text: String,
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
            let ranges = changed_byte_ranges(input);
            if !ranges.is_empty() {
                self.changed_ranges.insert(file_id(&input.rel_path), ranges);
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

        self.fit_agents_to_panes();
    }

    /// Resize every hosted agent's emulator and PTY to its pane's content area,
    /// so an agent reflows whenever the layout that frames it changes.
    ///
    /// Runs on every [`Self::layout`], but [`AgentSession::fit`] skips agents
    /// already at the right size, so a steady layout issues no PTY resizes. The
    /// content area excludes the status row via [`split_pane_status`], matching
    /// the rectangle the renderer composites the emulator into.
    fn fit_agents_to_panes(&mut self) {
        let targets: Vec<(AgentId, u16, u16)> = self
            .panes
            .split_panes()
            .filter_map(|(_, pane)| match pane.view {
                View::Agent(id) => {
                    let (content, _) = split_pane_status(pane.area);
                    Some((id, content.height, content.width))
                },
                _ => None,
            })
            .collect();

        for (id, rows, cols) in targets {
            if let Some(agent) = self.agents.get_mut(id) {
                agent.fit(rows, cols);
            }
        }
    }
}

/// The working-tree byte ranges a file's hunks cover, diffing its HEAD text
/// against its working-tree text.
///
/// Hunk line ranges are converted to byte ranges in the working-tree text
/// so a symbol's byte def-range can be tested for overlap directly.
fn changed_byte_ranges(input: &ReviewFileInput) -> Vec<Range<usize>> {
    let result = match &input.language {
        Some(language) => structural_diff::diff_with_language_or_lines(
            language,
            &input.base_text,
            &input.buffer_text,
        ),
        None => structural_diff::diff(&input.base_text, &input.buffer_text),
    };
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
    use super::changed_byte_ranges;
    use crate::review::ReviewFileInput;
    use std::{path::PathBuf, sync::Arc};

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
}
