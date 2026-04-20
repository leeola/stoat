//! Per-workspace session state persistence.
//!
//! On quit, the active workspace is serialized to
//! `<stoat_log::workspace_state_dir()>/<git_root_hash>.ron`. On next launch at
//! the same git root, the file is read back and the workspace is rehydrated
//! before the first frame renders.
//!
//! Coverage is best-effort: see sibling FIXMEs in `multi_buffer.rs`,
//! `review_session.rs`, `commit_list.rs`, and `claude_chat.rs` for the
//! remaining gaps. Buffer history (dirty content, undo stack, anchor-carrying
//! selections) rehydrates via the op log replay in
//! [`crate::buffer::TextBuffer::from_history`]. Anything referencing a live OS
//! resource (PTY-backed `Run`) is out of scope by design; Claude sessions are
//! out of scope pending a separate design pass.

use crate::{
    buffer_registry::BufferRegistrySnapshot,
    dump::snapshot::ActiveRebaseSnap,
    editor_state::{EditorId, EditorState, EditorStateSnapshot},
    pane::{DockId, DockPanel, FocusTarget, PaneTree, View},
    rebase::RebaseState,
    workspace::Workspace,
};
use serde::{Deserialize, Serialize};
use slotmap::SlotMap;
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
};
use stoat_scheduler::Executor;

/// Versioned on-disk representation of a [`Workspace`]. Fields not covered
/// by this struct are regenerated from defaults on load.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkspaceStateV1 {
    pub git_root: PathBuf,
    pub panes: PaneTree,
    pub docks: SlotMap<DockId, DockPanel>,
    pub focus: FocusTarget,
    /// Saved with their pre-shutdown [`EditorId`]s; on load the ids are
    /// remapped to fresh keys in the rehydrated slotmap and pane/dock
    /// `View::Editor` references are rewritten to match.
    pub editors: Vec<(EditorId, EditorStateSnapshot)>,
    pub buffers: BufferRegistrySnapshot,
    pub rebase: Option<RebaseState>,
    pub rebase_active: Option<ActiveRebaseSnap>,
}

/// Resolve the on-disk state file path for a given git root.
/// The canonical form of `git_root` is hashed with the stdlib's
/// [`DefaultHasher`] (stable within a Rust release; acceptable here because
/// a hash mismatch just falls back to a fresh session).
pub(crate) fn state_path_for(git_root: &Path) -> io::Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let canon = fs::canonicalize(git_root).unwrap_or_else(|_| git_root.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut hasher);
    let name = format!("{:016x}.ron", hasher.finish());
    Ok(stoat_log::workspace_state_dir()?.join(name))
}

impl Workspace {
    /// Build a serializable snapshot of everything the workspace currently
    /// knows how to round-trip.
    pub(crate) fn to_state(&self) -> WorkspaceStateV1 {
        let editors: Vec<(EditorId, EditorStateSnapshot)> = self
            .editors
            .iter()
            .map(|(id, state)| (id, state.snapshot()))
            .collect();

        let rebase_active = self
            .rebase_active
            .as_ref()
            .map(|active| ActiveRebaseSnap::from_active(active).snap);

        WorkspaceStateV1 {
            git_root: self.git_root.clone(),
            panes: clone_pane_tree(&self.panes),
            docks: clone_docks(&self.docks),
            focus: self.focus,
            editors,
            buffers: self.buffers.snapshot(),
            rebase: self.rebase.clone(),
            rebase_active,
        }
    }

    /// Serialize the current workspace state to RON and write it atomically
    /// to `path`. Parent directory is created if missing.
    pub(crate) fn save_state(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let state = self.to_state();
        let body = ron::ser::to_string_pretty(&state, ron::ser::PrettyConfig::default())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = path.with_extension("ron.tmp");
        fs::write(&tmp, body)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Replace `self` with the persisted state at `path`. Returns an error if
    /// the file cannot be read or parsed; the caller is expected to log and
    /// continue with the default state rather than abort startup.
    pub(crate) fn restore_state(&mut self, path: &Path, executor: &Executor) -> io::Result<()> {
        let body = fs::read_to_string(path)?;
        let state: WorkspaceStateV1 = ron::from_str(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.apply_state(state, executor);
        Ok(())
    }

    fn apply_state(&mut self, state: WorkspaceStateV1, executor: &Executor) {
        self.buffers.restore_from(state.buffers);

        let mut editors: SlotMap<EditorId, EditorState> = SlotMap::with_key();
        let mut editor_id_map: HashMap<EditorId, EditorId> = HashMap::new();
        for (old_id, snap) in state.editors {
            let Some(buffer) = self.buffers.get(snap.buffer_id) else {
                tracing::warn!(
                    buffer_id = ?snap.buffer_id,
                    "buffer missing after restore; dropping editor"
                );
                continue;
            };
            let new_id = editors.insert(EditorState::restore(snap, buffer, executor.clone()));
            editor_id_map.insert(old_id, new_id);
        }

        let mut panes = state.panes;
        remap_editor_views_in_panes(&mut panes, &editor_id_map);
        sweep_stale_views_in_panes(&mut panes);

        let mut docks = state.docks;
        remap_editor_views_in_docks(&mut docks, &editor_id_map);
        sweep_stale_views_in_docks(&mut docks);

        let focus = match state.focus {
            FocusTarget::Dock(id) if !docks.contains_key(id) => {
                FocusTarget::SplitPane(panes.focus())
            },
            other => other,
        };

        self.panes = panes;
        self.docks = docks;
        self.focus = focus;
        self.editors = editors;
        self.git_root = state.git_root;
        self.rebase = state.rebase;
        self.rebase_active = state.rebase_active.map(ActiveRebaseSnap::into_active);
    }
}

fn remap_editor_views_in_panes(panes: &mut PaneTree, remap: &HashMap<EditorId, EditorId>) {
    for id in panes.split_pane_ids() {
        if let View::Editor(old) = panes.pane(id).view {
            if let Some(&new) = remap.get(&old) {
                panes.pane_mut(id).view = View::Editor(new);
            }
        }
    }
}

fn remap_editor_views_in_docks(
    docks: &mut SlotMap<DockId, DockPanel>,
    remap: &HashMap<EditorId, EditorId>,
) {
    for dock in docks.values_mut() {
        if let View::Editor(old) = dock.view {
            if let Some(&new) = remap.get(&old) {
                dock.view = View::Editor(new);
            }
        }
    }
}

fn sweep_stale_views_in_panes(panes: &mut PaneTree) {
    for id in panes.split_pane_ids() {
        let replacement = stale_replacement(&panes.pane(id).view);
        if let Some(view) = replacement {
            panes.pane_mut(id).view = view;
        }
    }
}

fn sweep_stale_views_in_docks(docks: &mut SlotMap<DockId, DockPanel>) {
    for dock in docks.values_mut() {
        if let Some(view) = stale_replacement(&dock.view) {
            dock.view = view;
        }
    }
}

fn stale_replacement(view: &View) -> Option<View> {
    match view {
        View::Run(_) => Some(View::Label("Terminal (closed)".into())),
        View::Claude(_) => Some(View::Label("Claude session (closed)".into())),
        View::Label(_) | View::Editor(_) => None,
    }
}

/// Workaround for `PaneTree` not being `Clone`: deserialize a freshly
/// serialized copy. Only used on the save path where cloning is cheap and
/// avoids leaking layout internals out of `pane.rs`.
fn clone_pane_tree(tree: &PaneTree) -> PaneTree {
    let body = ron::ser::to_string(tree).expect("pane tree is always serializable");
    ron::from_str(&body).expect("pane tree round-trips through its own serde impl")
}

fn clone_docks(docks: &SlotMap<DockId, DockPanel>) -> SlotMap<DockId, DockPanel> {
    let body = ron::ser::to_string(docks).expect("dock slotmap is always serializable");
    ron::from_str(&body).expect("dock slotmap round-trips through serde")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{Axis, DockSide, DockVisibility, Placement};
    use std::sync::Arc;
    use stoat_scheduler::TestScheduler;
    use tempfile::TempDir;

    fn executor() -> Executor {
        Arc::new(TestScheduler::new()).executor()
    }

    fn write_file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("write fixture");
        path
    }

    fn new_laid_out_workspace(git_root: PathBuf, exec: &Executor) -> Workspace {
        let mut ws = Workspace::new(git_root, exec);
        ws.layout(ratatui::layout::Rect::new(0, 0, 120, 40));
        ws
    }

    #[test]
    fn round_trip_preserves_pane_tree_and_focus() {
        let tmp = TempDir::new().unwrap();
        let file_a = write_file(tmp.path(), "a.txt", "alpha\n");
        let file_b = write_file(tmp.path(), "b.txt", "beta\n");
        let exec = executor();

        let mut ws = new_laid_out_workspace(tmp.path().to_path_buf(), &exec);
        let (id_a, buf_a) = ws.buffers.open(&file_a, "alpha\n");
        let (id_b, buf_b) = ws.buffers.open(&file_b, "beta\n");
        let editor_a = ws
            .editors
            .insert(EditorState::new(id_a, buf_a, exec.clone()));
        let editor_b = ws
            .editors
            .insert(EditorState::new(id_b, buf_b, exec.clone()));
        ws.editors[editor_a].scroll_row = 7;
        ws.editors[editor_b].scroll_row = 3;

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_a);
        let right = ws.panes.split(Axis::Vertical);
        ws.panes.pane_mut(right).view = View::Editor(editor_b);
        ws.focus = FocusTarget::SplitPane(right);

        let state_path = tmp.path().join("state.ron");
        ws.save_state(&state_path).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec);
        fresh.restore_state(&state_path, &exec).unwrap();

        assert_eq!(fresh.git_root, tmp.path());
        assert_eq!(fresh.panes.pane_count(), 2);
        // Three editors: the two file-backed editors we inserted plus the
        // scratch-buffer editor that `Workspace::new` creates by default. The
        // scratch editor is preserved because its buffer's op log round-trips.
        assert_eq!(fresh.editors.len(), 3);

        let FocusTarget::SplitPane(focused) = fresh.focus else {
            panic!("focus should be a split pane");
        };
        let View::Editor(focused_editor) = fresh.panes.pane(focused).view else {
            panic!("focused pane should host an editor");
        };
        assert_eq!(fresh.editors[focused_editor].scroll_row, 3);

        let mut scrolls: Vec<u32> = fresh.editors.values().map(|e| e.scroll_row).collect();
        scrolls.sort();
        assert_eq!(scrolls, vec![0, 3, 7]);

        let mut path_backed: Vec<PathBuf> = fresh
            .editors
            .values()
            .filter_map(|e| fresh.buffers.path_for(e.buffer_id).map(|p| p.to_path_buf()))
            .collect();
        path_backed.sort();
        let mut expected = vec![file_a.clone(), file_b.clone()];
        expected.sort();
        assert_eq!(path_backed, expected);
    }

    #[test]
    fn stale_run_and_claude_views_collapse_to_labels() {
        use crate::{host::ClaudeSessionId, run::RunId};
        let tmp = TempDir::new().unwrap();
        let exec = executor();

        let mut ws = new_laid_out_workspace(tmp.path().to_path_buf(), &exec);
        let stale_run = RunId::default();
        let stale_chat = ClaudeSessionId::default();

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Run(stale_run);
        let second = ws.panes.split(Axis::Vertical);
        ws.panes.pane_mut(second).view = View::Claude(stale_chat);

        let dock_id = ws.docks.insert(DockPanel {
            view: View::Run(stale_run),
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 40 },
            default_width: 40,
            area: Default::default(),
        });
        let _ = dock_id;

        let state_path = tmp.path().join("state.ron");
        ws.save_state(&state_path).unwrap();

        let mut fresh = Workspace::new(tmp.path().to_path_buf(), &exec);
        fresh.restore_state(&state_path, &exec).unwrap();

        for id in fresh.panes.split_pane_ids() {
            match &fresh.panes.pane(id).view {
                View::Label(s) => {
                    assert!(
                        s.contains("closed"),
                        "expected placeholder label, got {s:?}"
                    );
                },
                other => panic!("stale view should become a label, got {other:?}"),
            }
        }
        for dock in fresh.docks.values() {
            assert!(matches!(&dock.view, View::Label(_)));
        }
    }

    #[test]
    fn placement_and_layout_rebuild_on_restore() {
        let tmp = TempDir::new().unwrap();
        let exec = executor();

        let mut ws = new_laid_out_workspace(tmp.path().to_path_buf(), &exec);
        ws.panes.split(Axis::Horizontal);
        ws.panes.split(Axis::Vertical);
        let count_before = ws.panes.pane_count();

        let state_path = tmp.path().join("state.ron");
        ws.save_state(&state_path).unwrap();

        let mut fresh = Workspace::new(tmp.path().to_path_buf(), &exec);
        fresh.restore_state(&state_path, &exec).unwrap();

        assert_eq!(fresh.panes.pane_count(), count_before);
        for id in fresh.panes.split_pane_ids() {
            assert_eq!(fresh.panes.pane(id).placement, Placement::Split);
        }
    }

    fn buffer_text(ws: &Workspace, id: crate::buffer::BufferId) -> String {
        let buffer = ws.buffers.get(id).expect("buffer missing");
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().to_string()
    }

    fn buffer_is_dirty(ws: &Workspace, id: crate::buffer::BufferId) -> bool {
        let buffer = ws.buffers.get(id).expect("buffer missing");
        let guard = buffer.read().expect("buffer poisoned");
        guard.dirty
    }

    #[test]
    fn dirty_buffer_content_round_trips() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "scratch.txt", "hello\n");
        let exec = executor();

        let mut ws = new_laid_out_workspace(tmp.path().to_path_buf(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "hello\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(5..5, ", world");
            guard.edit(12..12, "!");
        }
        let expected_text = buffer_text(&ws, id);
        assert_eq!(expected_text, "hello, world!\n");
        assert!(buffer_is_dirty(&ws, id));

        let state_path = tmp.path().join("state.ron");
        ws.save_state(&state_path).unwrap();

        let mut fresh = Workspace::new(tmp.path().to_path_buf(), &exec);
        fresh.restore_state(&state_path, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, id), expected_text);
        assert!(buffer_is_dirty(&fresh, id));
    }

    #[test]
    fn undo_stack_survives_restart() {
        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "count.txt", "one\n");
        let exec = executor();

        let mut ws = new_laid_out_workspace(tmp.path().to_path_buf(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "one\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(3..3, " two");
            guard.edit(7..7, " three");
        }
        assert_eq!(buffer_text(&ws, id), "one two three\n");

        let state_path = tmp.path().join("state.ron");
        ws.save_state(&state_path).unwrap();

        let mut fresh = Workspace::new(tmp.path().to_path_buf(), &exec);
        fresh.restore_state(&state_path, &exec).unwrap();

        let restored = fresh.buffers.get(id).expect("buffer missing");
        let mut guard = restored.write().expect("buffer poisoned");
        assert!(guard.undo(), "undo should succeed after restart");
        assert_eq!(guard.rope().to_string(), "one two\n");
        assert!(guard.undo(), "second undo should also succeed");
        assert_eq!(guard.rope().to_string(), "one\n");
    }

    #[test]
    fn selections_round_trip_through_anchors() {
        use crate::{multi_buffer::MultiBuffer, selection::SelectionsCollection};
        use stoat_text::{Bias, SelectionGoal};

        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "code.txt", "abcdefghij\n");
        let exec = executor();

        let mut ws = new_laid_out_workspace(tmp.path().to_path_buf(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "abcdefghij\n");
        let editor_id = ws
            .editors
            .insert(EditorState::new(id, buffer.clone(), exec.clone()));

        let multi = MultiBuffer::singleton(id, buffer.clone());
        let multi_snap = multi.snapshot();
        let mut selections = SelectionsCollection::new();
        selections.insert_cursor(
            multi_snap.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &multi_snap,
        );
        selections.insert_cursor(
            multi_snap.anchor_at(7, Bias::Right),
            SelectionGoal::None,
            &multi_snap,
        );
        ws.editors[editor_id].selections = selections;

        let offsets_before: Vec<usize> = ws.editors[editor_id]
            .selections
            .all_anchors()
            .iter()
            .map(|s| multi_snap.resolve_anchor(&s.start))
            .collect();

        let root = ws.panes.focus();
        ws.panes.pane_mut(root).view = View::Editor(editor_id);

        let state_path = tmp.path().join("state.ron");
        ws.save_state(&state_path).unwrap();

        let mut fresh = Workspace::new(tmp.path().to_path_buf(), &exec);
        fresh.restore_state(&state_path, &exec).unwrap();

        let restored_editor_id = fresh
            .panes
            .split_pane_ids()
            .into_iter()
            .find_map(|pid| match fresh.panes.pane(pid).view {
                View::Editor(eid) => Some(eid),
                _ => None,
            })
            .expect("pane with editor view");
        let restored_bid = fresh.editors[restored_editor_id].buffer_id;
        let restored_buffer = fresh.buffers.get(restored_bid).expect("buffer missing");
        let restored_multi = MultiBuffer::singleton(restored_bid, restored_buffer);
        let restored_snap = restored_multi.snapshot();
        let offsets_after: Vec<usize> = fresh.editors[restored_editor_id]
            .selections
            .all_anchors()
            .iter()
            .map(|s| restored_snap.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets_after, offsets_before);
    }

    #[test]
    fn clean_buffer_has_single_insert_op() {
        use crate::buffer::BufferOp;

        let tmp = TempDir::new().unwrap();
        let file = write_file(tmp.path(), "untouched.txt", "hello world\n");
        let exec = executor();

        let mut ws = new_laid_out_workspace(tmp.path().to_path_buf(), &exec);
        let (id, _) = ws.buffers.open(&file, "hello world\n");
        assert!(!buffer_is_dirty(&ws, id));

        let state_path = tmp.path().join("state.ron");
        ws.save_state(&state_path).unwrap();

        let mut fresh = Workspace::new(tmp.path().to_path_buf(), &exec);
        fresh.restore_state(&state_path, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, id), "hello world\n");
        assert!(!buffer_is_dirty(&fresh, id));

        let buffer = fresh.buffers.get(id).expect("buffer missing");
        let guard = buffer.read().expect("buffer poisoned");
        let history = guard.history();
        assert_eq!(history.ops.len(), 1);
        match &history.ops[0] {
            BufferOp::Edit { old, text } => {
                assert_eq!(*old, 0..0);
                assert_eq!(text, "hello world\n");
            },
            other => panic!("expected single Edit op, got {other:?}"),
        }
    }

    #[test]
    fn scratch_buffer_history_round_trips() {
        let tmp = TempDir::new().unwrap();
        let exec = executor();

        let mut ws = new_laid_out_workspace(tmp.path().to_path_buf(), &exec);
        let (scratch_id, scratch_buf) = ws.buffers.new_scratch();
        {
            let mut guard = scratch_buf.write().expect("buffer poisoned");
            guard.edit(0..0, "notes\n");
            guard.edit(6..6, "more\n");
        }
        assert_eq!(buffer_text(&ws, scratch_id), "notes\nmore\n");

        let state_path = tmp.path().join("state.ron");
        ws.save_state(&state_path).unwrap();

        let mut fresh = Workspace::new(tmp.path().to_path_buf(), &exec);
        fresh.restore_state(&state_path, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, scratch_id), "notes\nmore\n");
        let buffer = fresh.buffers.get(scratch_id).expect("scratch lost");
        let guard = buffer.read().expect("buffer poisoned");
        assert!(guard.dirty);
    }
}
