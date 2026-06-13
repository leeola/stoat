use crate::{
    action_handlers::{focused_editor_mut, movement},
    app::{Stoat, UpdateEffect},
};
use std::path::PathBuf;
use stoat_text::Point;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum MarkRequest {
    Set,
    GotoLine,
    GotoExact,
}

pub(super) fn set_mark(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_mark = Some(MarkRequest::Set);
    UpdateEffect::Redraw
}

pub(super) fn goto_mark(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_mark = Some(MarkRequest::GotoLine);
    UpdateEffect::Redraw
}

pub(super) fn goto_mark_exact(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_mark = Some(MarkRequest::GotoExact);
    UpdateEffect::Redraw
}

/// Apply the consumed-char keypress to the pending [`MarkRequest`].
/// `Set` writes `(focused_buffer_id, ch) -> primary cursor offset`
/// into [`Stoat::marks`]. `GotoLine`/`GotoExact` look that key up;
/// on miss return [`UpdateEffect::None`].
pub(crate) fn execute_mark(stoat: &mut Stoat, request: MarkRequest, ch: char) -> UpdateEffect {
    match request {
        MarkRequest::Set => set_mark_at_cursor(stoat, ch),
        MarkRequest::GotoLine | MarkRequest::GotoExact => goto_stored_mark(stoat, request, ch),
    }
}

fn set_mark_at_cursor(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let head = editor.selections.newest_anchor().head();
    let buffer_id = editor.buffer_id;

    if ch.is_uppercase() {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let offset = buf_snap.resolve_anchor(&head);
        let path = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(|p| p.to_path_buf());
        if let Some(path) = path {
            stoat.global_marks.insert(ch, (path, offset));
            return UpdateEffect::Redraw;
        }
        // Scratch buffer (no path): fall through to buffer-local
        // storage so the mark still works in-session.
    }

    stoat.marks.insert((buffer_id, ch), head);
    UpdateEffect::Redraw
}

fn goto_stored_mark(stoat: &mut Stoat, request: MarkRequest, ch: char) -> UpdateEffect {
    if ch.is_uppercase() {
        if let Some((path, offset)) = stoat.global_marks.get(&ch).cloned() {
            return goto_global(stoat, request, path, offset);
        }
    }

    let Some(buffer_id) = focused_editor_mut(stoat).map(|e| e.buffer_id) else {
        return UpdateEffect::None;
    };
    let Some(&stored_anchor) = stoat.marks.get(&(buffer_id, ch)) else {
        return UpdateEffect::None;
    };
    let editor = focused_editor_mut(stoat).expect("buffer present above");
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let rope = buf_snap.rope();
    let stored_offset = buf_snap.resolve_anchor(&stored_anchor);
    let target = resolve_target(rope, stored_offset, request);
    movement::jump_to_offset(stoat, target)
}

fn goto_global(
    stoat: &mut Stoat,
    request: MarkRequest,
    path: PathBuf,
    stored_offset: usize,
) -> UpdateEffect {
    let already_focused = focused_editor_mut(stoat)
        .map(|e| e.buffer_id)
        .and_then(|id| {
            stoat
                .active_workspace()
                .buffers
                .path_for(id)
                .map(|p| p.to_path_buf())
        })
        .is_some_and(|p| p == path);

    if !already_focused {
        let target = stoat.active_workspace().panes.focus();
        if crate::action_handlers::file::open_file_in_pane(stoat, target, &path).is_none() {
            return UpdateEffect::None;
        }
    }

    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let rope = buf_snap.rope();
    let target = resolve_target(rope, stored_offset, request);
    movement::jump_to_offset(stoat, target)
}

fn resolve_target(rope: &stoat_text::Rope, stored_offset: usize, request: MarkRequest) -> usize {
    match request {
        MarkRequest::GotoExact => stored_offset.min(rope.len()),
        MarkRequest::GotoLine => {
            let clamped = stored_offset.min(rope.len());
            let row = rope.offset_to_point(clamped).row;
            rope.point_to_offset(Point::new(row, 0))
        },
        MarkRequest::Set => unreachable!(),
    }
}
