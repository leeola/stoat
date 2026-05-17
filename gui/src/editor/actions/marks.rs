use crate::editor::{Editor, EditorEvent};
use gpui::Context;
use stoat_text::{Anchor, Bias, Point, Selection, SelectionGoal};

/// After-key chord variant for [`Editor::handle_set_mark`] /
/// [`Editor::handle_goto_mark`]. `Set` records the cursor under a
/// mark name; `GotoLine` jumps to the stored mark's row at column
/// zero; `GotoExact` jumps to the stored byte offset directly.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MarkRequest {
    Set,
    GotoLine,
    GotoExact,
}

impl Editor {
    /// Record the current primary head as the mark named `ch` on
    /// the underlying singleton buffer. Overwrites any prior mark
    /// with the same name. No-op when the editor is not over a
    /// singleton buffer (multi-buffer reviews do not yet support
    /// marks).
    pub fn handle_set_mark(&mut self, ch: char, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let sel = self
            .selections
            .all_anchors()
            .iter()
            .max_by_key(|s| s.id)
            .expect("at least one selection");
        let head = sel.head();
        let offset = snapshot.resolve_anchor(&head);
        let anchor = snapshot.anchor_at(offset, Bias::Right);

        let Some(buffer) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return;
        };
        buffer.update(cx, |b, _| b.set_mark(ch, anchor));
        cx.notify();
    }

    /// Jump the primary selection to the stored mark `ch`. With
    /// `exact = true` the cursor lands on the exact byte offset
    /// where the mark was set; with `exact = false` the cursor
    /// lands at column zero of that row. No-op when the mark is
    /// unset or when the editor is not over a singleton buffer.
    pub fn handle_goto_mark(&mut self, ch: char, exact: bool, cx: &mut Context<'_, Self>) {
        let Some(buffer) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return;
        };
        let Some(stored_anchor) = buffer.read(cx).get_mark(ch) else {
            return;
        };
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let stored_offset = snapshot.resolve_anchor(&stored_anchor);
        let rope = snapshot.rope();
        let clamped = stored_offset.min(rope.len());
        let target = if exact {
            clamped
        } else {
            let row = rope.offset_to_point(clamped).row;
            rope.point_to_offset(Point::new(row, 0))
        };
        collapse_primary_to(self, &snapshot, target);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Record the current primary head offset on the editor's
    /// jumplist. Used by [`JumpBackward`] / [`JumpForward`] to walk
    /// previously-saved positions.
    pub fn handle_save_selection(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let head = self
            .selections
            .all_anchors()
            .iter()
            .max_by_key(|s| s.id)
            .expect("at least one selection")
            .head();
        let offset = snapshot.resolve_anchor(&head);
        self.jumplist.save(offset);
        cx.notify();
    }

    /// Walk the editor's jumplist backward `count` times. The
    /// primary selection collapses to the final position. No-op
    /// when the jumplist is empty or already at the oldest entry.
    pub fn handle_jump_backward(&mut self, count: u32, cx: &mut Context<'_, Self>) {
        let mut target = None;
        for _ in 0..count {
            match self.jumplist.backward() {
                Some(pos) => target = Some(pos),
                None => break,
            }
        }
        let Some(target) = target else {
            return;
        };
        let snapshot = self.multi_buffer.read(cx).snapshot();
        collapse_primary_to(self, &snapshot, target);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Walk the editor's jumplist forward `count` times after a
    /// prior [`Self::handle_jump_backward`]. The primary selection
    /// collapses to the final position. No-op at the newest entry.
    pub fn handle_jump_forward(&mut self, count: u32, cx: &mut Context<'_, Self>) {
        let mut target = None;
        for _ in 0..count {
            match self.jumplist.forward() {
                Some(pos) => target = Some(pos),
                None => break,
            }
        }
        let Some(target) = target else {
            return;
        };
        let snapshot = self.multi_buffer.read(cx).snapshot();
        collapse_primary_to(self, &snapshot, target);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Collapse the primary selection at jumplist entry `idx` and
    /// position the jumplist's navigation cursor so the next
    /// [`Self::handle_jump_backward`] walks from `idx`. No-op when
    /// `idx` is out of range.
    pub fn handle_jump_to_jumplist_entry(&mut self, idx: usize, cx: &mut Context<'_, Self>) {
        let Some(&target) = self.jumplist.entries().get(idx) else {
            return;
        };
        let snapshot = self.multi_buffer.read(cx).snapshot();
        collapse_primary_to(self, &snapshot, target);
        self.jumplist.set_cursor(idx);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }
}

fn collapse_primary_to(
    editor: &mut Editor,
    snapshot: &stoat::multi_buffer::MultiBufferSnapshot,
    offset: usize,
) {
    let anchor = snapshot.anchor_at(offset, Bias::Right);
    let new_disjoint: Vec<Selection<Anchor>> = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        })
        .collect();
    editor.selections.replace_with(new_disjoint, snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, Entity, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn new_editor(cx: &mut TestAppContext, text: &str) -> (Entity<Buffer>, Entity<Editor>) {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        let editor = cx.update(|cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        });
        (buffer, editor)
    }

    fn seed_cursor(editor: &Entity<Editor>, cx: &mut TestAppContext, offset: usize) {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let anchor = snapshot.anchor_at(offset, Bias::Left);
            ed.selections_mut().replace_with(
                vec![Selection {
                    id: 1,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
        });
    }

    fn primary_offset(editor: &Entity<Editor>, cx: &mut TestAppContext) -> usize {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = ed
                .selections()
                .all_anchors()
                .iter()
                .max_by_key(|s| s.id)
                .expect("at least one selection");
            snapshot.resolve_anchor(&sel.head())
        })
    }

    #[test]
    fn set_mark_stores_anchor_under_char() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "abcdef");
        seed_cursor(&editor, &mut cx, 3);

        editor.update(&mut cx, |ed, cx| ed.handle_set_mark('a', cx));

        let anchor = buffer.read_with(&cx, |b, _| b.get_mark('a')).expect("mark");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            assert_eq!(snapshot.resolve_anchor(&anchor), 3);
        });
    }

    #[test]
    fn goto_mark_exact_jumps_to_stored_offset() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abcdef");
        seed_cursor(&editor, &mut cx, 4);
        editor.update(&mut cx, |ed, cx| ed.handle_set_mark('a', cx));
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_mark('a', true, cx));

        assert_eq!(primary_offset(&editor, &mut cx), 4);
    }

    #[test]
    fn goto_mark_line_clamps_to_row_start() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc\ndefgh\nij");
        seed_cursor(&editor, &mut cx, 6);
        editor.update(&mut cx, |ed, cx| ed.handle_set_mark('m', cx));
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_mark('m', false, cx));

        assert_eq!(primary_offset(&editor, &mut cx), 4);
    }

    #[test]
    fn goto_mark_unset_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");
        seed_cursor(&editor, &mut cx, 2);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_mark('z', true, cx));

        assert_eq!(primary_offset(&editor, &mut cx), 2);
    }

    #[test]
    fn save_selection_records_on_jumplist() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abcdef");
        seed_cursor(&editor, &mut cx, 4);

        editor.update(&mut cx, |ed, cx| ed.handle_save_selection(cx));

        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.jumplist().entries(), &[4]);
        });
    }

    #[test]
    fn jump_backward_walks_through_history() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abcdef");
        for offset in [1, 3, 5] {
            seed_cursor(&editor, &mut cx, offset);
            editor.update(&mut cx, |ed, cx| ed.handle_save_selection(cx));
        }
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_jump_backward(1, cx));
        assert_eq!(primary_offset(&editor, &mut cx), 5);

        editor.update(&mut cx, |ed, cx| ed.handle_jump_backward(1, cx));
        assert_eq!(primary_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn jump_backward_count_accumulates() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abcdef");
        for offset in [1, 3, 5] {
            seed_cursor(&editor, &mut cx, offset);
            editor.update(&mut cx, |ed, cx| ed.handle_save_selection(cx));
        }
        seed_cursor(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_jump_backward(2, cx));

        assert_eq!(primary_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn jump_forward_walks_back_after_backward() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abcdef");
        for offset in [1, 3, 5] {
            seed_cursor(&editor, &mut cx, offset);
            editor.update(&mut cx, |ed, cx| ed.handle_save_selection(cx));
        }

        editor.update(&mut cx, |ed, cx| ed.handle_jump_backward(2, cx));
        editor.update(&mut cx, |ed, cx| ed.handle_jump_forward(1, cx));

        assert_eq!(primary_offset(&editor, &mut cx), 5);
    }

    #[test]
    fn jump_backward_empty_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");
        seed_cursor(&editor, &mut cx, 2);

        editor.update(&mut cx, |ed, cx| ed.handle_jump_backward(1, cx));

        assert_eq!(primary_offset(&editor, &mut cx), 2);
    }

    #[test]
    fn mark_anchor_shifts_with_edit_before_position() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "abcdef");
        seed_cursor(&editor, &mut cx, 4);
        editor.update(&mut cx, |ed, cx| ed.handle_set_mark('m', cx));

        buffer.update(&mut cx, |b, cx| b.edit(0..0, "// ", cx));

        seed_cursor(&editor, &mut cx, 0);
        editor.update(&mut cx, |ed, cx| ed.handle_goto_mark('m', true, cx));

        assert_eq!(primary_offset(&editor, &mut cx), 7);
    }
}
