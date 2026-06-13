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
    item::ItemKind,
};
use gpui::{point, px, size, Bounds, WindowBounds};
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
    /// vector order, so left/right pinning round-trips. Each dock
    /// records its hosted item generically (kind + blob), so every
    /// dockable kind restores through the same dispatch as pane items.
    #[serde(default)]
    pub docks: Vec<DockSnapV1>,
    pub buffers: BufferRegistrySnapshot,
    /// Recently confirmed command-palette queries, oldest first. Empty
    /// for snapshots written before query history was persisted.
    #[serde(default)]
    pub command_palette_history: VecDeque<String>,
    /// Whether the minimap overview column is shown. Workspace-level:
    /// one minimap follows the active editor. Defaults to `true` so
    /// snapshots written before the minimap moved to the workspace
    /// restore with the overview column shown, matching the prior
    /// per-pane default.
    #[serde(default = "default_true")]
    pub minimap_visible: bool,
    /// Window geometry restored before the window opens. `None` for
    /// snapshots written before window bounds were tracked, which reopen
    /// at the centered default.
    #[serde(default)]
    pub window_bounds: Option<WindowBoundsV1>,
}

/// Window placement mode persisted alongside the bounds rect, mirroring
/// gpui's [`WindowBounds`] variants.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum WindowModeV1 {
    Windowed,
    Maximized,
    Fullscreen,
}

/// Persisted window geometry: the placement mode plus the bounds rect in
/// logical pixels (origin + size). Restored into a [`WindowBounds`] before
/// the window opens so a `--continue` session reopens where it was.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowBoundsV1 {
    pub mode: WindowModeV1,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowBoundsV1 {
    /// Capture a gpui [`WindowBounds`] as the persisted shape.
    pub fn from_window_bounds(bounds: WindowBounds) -> Self {
        let (mode, rect) = match bounds {
            WindowBounds::Windowed(rect) => (WindowModeV1::Windowed, rect),
            WindowBounds::Maximized(rect) => (WindowModeV1::Maximized, rect),
            WindowBounds::Fullscreen(rect) => (WindowModeV1::Fullscreen, rect),
        };
        Self {
            mode,
            x: f32::from(rect.origin.x),
            y: f32::from(rect.origin.y),
            width: f32::from(rect.size.width),
            height: f32::from(rect.size.height),
        }
    }

    /// Rebuild the gpui [`WindowBounds`] for `WindowOptions` on restore.
    pub fn to_window_bounds(self) -> WindowBounds {
        let rect = Bounds {
            origin: point(px(self.x), px(self.y)),
            size: size(px(self.width), px(self.height)),
        };
        match self.mode {
            WindowModeV1::Windowed => WindowBounds::Windowed(rect),
            WindowModeV1::Maximized => WindowBounds::Maximized(rect),
            WindowModeV1::Fullscreen => WindowBounds::Fullscreen(rect),
        }
    }
}

/// V1 dock snapshot: position, current visibility (open width /
/// minimized / hidden), default open width, and the hosted item as
/// a generic kind + blob snapshot.
#[derive(Debug, Serialize, Deserialize)]
pub struct DockSnapV1 {
    pub side: DockSide,
    pub visibility: DockVisibility,
    /// Stable serialized key for the dock's default open extent
    /// (`Dock::default_extent`). Retains the `default_width` name so
    /// snapshots written before [`DockSide::Bottom`] still deserialize.
    pub default_width: u16,
    /// The dock's hosted item as a kind + blob snapshot, mirroring the
    /// pane-side [`ItemSnap`] shape so every dockable kind round-trips
    /// through its [`crate::item::ItemView::serialize`] and the shared
    /// restore dispatch. `None` only for snapshots written before docks
    /// recorded their item generically; those fall back to the legacy
    /// fields below.
    #[serde(default)]
    pub item: Option<ItemSnap>,
    /// Legacy field: the hosted editor's file path. Retained
    /// deserialize-only so pre-[`Self::item`] snapshots still load; new
    /// snapshots leave it `None` and carry the editor in [`Self::item`].
    #[serde(default)]
    pub editor_path: Option<PathBuf>,
    /// Legacy field: the hosted project tree's expanded set. Retained
    /// deserialize-only for the same reason as [`Self::editor_path`].
    #[serde(default)]
    pub project_tree: Option<ProjectTreeSnapV1>,
}

impl DockSnapV1 {
    /// Split a dock snapshot into its metadata and hosted item.
    ///
    /// Prefers the generic [`Self::item`] snapshot and falls back to the
    /// legacy `editor_path` / `project_tree` fields so pre-`item`
    /// snapshots still restore. The [`ItemSnap`] is `None` when the dock
    /// recorded no restorable item.
    pub(crate) fn into_parts(self) -> (DockSide, DockVisibility, u16, Option<ItemSnap>) {
        let DockSnapV1 {
            side,
            visibility,
            default_width,
            item,
            editor_path,
            project_tree,
        } = self;
        let item = item.or_else(|| legacy_dock_item(editor_path, project_tree));
        (side, visibility, default_width, item)
    }
}

/// Map a pre-`item` dock snapshot's legacy fields onto the generic
/// [`ItemSnap`] shape: an `editor_path` becomes an `Editor` blob with a
/// `file_path`, a `project_tree` becomes a `ProjectTree` blob with its
/// `expanded` set. Returns `None` when neither legacy field is set.
fn legacy_dock_item(
    editor_path: Option<PathBuf>,
    project_tree: Option<ProjectTreeSnapV1>,
) -> Option<ItemSnap> {
    if let Some(path) = editor_path {
        return Some(ItemSnap {
            kind: ItemKind::Editor,
            blob: serde_json::json!({ "file_path": path }),
        });
    }
    if let Some(tree) = project_tree {
        return Some(ItemSnap {
            kind: ItemKind::ProjectTree,
            blob: serde_json::json!({ "expanded": tree.expanded }),
        });
    }
    None
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

/// Snapshot one dock for workspace persistence: position, visibility,
/// and default extent, plus the hosted item as a generic [`ItemSnap`]
/// (kind + the item's [`crate::item::ItemView::serialize`] blob), the
/// same shape pane items use. The legacy `editor_path` / `project_tree`
/// fields stay `None`; new snapshots carry the item in `item`.
pub(crate) fn snapshot_dock(dock: &crate::dock::Dock, cx: &gpui::App) -> DockSnapV1 {
    let handle = dock.item();
    DockSnapV1 {
        side: dock.side(),
        visibility: dock.visibility(),
        default_width: dock.default_extent(),
        item: Some(ItemSnap {
            kind: handle.item_kind(cx),
            blob: handle.serialize(cx),
        }),
        editor_path: None,
        project_tree: None,
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

    #[test]
    fn into_parts_maps_legacy_editor_path() {
        let snap = DockSnapV1 {
            side: DockSide::Left,
            visibility: DockVisibility::Minimized,
            default_width: 220,
            item: None,
            editor_path: Some(PathBuf::from("/x/y.rs")),
            project_tree: None,
        };
        let (_, _, _, item) = snap.into_parts();
        let item = item.expect("legacy editor_path yields an item");
        assert_eq!(item.kind, ItemKind::Editor);
        assert_eq!(item.blob, serde_json::json!({ "file_path": "/x/y.rs" }));
    }

    #[test]
    fn into_parts_maps_legacy_project_tree() {
        let snap = DockSnapV1 {
            side: DockSide::Left,
            visibility: DockVisibility::Minimized,
            default_width: 240,
            item: None,
            editor_path: None,
            project_tree: Some(ProjectTreeSnapV1 {
                expanded: vec![PathBuf::from("/a"), PathBuf::from("/a/b")],
            }),
        };
        let (_, _, _, item) = snap.into_parts();
        let item = item.expect("legacy project_tree yields an item");
        assert_eq!(item.kind, ItemKind::ProjectTree);
        assert_eq!(item.blob, serde_json::json!({ "expanded": ["/a", "/a/b"] }));
    }

    #[test]
    fn into_parts_prefers_item_over_legacy_fields() {
        let snap = DockSnapV1 {
            side: DockSide::Right,
            visibility: DockVisibility::Minimized,
            default_width: 320,
            item: Some(ItemSnap {
                kind: ItemKind::Terminal,
                blob: serde_json::json!({ "cwd": "/t" }),
            }),
            editor_path: Some(PathBuf::from("/ignored.rs")),
            project_tree: None,
        };
        let (_, _, _, item) = snap.into_parts();
        assert_eq!(item.expect("item present").kind, ItemKind::Terminal);
    }

    #[test]
    fn window_bounds_records_origin_and_size() {
        let v1 = WindowBoundsV1::from_window_bounds(WindowBounds::Windowed(Bounds {
            origin: point(px(12.0), px(34.0)),
            size: size(px(800.0), px(600.0)),
        }));
        assert_eq!(v1.mode, WindowModeV1::Windowed);
        assert_eq!(
            (v1.x, v1.y, v1.width, v1.height),
            (12.0, 34.0, 800.0, 600.0)
        );
    }

    #[test]
    fn window_bounds_round_trips_every_mode() {
        let rect = Bounds {
            origin: point(px(12.0), px(34.0)),
            size: size(px(800.0), px(600.0)),
        };
        for original in [
            WindowBounds::Windowed(rect),
            WindowBounds::Maximized(rect),
            WindowBounds::Fullscreen(rect),
        ] {
            assert_eq!(
                WindowBoundsV1::from_window_bounds(original).to_window_bounds(),
                original
            );
        }
    }
}
