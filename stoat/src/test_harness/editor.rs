use crate::{app::Stoat, View};

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
