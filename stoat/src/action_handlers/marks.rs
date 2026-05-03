use crate::{
    action_handlers::{focused_editor_mut, movement},
    app::{Stoat, UpdateEffect},
};
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
        MarkRequest::Set => {
            let Some(editor) = focused_editor_mut(stoat) else {
                return UpdateEffect::None;
            };
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            let head = editor.selections.newest_anchor().head();
            let offset = buf_snap.resolve_anchor(&head);
            let buffer_id = editor.buffer_id;
            stoat.marks.insert((buffer_id, ch), offset);
            UpdateEffect::Redraw
        },
        MarkRequest::GotoLine | MarkRequest::GotoExact => {
            let buffer_id = focused_editor_mut(stoat).map(|e| e.buffer_id);
            let Some(buffer_id) = buffer_id else {
                return UpdateEffect::None;
            };
            let Some(&stored_offset) = stoat.marks.get(&(buffer_id, ch)) else {
                return UpdateEffect::None;
            };
            let target = match request {
                MarkRequest::GotoExact => stored_offset,
                MarkRequest::GotoLine => {
                    let editor = focused_editor_mut(stoat).expect("buffer present above");
                    let snapshot = editor.display_map.snapshot();
                    let buf_snap = snapshot.buffer_snapshot();
                    let rope = buf_snap.rope();
                    let clamped = stored_offset.min(rope.len());
                    let row = rope.offset_to_point(clamped).row;
                    rope.point_to_offset(Point::new(row, 0))
                },
                MarkRequest::Set => unreachable!(),
            };
            movement::jump_to_offset(stoat, target)
        },
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
}
