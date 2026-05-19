//! Per-workspace session-state persistence for the gpui-backed
//! workspace. Mirrors the TUI's [`stoat::workspace::persist`] shape
//! but adapted to the entity-based pane / item model: each pane's
//! items are heterogeneous `Box<dyn ItemHandle>`, so the persisted
//! state records each pane's editor file paths and active item
//! index rather than a global `EditorId` slotmap.
//!
//! Scope v1: pane tree shape (via [`stoat::pane::PaneTree`]'s
//! existing serde), per-pane editor file paths + active index,
//! focused pane id, buffer registry op-log (so unsaved scratch
//! edits and dirty-buffer history round-trip via
//! [`stoat::buffer::TextBuffer::from_history`]). Non-editor items
//! drop silently with a tracing line; restoring an editor that
//! points at a path missing from the registry rebuilds it as an
//! empty file-backed buffer. Sibling items in the
//! "Workspace persistence" parent track per-feature content
//! (review status, claude scrollback, commit-list selection,
//! multi-buffer excerpts).

use crate::editor::Editor;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use stoat::{
    buffer_registry::BufferRegistrySnapshot,
    pane::{PaneId, PaneTree as InnerPaneTree},
    workspace::WorkspaceUid,
};

/// Versioned on-disk shape of a GUI workspace.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceStateV1 {
    pub uid: WorkspaceUid,
    pub name: String,
    pub git_root: PathBuf,
    /// Inner split-tree shape. Reuses [`stoat::pane::PaneTree`]'s
    /// serde directly; [`pane_items`] supplies the per-pane content
    /// the inner tree's `View::Editor(EditorId)` slot does not
    /// represent in the GUI.
    pub panes: InnerPaneTree,
    pub focused_pane: PaneId,
    pub pane_items: BTreeMap<PaneId, PaneItemsV1>,
    pub buffers: BufferRegistrySnapshot,
}

/// Ordered list of editor file paths in a single pane, plus the
/// index of the active editor (0 when the pane has no editors).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PaneItemsV1 {
    pub editor_paths: Vec<PathBuf>,
    pub active_index: usize,
}

/// Walk every editor in `pane` and collect its file path. Items
/// that are not [`Editor`]s, or editors with no file path, drop
/// with a tracing line so the user can audit what failed to
/// persist.
pub(crate) fn snapshot_pane_items(
    pane: &crate::pane::Pane,
    cx: &gpui::App,
    pane_id: PaneId,
) -> PaneItemsV1 {
    let mut editor_paths = Vec::new();
    let mut active_editor_index: Option<usize> = None;
    for (idx, item) in pane.items().iter().enumerate() {
        let any = item.to_any_view();
        let Ok(editor) = any.downcast::<Editor>() else {
            tracing::info!(
                pane_id = ?pane_id,
                item_index = idx,
                "skipping non-editor item in workspace persistence v1"
            );
            continue;
        };
        let Some(path) = editor.read(cx).file_path() else {
            tracing::info!(
                pane_id = ?pane_id,
                item_index = idx,
                "skipping editor with no file_path in workspace persistence v1"
            );
            continue;
        };
        if idx == pane.active_index() {
            active_editor_index = Some(editor_paths.len());
        }
        editor_paths.push(path.to_path_buf());
    }
    PaneItemsV1 {
        editor_paths,
        active_index: active_editor_index.unwrap_or(0),
    }
}

/// Resolve the per-workspace state file path:
/// `<XDG_STATE_HOME>/stoat/workspaces/<git_root_hash>/<uid>.ron`.
/// Wraps [`stoat::workspace::persist::state_path_for`] for the GUI
/// crate so callers don't reach across modules.
pub fn state_path(
    git_root: &Path,
    uid: WorkspaceUid,
    fs: &dyn stoat::host::FsHost,
) -> std::io::Result<PathBuf> {
    stoat::workspace::persist::state_path_for(git_root, uid, fs)
}

/// List every persisted workspace under `git_root`, newest first
/// by filesystem mtime.
pub fn list_workspace_files(
    git_root: &Path,
    fs: &dyn stoat::host::FsHost,
) -> std::io::Result<Vec<PathBuf>> {
    stoat::workspace::persist::list_workspace_files(git_root, fs)
}
