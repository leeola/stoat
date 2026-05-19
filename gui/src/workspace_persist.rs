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

use crate::{
    dock::{DockSide, DockVisibility},
    editor::Editor,
    item::ItemKind,
};
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
    /// Per-dock snapshot list ordered by `Workspace::docks`'s
    /// vector order, so left/right pinning round-trips. Non-editor
    /// items drop with a tracing line on save and the restored
    /// dock comes back without an item until non-editor
    /// persistence lands.
    #[serde(default)]
    pub docks: Vec<DockSnapV1>,
    pub buffers: BufferRegistrySnapshot,
}

/// V1 dock snapshot: position, current visibility (open width /
/// minimized / hidden), default open width, and the file path of
/// the hosted editor when the dock holds one.
#[derive(Debug, Serialize, Deserialize)]
pub struct DockSnapV1 {
    pub side: DockSide,
    pub visibility: DockVisibility,
    pub default_width: u16,
    /// `Some` when the dock's item is an [`Editor`] with a file
    /// path; `None` for editors without a path or non-editor items.
    /// Non-`None` entries rebuild the editor via
    /// `Workspace::build_editor_for_path` on restore.
    pub editor_path: Option<PathBuf>,
}

/// Per-pane snapshot: every item recorded with its
/// [`ItemKind`] discriminator + the item's `serialize()` JSON
/// payload. The active-index is preserved as-is from the pane's
/// own counter so restoration can put the right item back on top.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PaneItemsV1 {
    #[serde(default)]
    pub items: Vec<ItemSnap>,
    pub active_index: usize,
}

/// Versioned per-item snapshot: a [`ItemKind`] discriminator and
/// the JSON blob produced by the item's
/// [`crate::item::ItemView::serialize`] impl. The restoration
/// dispatch lives in [`crate::workspace::Workspace::apply_state`]
/// because materializing each kind requires workspace-level state
/// (buffer registry, hosts) that `Context<'_, Self>` does not
/// expose.
#[derive(Debug, Serialize, Deserialize)]
pub struct ItemSnap {
    pub kind: ItemKind,
    #[serde(default)]
    pub blob: serde_json::Value,
}

/// Walk every item in `pane` and record its kind + serialized
/// blob. Pane order is preserved so the restore path adds items
/// in the same sequence; the `active_index` carries over from the
/// pane's own active-item counter.
pub(crate) fn snapshot_pane_items(
    pane: &crate::pane::Pane,
    cx: &gpui::App,
    _pane_id: PaneId,
) -> PaneItemsV1 {
    let items: Vec<ItemSnap> = pane
        .items()
        .iter()
        .map(|item| ItemSnap {
            kind: item.item_kind(cx),
            blob: item.serialize(cx),
        })
        .collect();
    PaneItemsV1 {
        items,
        active_index: pane.active_index(),
    }
}

/// Snapshot one dock for workspace persistence. Captures
/// position + visibility + default width unconditionally; the
/// hosted editor's file path lands in `editor_path` when the
/// dock's item is an [`Editor`] with a path, otherwise the field
/// is `None` and a tracing line records why.
pub(crate) fn snapshot_dock(dock: &crate::dock::Dock, cx: &gpui::App, index: usize) -> DockSnapV1 {
    let any = dock.item().to_any_view();
    let editor_path = match any.downcast::<Editor>() {
        Ok(editor) => {
            let path = editor.read(cx).file_path().map(Path::to_path_buf);
            if path.is_none() {
                tracing::info!(
                    dock_index = index,
                    "skipping editor with no file_path in dock persistence v1"
                );
            }
            path
        },
        Err(_) => {
            tracing::info!(
                dock_index = index,
                "skipping non-editor item in dock persistence v1"
            );
            None
        },
    };
    DockSnapV1 {
        side: dock.side(),
        visibility: dock.visibility(),
        default_width: dock.default_width(),
        editor_path,
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
