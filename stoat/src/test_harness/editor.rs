use crate::{
    action_handlers,
    app::Stoat,
    editor_state::{EditorId, EditorState},
    pane::PaneId,
    selection::SelectionsCollection,
    View,
};
use std::path::PathBuf;
use stoat_text::{cursor_offset, Bias, Point, SelectionGoal};

/// Append `text` at offset 0 in the focused editor's buffer, then re-seed the
/// cursor as a fresh 1-wide block over the first character. Panics if the
/// focused pane is not an editor.
///
/// The editor was constructed over an empty buffer, so its seed anchor slid to
/// the end when `text` was inserted. Re-seeding restores the same
/// start-of-buffer cursor a real content-open would produce.
pub(crate) fn seed_focused_buffer(stoat: &mut Stoat, text: &str) {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer exists");
        let mut guard = buffer.write().expect("buffer poisoned");
        let len = guard.snapshot.visible_text.len();
        guard.edit(0..len, text);
    }
    let editor = &mut ws.editors[editor_id];
    let snapshot = editor.display_map.snapshot();
    editor.selections.seed_cursor(snapshot.buffer_snapshot());
}

/// Block-cursor cell (via [`cursor_offset`]) for each selection in the focused
/// editor.
///
/// Under the min-width-1 model a forward selection's head sits one cell past
/// the block cursor, so this reports the cursor cell -- the position a test
/// verifies -- rather than the raw head.
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
    let rope = buffer_snapshot.rope();
    editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            cursor_offset(
                rope,
                buffer_snapshot.resolve_anchor(&sel.tail()),
                buffer_snapshot.resolve_anchor(&sel.head()),
            )
        })
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

/// Block-cursor cell (via [`cursor_offset`]) of the primary (newest) selection
/// in the focused editor.
///
/// Reports the cursor cell rather than the raw head, which under the
/// min-width-1 model sits one cell past it for a forward selection.
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
    let rope = buffer_snapshot.rope();
    cursor_offset(
        rope,
        buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().tail()),
        buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().head()),
    )
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
    let rope = buffer_snapshot.rope();
    editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let cursor = cursor_offset(
                rope,
                buffer_snapshot.resolve_anchor(&sel.tail()),
                buffer_snapshot.resolve_anchor(&sel.head()),
            );
            let point = rope.offset_to_point(cursor);
            let display = snapshot.buffer_to_display(point);
            (display.row, display.column)
        })
        .collect()
}

/// Buffer-space `(row, column)` of every cursor, in selection order.
///
/// The buffer-line analogue of [`cursor_display_positions`], for asserting
/// motions that must land by buffer line regardless of soft wrap.
pub(crate) fn cursor_buffer_positions(stoat: &mut Stoat) -> Vec<(u32, u32)> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let editor = ws.editors.get_mut(editor_id).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let cursor = cursor_offset(
                rope,
                buffer_snapshot.resolve_anchor(&sel.tail()),
                buffer_snapshot.resolve_anchor(&sel.head()),
            );
            let point = rope.offset_to_point(cursor);
            (point.row, point.column)
        })
        .collect()
}

/// Replace `editor`'s selections with one collapsed cursor at the buffer point
/// `(row, col)`.
///
/// Drives view tests that need the cursor at a known buffer position without
/// typing motions to get it there.
pub(crate) fn place_cursor(editor: &mut EditorState, row: u32, col: u32) {
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let offset = buffer_snapshot.rope().point_to_offset(Point::new(row, col));
    let anchor = buffer_snapshot.anchor_at(offset, Bias::Left);
    editor.selections = SelectionsCollection::new();
    editor
        .selections
        .insert_cursor(anchor, SelectionGoal::None, buffer_snapshot);
}

/// Path of the file backing the focused editor's buffer.
///
/// Panics if the buffer has no path, so a test that expects a jump to have
/// landed in a file fails on the jump rather than on a `None`.
pub(crate) fn focused_buffer_path(stoat: &Stoat) -> PathBuf {
    let ws = stoat.active_workspace();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    ws.buffers
        .path_for(buffer_id)
        .expect("buffer has a path")
        .to_path_buf()
}

/// Buffer row of the focused editor's primary selection head.
///
/// The raw head rather than the block-cursor cell, for tests asserting which
/// line a jump or a scroll landed the selection on.
pub(crate) fn focused_head_row(stoat: &mut Stoat) -> u32 {
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let head = editor.selections.newest_anchor().head();
    let offset = buffer_snapshot.resolve_anchor(&head);
    buffer_snapshot.rope().offset_to_point(offset).row
}

/// Buffer point of the focused editor's block cursor.
///
/// The single-cursor counterpart to [`cursor_buffer_positions`], for the common
/// case of asserting where one cursor landed.
pub(crate) fn focused_cursor_point(stoat: &mut Stoat) -> Point {
    let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let tail = buffer_snapshot.resolve_anchor(&sel.tail());
    let head = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(buffer_snapshot.rope(), tail, head);
    buffer_snapshot.rope().offset_to_point(cursor)
}
