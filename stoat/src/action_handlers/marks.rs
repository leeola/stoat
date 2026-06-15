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
    if ch.is_uppercase()
        && let Some((path, offset)) = stoat.global_marks.get(&ch).cloned()
    {
        return goto_global(stoat, request, path, offset);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_harness::TestHarness;
    use std::path::PathBuf;
    use stoat_action::{self as action, OpenFile};

    fn seed(h: &mut TestHarness, contents: &str) -> PathBuf {
        let root = PathBuf::from("/marks-test");
        let path = root.join("main.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        buf_snap.resolve_anchor(&head)
    }

    fn focused_path(h: &TestHarness) -> PathBuf {
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let crate::pane::View::Editor(eid) = pane.view else {
            panic!("not an editor");
        };
        let buffer_id = ws.editors.get(eid).expect("editor").buffer_id;
        ws.buffers
            .path_for(buffer_id)
            .expect("buffer path")
            .to_path_buf()
    }

    fn seed_two_files(h: &mut TestHarness) -> (PathBuf, PathBuf) {
        let root = PathBuf::from("/marks-global-test");
        let a = root.join("a.rs");
        let b = root.join("b.rs");
        h.fake_fs()
            .insert_files([(a.clone(), b"abc\ndef\n" as &[u8]), (b.clone(), b"xyz\n")]);
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: a.clone() });
        h.settle();
        (a, b)
    }

    #[test]
    fn set_mark_then_goto_mark_jumps_to_line_start() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\ndef\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveRight);
        assert_eq!(cursor_offset(&mut h), 5);

        crate::action_handlers::dispatch(&mut h.stoat, &action::SetMark);
        h.type_keys("a");
        crate::action_handlers::dispatch(&mut h.stoat, &action::GotoFileStart);
        assert_eq!(cursor_offset(&mut h), 0);

        crate::action_handlers::dispatch(&mut h.stoat, &action::GotoMark);
        h.type_keys("a");
        assert_eq!(cursor_offset(&mut h), 4);
        assert!(h.stoat.pending_mark.is_none());
    }

    #[test]
    fn set_mark_then_goto_mark_exact_jumps_to_offset() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\ndef\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveRight);
        assert_eq!(cursor_offset(&mut h), 5);

        crate::action_handlers::dispatch(&mut h.stoat, &action::SetMark);
        h.type_keys("a");
        crate::action_handlers::dispatch(&mut h.stoat, &action::GotoFileStart);
        assert_eq!(cursor_offset(&mut h), 0);

        crate::action_handlers::dispatch(&mut h.stoat, &action::GotoMarkExact);
        h.type_keys("a");
        assert_eq!(cursor_offset(&mut h), 5);
        assert!(h.stoat.pending_mark.is_none());
    }

    #[test]
    fn goto_mark_unset_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::GotoMark);
        h.type_keys("z");
        assert_eq!(cursor_offset(&mut h), before);
        assert!(h.stoat.pending_mark.is_none());
    }

    #[test]
    fn pending_mark_clears_on_non_char() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SetMark);
        assert_eq!(h.stoat.pending_mark, Some(MarkRequest::Set));
        h.type_keys("escape");
        assert!(h.stoat.pending_mark.is_none());
    }

    #[test]
    fn set_uppercase_mark_then_goto_jumps_across_files() {
        let mut h = TestHarness::with_size(40, 10);
        let (a, b) = seed_two_files(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveRight);
        assert_eq!(cursor_offset(&mut h), 5);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SetMark);
        h.type_keys("A");

        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: b.clone() });
        h.settle();
        assert_eq!(focused_path(&h), b);

        crate::action_handlers::dispatch(&mut h.stoat, &action::GotoMarkExact);
        h.type_keys("A");
        assert_eq!(focused_path(&h), a);
        assert_eq!(cursor_offset(&mut h), 5);
    }

    #[test]
    fn lowercase_mark_remains_buffer_local() {
        let mut h = TestHarness::with_size(40, 10);
        let (_a, b) = seed_two_files(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveRight);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SetMark);
        h.type_keys("a");

        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: b.clone() });
        h.settle();
        assert_eq!(focused_path(&h), b);
        let before = cursor_offset(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &action::GotoMark);
        h.type_keys("a");
        assert_eq!(focused_path(&h), b);
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn mark_survives_edit_before_position() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\ndef\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveRight);
        assert_eq!(cursor_offset(&mut h), 5);

        crate::action_handlers::dispatch(&mut h.stoat, &action::SetMark);
        h.type_keys("a");

        h.edit_focused(0..0, "// ");

        crate::action_handlers::dispatch(&mut h.stoat, &action::GotoMarkExact);
        h.type_keys("a");
        assert_eq!(cursor_offset(&mut h), 8);
    }
}
