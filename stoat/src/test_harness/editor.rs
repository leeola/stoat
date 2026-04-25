use crate::{app::Stoat, editor_state::EditorId, pane::PaneId, View};

/// Append `text` at offset 0 in the focused editor's buffer. Panics
/// if the focused pane is not an editor.
pub(crate) fn seed_focused_buffer(stoat: &mut Stoat, text: &str) {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer exists");
    let mut guard = buffer.write().expect("buffer poisoned");
    guard.edit(0..0, text);
}

/// Resolved byte offsets for each selection's head in the focused editor.
pub(crate) fn head_offsets(stoat: &mut Stoat) -> Vec<usize> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let editor = ws.editors.get_mut(editor_id).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| buffer_snapshot.resolve_anchor(&sel.head()))
        .collect()
}

/// Resolved `(start, end, reversed)` byte offsets for each selection in
/// the focused editor.
pub(crate) fn selection_spans(stoat: &mut Stoat) -> Vec<(usize, usize, bool)> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let editor = ws.editors.get_mut(editor_id).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            (
                buffer_snapshot.resolve_anchor(&sel.start),
                buffer_snapshot.resolve_anchor(&sel.end),
                sel.reversed,
            )
        })
        .collect()
}

/// Resolved byte offset for the primary (newest) selection's head in the
/// focused editor.
pub(crate) fn primary_head_offset(stoat: &mut Stoat) -> usize {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let editor = ws.editors.get_mut(editor_id).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().head())
}

/// `scroll_row` for every editor in the active workspace, in `editors`
/// iteration order. Useful for asserting that no editor scrolled.
pub(crate) fn editor_scroll_rows(stoat: &Stoat) -> Vec<u32> {
    stoat
        .active_workspace()
        .editors
        .iter()
        .map(|(_, e)| e.scroll_row)
        .collect()
}

/// First split-pane in the active workspace whose view is an editor.
/// Panics if no editor pane exists.
pub(crate) fn editor_pane(stoat: &Stoat) -> PaneId {
    stoat
        .active_workspace()
        .panes
        .split_panes()
        .find(|(_, p)| matches!(p.view, View::Editor(_)))
        .map(|(pid, _)| pid)
        .expect("active workspace has no editor pane")
}

/// `EditorId` held by `pane`. Panics if the pane is not an editor.
pub(crate) fn editor_id_in_pane(stoat: &Stoat, pane: PaneId) -> EditorId {
    match stoat.active_workspace().panes.pane(pane).view {
        View::Editor(id) => id,
        _ => panic!("pane {pane:?} is not an editor"),
    }
}

/// `scroll_row` for a specific editor in the active workspace.
pub(crate) fn editor_scroll_row(stoat: &Stoat, editor_id: EditorId) -> u32 {
    stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .expect("editor exists")
        .scroll_row
}

/// Display-grid `(row, column)` for each selection's head in the focused
/// editor.
pub(crate) fn cursor_display_positions(stoat: &mut Stoat) -> Vec<(u32, u32)> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let editor = ws.editors.get_mut(editor_id).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let head = sel.head();
            let point = buffer_snapshot.point_for_anchor(&head);
            let display = snapshot.buffer_to_display(point);
            (display.row, display.column)
        })
        .collect()
}
