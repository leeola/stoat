use crate::{
    app::{parse_buffer_async, parse_buffer_step, ParseJobOutput},
    badge::BadgeTray,
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    display_map::syntax_theme::SyntaxStyles,
    editor_state::{EditorId, EditorState},
    host::ClaudeSessionId,
    pane::{PaneTree, View},
    run::{RunId, RunState},
};
use ratatui::layout::Rect;
use slotmap::{new_key_type, SlotMap};
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};
use stoat_scheduler::{Executor, Task};

new_key_type! {
    pub struct WorkspaceId;
}

/// A self-contained editing context: its own buffers, editors, pane layout, git
/// root, and optional Claude chat. Workspaces are owned by the root [`crate::app::Stoat`]
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
    pub git_root: PathBuf,
    pub claude_chat: Option<ClaudeSessionId>,
    pub panes: PaneTree,
    pub(crate) buffers: BufferRegistry,
    pub(crate) editors: SlotMap<EditorId, EditorState>,
    pub(crate) runs: SlotMap<RunId, RunState>,
    parse_jobs: HashMap<BufferId, ParseJob>,
    pub(crate) badges: BadgeTray,
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
        panes.pane_mut(panes.focus()).view = View::Editor(editor_id);

        Self {
            id: WorkspaceId::default(),
            git_root,
            claude_chat: None,
            panes,
            buffers,
            editors,
            runs: SlotMap::with_key(),
            parse_jobs: HashMap::new(),
            badges: BadgeTray::new(),
        }
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
    pub(crate) fn drive_parse_jobs(&mut self, executor: &Executor, syntax_styles: &SyntaxStyles) {
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
        }

        let mut visible: Vec<BufferId> = Vec::new();
        for (_, pane) in self.panes.split_panes() {
            match pane.view {
                View::Editor(editor_id) => {
                    if let Some(editor) = self.editors.get(editor_id) {
                        if !visible.contains(&editor.buffer_id) {
                            visible.push(editor.buffer_id);
                        }
                    }
                },
                View::Label(_) | View::Run(_) => {},
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

            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1);
            if let Some(out) = parse_buffer_step(
                buffer_id,
                snapshot.clone(),
                &lang,
                &mut prior,
                &mut prior_map,
                syntax_styles,
                Some(deadline),
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
                continue;
            }

            let styles = syntax_styles.clone();
            let task = executor.spawn(parse_buffer_async(
                buffer_id, snapshot, lang, prior, prior_map, styles,
            ));
            self.parse_jobs.insert(
                buffer_id,
                ParseJob {
                    target_version: cur_version,
                    task,
                },
            );
        }
    }
}
