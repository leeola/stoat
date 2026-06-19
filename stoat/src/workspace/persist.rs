//! Per-workspace session state persistence.
//!
//! Each workspace serializes to
//! `<stoat_log::workspace_state_dir()>/<git_root_hash>/<uid>.ron`. When the
//! user passes `--continue`, the binary scans this directory and rehydrates
//! the most-recently-modified file before the first frame renders; a bare
//! launch skips the load so each new session begins in a fresh workspace.
//! Multiple workspaces per git root coexist as sibling files in the same
//! directory. [`crate::workspace::Workspace::is_fresh`] gates the save side
//! so unused fresh workspaces never write a file at all.
//!
//! Coverage is best-effort: see sibling FIXMEs in `multi_buffer.rs`,
//! `review_session.rs`, `commit_list.rs`, and `claude_chat.rs` for the
//! remaining gaps. Buffer history (dirty content, undo stack, anchor-carrying
//! selections) rehydrates via the op log replay in
//! [`crate::buffer::TextBuffer::from_history`]. Anything referencing a live OS
//! resource (PTY-backed `Run`) is out of scope by design. For Claude, the
//! primary session's protocol UUID round-trips (stored for future
//! resume-on-load wiring); live session rehydration itself is still out of
//! scope pending a separate design pass.

use crate::{
    buffer_registry::BufferRegistrySnapshot,
    dump::snapshot::ActiveRebaseSnap,
    editor_state::{EditorId, EditorState, EditorStateSnapshot},
    host::FsHost,
    pane::{DockId, DockPanel, FocusTarget, PaneTree, View},
    rebase::RebaseState,
    workspace::{Workspace, WorkspaceUid},
};
use serde::{Deserialize, Serialize};
use slotmap::SlotMap;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use stoat_scheduler::Executor;

/// Versioned on-disk representation of a [`Workspace`]. Fields not covered
/// by this struct are regenerated from defaults on load.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkspaceStateV1 {
    /// Stable workspace identifier preserved across saves; doubles as the
    /// on-disk filename. Defaults to zero for pre-field on-disk files; the
    /// loader will treat that as a legacy file (still readable, but its
    /// filename will change the next time it's saved under the new scheme).
    #[serde(default)]
    pub uid: WorkspaceUid,
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
    /// Protocol-level UUID of the workspace's primary Claude session, if one
    /// existed and had received `AgentMessage::Init` at save time. `None`
    /// when no Claude session was active. `#[serde(default)]` keeps
    /// pre-field on-disk files readable.
    #[serde(default)]
    pub claude_session_id: Option<String>,
    /// User-facing display name. Empty string on legacy files that predate
    /// the field; restore regenerates a default from `uid` in that case.
    #[serde(default)]
    pub name: String,
}

/// Resolve the per-git-root directory that holds every workspace persisted
/// against that root. One file per workspace sits in this directory, named
/// by the workspace's [`WorkspaceUid`]. Canonical form of `git_root` is
/// hashed with the stdlib's [`DefaultHasher`] (stable within a Rust release;
/// acceptable here because a hash mismatch just falls back to a fresh session).
pub(crate) fn workspace_dir_for(git_root: &Path, fs: &dyn FsHost) -> io::Result<PathBuf> {
    Ok(anchor_state_dir(
        &stoat_log::workspace_state_dir()?,
        git_root,
        fs,
    ))
}

/// Hash-derived state directory for a single anchor under `state_dir`.
/// Factored so callers (and tests) can supply a custom `state_dir`
/// rather than always going through `stoat_log::workspace_state_dir`.
fn anchor_state_dir(state_dir: &Path, anchor: &Path, fs: &dyn FsHost) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let canon = fs
        .canonicalize(anchor)
        .unwrap_or_else(|_| anchor.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut hasher);
    let name = format!("{:016x}", hasher.finish());
    state_dir.join(name)
}

/// Resolve the on-disk state file path for a specific workspace.
pub(crate) fn state_path_for(
    git_root: &Path,
    uid: WorkspaceUid,
    fs: &dyn FsHost,
) -> io::Result<PathBuf> {
    Ok(workspace_dir_for(git_root, fs)?.join(format!("{uid}.ron")))
}

/// List every persisted workspace file for a git root, newest first by
/// filesystem mtime. Returns an empty vec (not an error) if the directory
/// does not exist.
pub(crate) fn list_workspace_files(git_root: &Path, fs: &dyn FsHost) -> io::Result<Vec<PathBuf>> {
    list_ron_files_by_mtime_desc(&workspace_dir_for(git_root, fs)?, fs)
}

/// Walk ancestors of `cwd` (cwd itself first) for any directory whose
/// workspace state directory contains persisted `.ron` files, and return
/// the ancestor whose newest file has the highest mtime across all
/// candidates. Returns `None` when no ancestor has any persisted state.
///
/// Backs the binary's `--resume` flag: workspaces are tracked per anchor
/// directory, and `--resume` cascades up so a session run from
/// `~/foo/bar/baz/bang` reopens whichever ancestor's state is most
/// recent. cwd-first iteration means a tie at the same mtime resolves
/// to the deepest ancestor, which is the natural "most specific match"
/// when multiple state files were saved at the same instant.
pub fn find_resume_anchor(cwd: &Path, fs: &dyn FsHost) -> io::Result<Option<PathBuf>> {
    let state_dir = stoat_log::workspace_state_dir()?;
    find_resume_anchor_in(&state_dir, cwd, fs)
}

fn find_resume_anchor_in(
    state_dir: &Path,
    cwd: &Path,
    fs: &dyn FsHost,
) -> io::Result<Option<PathBuf>> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for anc in cwd.ancestors() {
        let dir = anchor_state_dir(state_dir, anc, fs);
        if !fs.exists(&dir) {
            continue;
        }
        let mut newest: Option<std::time::SystemTime> = None;
        for entry in fs.list_dir(&dir)? {
            let path = dir.join(entry.name.as_str());
            if path.extension().and_then(|s| s.to_str()) != Some("ron") {
                continue;
            }
            let mtime = fs
                .metadata(&path)
                .ok()
                .flatten()
                .map(|m| m.modified)
                .unwrap_or(UNIX_EPOCH);
            newest = Some(newest.map_or(mtime, |prev| prev.max(mtime)));
        }
        if let Some(mtime) = newest {
            match &best {
                Some((_, prev_mtime)) if *prev_mtime >= mtime => {},
                _ => best = Some((anc.to_path_buf(), mtime)),
            }
        }
    }
    Ok(best.map(|(p, _)| p))
}

/// Underlying directory scan for [`list_workspace_files`]. Factored so tests
/// can exercise it against a tempdir without touching the real XDG path.
/// Entries whose metadata cannot be read are treated as unix-epoch-old so
/// they sort to the bottom rather than dropping out silently.
fn list_ron_files_by_mtime_desc(dir: &Path, fs: &dyn FsHost) -> io::Result<Vec<PathBuf>> {
    if !fs.exists(dir) {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in fs.list_dir(dir)? {
        let path = dir.join(entry.name.as_str());
        if path.extension().and_then(|s| s.to_str()) != Some("ron") {
            continue;
        }
        let mtime = fs
            .metadata(&path)
            .ok()
            .flatten()
            .map(|m| m.modified)
            .unwrap_or(UNIX_EPOCH);
        entries.push((path, mtime));
    }
    entries.sort_by_key(|b| std::cmp::Reverse(b.1));
    Ok(entries.into_iter().map(|(p, _)| p).collect())
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

        let claude_session_id = self
            .claude_chat
            .and_then(|id| self.chats.get(&id))
            .and_then(|chat| chat.protocol_session_id.clone());

        WorkspaceStateV1 {
            uid: self.uid,
            git_root: self.git_root.clone(),
            panes: clone_pane_tree(&self.panes),
            docks: clone_docks(&self.docks),
            focus: self.focus,
            editors,
            buffers: self.buffers.snapshot(),
            rebase: self.rebase.clone(),
            rebase_active,
            claude_session_id,
            name: self.name.clone(),
        }
    }

    /// Serialize the current workspace state to RON and write it atomically
    /// to `path`. Parent directory is created if missing.
    pub(crate) fn save_state(&self, path: &Path, fs: &dyn FsHost) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent)?;
        }
        let state = self.to_state();
        let body = ron::ser::to_string_pretty(&state, ron::ser::PrettyConfig::default())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = path.with_extension("ron.tmp");
        fs.write(&tmp, body.as_bytes())?;
        fs.rename(&tmp, path)?;
        Ok(())
    }

    /// Replace `self` with the persisted state at `path`. Returns an error if
    /// the file cannot be read or parsed; the caller is expected to log and
    /// continue with the default state rather than abort startup.
    pub(crate) fn restore_state(
        &mut self,
        path: &Path,
        fs: &dyn FsHost,
        executor: &Executor,
    ) -> io::Result<()> {
        let mut buf = Vec::new();
        fs.read(path, &mut buf)?;
        let body = String::from_utf8(buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let state: WorkspaceStateV1 = ron::from_str(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.apply_state(state, executor);
        Ok(())
    }

    pub(crate) fn apply_state(&mut self, state: WorkspaceStateV1, executor: &Executor) {
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
        self.uid = state.uid;
        self.git_root = state.git_root;
        self.rebase = state.rebase;
        self.rebase_active = state.rebase_active.map(ActiveRebaseSnap::into_active);
        self.restored_claude_session_id = state.claude_session_id;
        self.name = if state.name.is_empty() {
            super::name::default_workspace_name(state.uid)
        } else {
            state.name
        };
    }
}

fn remap_editor_views_in_panes(panes: &mut PaneTree, remap: &HashMap<EditorId, EditorId>) {
    for id in panes.split_pane_ids() {
        if let View::Editor(old) = panes.pane(id).view
            && let Some(&new) = remap.get(&old)
        {
            panes.pane_mut(id).view = View::Editor(new);
        }
    }
}

fn remap_editor_views_in_docks(
    docks: &mut SlotMap<DockId, DockPanel>,
    remap: &HashMap<EditorId, EditorId>,
) {
    for dock in docks.values_mut() {
        if let View::Editor(old) = dock.view
            && let Some(&new) = remap.get(&old)
        {
            dock.view = View::Editor(new);
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
    use crate::{
        host::FakeFs,
        pane::{Axis, DockSide, DockVisibility, Placement},
    };
    use std::{sync::Arc, time::Duration};
    use stoat_scheduler::TestScheduler;

    fn executor() -> Executor {
        Arc::new(TestScheduler::new()).executor()
    }

    fn new_laid_out_workspace(git_root: PathBuf, exec: &Executor) -> Workspace {
        let mut ws = Workspace::new(git_root, exec);
        ws.layout(ratatui::layout::Rect::new(0, 0, 120, 40));
        ws
    }

    #[test]
    fn round_trip_preserves_pane_tree_and_focus() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file_a = ws_dir.join("a.txt");
        let file_b = ws_dir.join("b.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
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

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(PathBuf::from("/elsewhere"), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.git_root, ws_dir);
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
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
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

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

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
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.panes.split(Axis::Horizontal);
        ws.panes.split(Axis::Vertical);
        let count_before = ws.panes.pane_count();

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.panes.pane_count(), count_before);
        for id in fresh.panes.split_pane_ids() {
            assert_eq!(fresh.panes.pane(id).placement, Placement::Split);
        }
    }

    #[test]
    fn round_trip_preserves_claude_session_id() {
        use crate::{claude_chat::ClaudeChatState, editor_state::EditorId, host::ClaudeSessionId};
        use stoat_text::BufferId;

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let chat_id = ClaudeSessionId::default();
        ws.claude_chat = Some(chat_id);
        ws.chats.insert(
            chat_id,
            ClaudeChatState {
                session_id: chat_id,
                input: crate::input_view::InputView {
                    editor_id: EditorId::default(),
                    buffer_id: BufferId::new(0),
                    target: crate::input_view::SubmitTarget::ClaudeChat,
                    max_height: u16::MAX,
                    start_mode: "prompt",
                },
                messages: Vec::new(),
                streaming_text: None,
                scroll_offset: 0,
                pending_sends: Vec::new(),
                active_since: None,
                protocol_session_id: Some("00000000-0000-4000-8000-000000000abc".into()),
                follow: false,
                focused_tool_id: None,
                expanded_tool_ids: std::collections::HashSet::new(),
                usage: crate::host::TokenUsage::default(),
                cancelled_tool_uses: std::collections::HashSet::new(),
                layout_cache: std::cell::RefCell::default(),
            },
        );

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(
            fresh.restored_claude_session_id,
            Some("00000000-0000-4000-8000-000000000abc".into()),
        );
    }

    #[test]
    fn round_trip_claude_session_id_absent_when_no_chat() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(fresh.restored_claude_session_id, None);
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
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("scratch.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "hello\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(5..5, ", world");
            guard.edit(12..12, "!");
        }
        let expected_text = buffer_text(&ws, id);
        assert_eq!(expected_text, "hello, world!\n");
        assert!(buffer_is_dirty(&ws, id));

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, id), expected_text);
        assert!(buffer_is_dirty(&fresh, id));
    }

    #[test]
    fn undo_stack_survives_restart() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("count.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, buffer) = ws.buffers.open(&file, "one\n");
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(3..3, " two");
            guard.edit(7..7, " three");
        }
        assert_eq!(buffer_text(&ws, id), "one two three\n");

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

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

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("code.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
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

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

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

        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let file = ws_dir.join("untouched.txt");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (id, _) = ws.buffers.open(&file, "hello world\n");
        assert!(!buffer_is_dirty(&ws, id));

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

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
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let (scratch_id, scratch_buf) = ws.buffers.new_scratch();
        {
            let mut guard = scratch_buf.write().expect("buffer poisoned");
            guard.edit(0..0, "notes\n");
            guard.edit(6..6, "more\n");
        }
        assert_eq!(buffer_text(&ws, scratch_id), "notes\nmore\n");

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();

        assert_eq!(buffer_text(&fresh, scratch_id), "notes\nmore\n");
        let buffer = fresh.buffers.get(scratch_id).expect("scratch lost");
        let guard = buffer.read().expect("buffer poisoned");
        assert!(guard.dirty);
    }

    #[test]
    fn list_ron_files_sorts_newest_first() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let older = ws_dir.join("aaaa.ron");
        let newer = ws_dir.join("bbbb.ron");
        fake.insert_file(&older, "old");
        fake.insert_file(&newer, "new");

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed, vec![newer, older]);
    }

    #[test]
    fn list_ron_files_ignores_non_ron_entries() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        fake.insert_file(ws_dir.join("ok.ron"), "");
        fake.insert_file(ws_dir.join("skip.txt"), "");
        fake.insert_dir(ws_dir.join("subdir"));

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed, vec![ws_dir.join("ok.ron")]);
    }

    #[test]
    fn list_ron_files_missing_dir_returns_empty() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let missing = ws_dir.join("nope");
        assert!(list_ron_files_by_mtime_desc(&missing, &fake)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn uid_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let scheduler = Arc::new(TestScheduler::new());
        let exec = scheduler.executor();

        let ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let original_uid = ws.uid;

        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        scheduler.advance_clock(Duration::from_nanos(1));
        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        assert_ne!(fresh.uid, original_uid, "new workspaces get distinct uids");
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.uid, original_uid);
    }

    #[test]
    fn user_name_round_trips_through_save_and_restore() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.name = "my workspace".to_string();
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(fresh.name, "my workspace");
    }

    #[test]
    fn legacy_empty_name_regenerates_default_from_uid() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let mut ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        ws.name = String::new();
        let state_path = ws_dir.join("state.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let mut fresh = Workspace::new(ws_dir.clone(), &exec);
        fresh.restore_state(&state_path, &fake, &exec).unwrap();
        assert_eq!(
            fresh.name,
            crate::workspace::name::default_workspace_name(ws.uid)
        );
    }

    #[test]
    fn multiple_saves_with_same_uid_overwrite() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let exec = executor();

        let ws = new_laid_out_workspace(ws_dir.clone(), &exec);
        let path = ws_dir.join(format!("{}.ron", ws.uid));
        ws.save_state(&path, &fake).unwrap();
        ws.save_state(&path, &fake).unwrap();

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed, vec![path]);
    }

    #[test]
    fn different_uids_sit_side_by_side() {
        let fake = FakeFs::new();
        let ws_dir = PathBuf::from("/test");
        let scheduler = Arc::new(TestScheduler::new());
        let exec = scheduler.executor();

        let ws_a = new_laid_out_workspace(ws_dir.clone(), &exec);
        scheduler.advance_clock(Duration::from_nanos(1));
        let ws_b = new_laid_out_workspace(ws_dir.clone(), &exec);
        assert_ne!(ws_a.uid, ws_b.uid);

        let path_a = ws_dir.join(format!("{}.ron", ws_a.uid));
        let path_b = ws_dir.join(format!("{}.ron", ws_b.uid));
        ws_a.save_state(&path_a, &fake).unwrap();
        ws_b.save_state(&path_b, &fake).unwrap();

        let listed = list_ron_files_by_mtime_desc(&ws_dir, &fake).unwrap();
        assert_eq!(listed.len(), 2);
    }

    fn write_anchor_state(state_dir: &Path, anchor: &Path, fake: &FakeFs, name: &str) -> PathBuf {
        let dir = anchor_state_dir(state_dir, anchor, fake);
        let path = dir.join(name);
        fake.insert_file(&path, "");
        path
    }

    #[test]
    fn find_resume_anchor_no_state_returns_none() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn find_resume_anchor_picks_only_ancestor_with_state() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        let anchor = PathBuf::from("/foo");
        write_anchor_state(&state_dir, &anchor, &fake, "ws.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(anchor));
    }

    #[test]
    fn find_resume_anchor_picks_cwd_when_only_cwd_has_state() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &cwd, &fake, "ws.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(cwd));
    }

    #[test]
    fn find_resume_anchor_prefers_more_recent_anchor() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &PathBuf::from("/foo"), &fake, "old.ron");
        write_anchor_state(&state_dir, &cwd, &fake, "new.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(cwd));
    }

    #[test]
    fn find_resume_anchor_prefers_parent_when_parent_newer() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar/baz");
        write_anchor_state(&state_dir, &cwd, &fake, "old.ron");
        let parent = PathBuf::from("/foo");
        write_anchor_state(&state_dir, &parent, &fake, "new.ron");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(result, Some(parent));
    }

    #[test]
    fn find_resume_anchor_skips_non_ron_files() {
        let fake = FakeFs::new();
        let state_dir = PathBuf::from("/state");
        let cwd = PathBuf::from("/foo/bar");
        let parent = PathBuf::from("/foo");
        let parent_dir = anchor_state_dir(&state_dir, &parent, &fake);
        fake.insert_file(parent_dir.join("notes.txt"), "");
        let result = find_resume_anchor_in(&state_dir, &cwd, &fake).unwrap();
        assert_eq!(
            result, None,
            "non-.ron files in an ancestor's state dir should be ignored"
        );
    }
}
