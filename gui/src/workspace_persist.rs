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
    project_tree::ProjectTree,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    ops::Range,
    path::{Path, PathBuf},
};
use stoat::{
    buffer_registry::BufferRegistrySnapshot,
    pane::{PaneId, PaneTree as InnerPaneTree},
    workspace::WorkspaceUid,
};
use stoat_text::Point;

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
    /// Recently confirmed command-palette queries, oldest first. Empty
    /// for snapshots written before query history was persisted.
    #[serde(default)]
    pub command_palette_history: VecDeque<String>,
}

/// V1 dock snapshot: position, current visibility (open width /
/// minimized / hidden), default open width, and the file path of
/// the hosted editor when the dock holds one.
#[derive(Debug, Serialize, Deserialize)]
pub struct DockSnapV1 {
    pub side: DockSide,
    pub visibility: DockVisibility,
    /// Stable serialized key for the dock's default open extent
    /// (`Dock::default_extent`). Retains the `default_width` name so
    /// snapshots written before [`DockSide::Bottom`] still deserialize.
    pub default_width: u16,
    /// `Some` when the dock's item is an [`Editor`] with a file
    /// path; `None` for editors without a path or non-editor items.
    /// Non-`None` entries rebuild the editor via
    /// `Workspace::build_editor_for_path` on restore.
    pub editor_path: Option<PathBuf>,
    /// `Some` when the dock hosts a [`ProjectTree`]; carries the
    /// expanded-directory set so the tree restores with the same
    /// directories open. Mutually exclusive with `editor_path`.
    #[serde(default)]
    pub project_tree: Option<ProjectTreeSnapV1>,
}

/// V1 project tree dock payload: the set of directory paths that
/// were expanded when the workspace was saved.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectTreeSnapV1 {
    pub expanded: Vec<PathBuf>,
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
    /// Minimap visibility for the pane, taken from its active editor.
    /// Defaults to `true` so workspaces saved before minimaps were
    /// persisted restore with the overview column shown.
    #[serde(default = "default_true")]
    pub minimap_visible: bool,
}

fn default_true() -> bool {
    true
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

/// Parse the persisted fold ranges from an editor item's serialized
/// blob. Each entry is `[start_row, start_col, end_row, end_col]`;
/// missing or malformed entries are skipped, so a blob written before
/// fold persistence restores with no folds.
pub(crate) fn folds_from_blob(blob: &serde_json::Value) -> Vec<Range<Point>> {
    let Some(entries) = blob.get("folds").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let nums = entry.as_array()?;
            let at = |i: usize| nums.get(i)?.as_u64().map(|n| n as u32);
            Some(Point::new(at(0)?, at(1)?)..Point::new(at(2)?, at(3)?))
        })
        .collect()
}

/// Walk every item in `pane` and record its kind + serialized
/// blob. Pane order is preserved so the restore path adds items
/// in the same sequence; the `active_index` carries over from the
/// pane's own active-item counter. `minimap_visible` is read from
/// the active item when it is an [`Editor`], else defaults to
/// `true`.
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
    let minimap_visible = pane
        .active_item()
        .and_then(|item| item.to_any_view().downcast::<Editor>().ok())
        .map(|editor| editor.read(cx).minimap_visible())
        .unwrap_or(true);
    PaneItemsV1 {
        items,
        active_index: pane.active_index(),
        minimap_visible,
    }
}

/// Snapshot one dock for workspace persistence. Captures
/// position + visibility + default width unconditionally; the
/// hosted editor's file path lands in `editor_path` when the
/// dock's item is an [`Editor`] with a path, otherwise the field
/// is `None` and a tracing line records why.
pub(crate) fn snapshot_dock(dock: &crate::dock::Dock, cx: &gpui::App, index: usize) -> DockSnapV1 {
    let mut editor_path = None;
    let mut project_tree = None;
    match dock.item().to_any_view().downcast::<Editor>() {
        Ok(editor) => {
            editor_path = editor.read(cx).file_path().map(Path::to_path_buf);
            if editor_path.is_none() {
                tracing::info!(
                    dock_index = index,
                    "skipping editor with no file_path in dock persistence v1"
                );
            }
        },
        Err(any) => match any.downcast::<ProjectTree>() {
            Ok(tree) => {
                project_tree = Some(ProjectTreeSnapV1 {
                    expanded: tree.read(cx).expanded_paths(),
                });
            },
            Err(_) => {
                tracing::info!(
                    dock_index = index,
                    "skipping non-editor item in dock persistence v1"
                );
            },
        },
    }
    DockSnapV1 {
        side: dock.side(),
        visibility: dock.visibility(),
        default_width: dock.default_extent(),
        editor_path,
        project_tree,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_from_blob_parses_point_ranges() {
        let blob = serde_json::json!({ "folds": [[0, 11, 2, 0], [5, 3, 9, 0]] });
        assert_eq!(
            folds_from_blob(&blob),
            vec![
                Point::new(0, 11)..Point::new(2, 0),
                Point::new(5, 3)..Point::new(9, 0),
            ]
        );
    }

    #[test]
    fn folds_from_blob_missing_or_malformed_is_empty() {
        assert!(folds_from_blob(&serde_json::json!({ "file_path": "/x" })).is_empty());
        assert!(folds_from_blob(&serde_json::json!({ "folds": [[0, 1, 2]] })).is_empty());
    }
}
