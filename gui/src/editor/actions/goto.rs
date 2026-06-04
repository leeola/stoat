use crate::editor::{
    actions::movement::extend_head, scroll::autoscroll::AutoscrollStrategy, Editor, EditorEvent,
};
use gpui::Context;
use stoat_text::{Anchor, Bias, Point, Selection, SelectionGoal};

/// Line-boundary target for [`Editor::handle_goto_line_boundary`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LineBoundary {
    Start,
    End,
}

/// Direction passed to [`Editor::handle_goto_diagnostic`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DiagnosticDir {
    Next,
    Prev,
}

/// Direction passed to [`Editor::handle_goto_hunk`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChangeDir {
    Next,
    Prev,
}

impl Editor {
    /// Move every selection's head to the start or end of its
    /// current line. With `extend = false`, each selection
    /// collapses to a cursor at the boundary; with `extend = true`,
    /// the tail is preserved.
    pub fn handle_goto_line_boundary(
        &mut self,
        boundary: LineBoundary,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let head_anchor = sel.head();
                let head_point = snapshot.point_for_anchor(&head_anchor);
                let col = match boundary {
                    LineBoundary::Start => 0,
                    LineBoundary::End => snapshot.rope().line_len(head_point.row),
                };
                let target_offset = snapshot
                    .rope()
                    .point_to_offset(Point::new(head_point.row, col));
                let anchor = snapshot.anchor_at(target_offset, Bias::Right);
                if extend {
                    extend_head(sel, anchor, target_offset, SelectionGoal::None, &snapshot)
                } else {
                    let mut new = sel.clone();
                    new.collapse_to(anchor, SelectionGoal::None);
                    new
                }
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn handle_goto_line_start(&mut self, extend: bool, cx: &mut Context<'_, Self>) {
        self.handle_goto_line_boundary(LineBoundary::Start, extend, cx);
    }

    pub fn handle_goto_line_end(&mut self, extend: bool, cx: &mut Context<'_, Self>) {
        self.handle_goto_line_boundary(LineBoundary::End, extend, cx);
    }

    /// Move every selection's head to the first non-whitespace
    /// char on its current line. No-op for lines that contain only
    /// whitespace.
    pub fn handle_goto_first_nonwhitespace(&mut self, extend: bool, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let head_anchor = sel.head();
                let head_point = snapshot.point_for_anchor(&head_anchor);
                let row = head_point.row;
                let line_start = snapshot.rope().point_to_offset(Point::new(row, 0));
                let line_end = snapshot
                    .rope()
                    .point_to_offset(Point::new(row, snapshot.rope().line_len(row)));

                let mut found = None;
                let mut cursor = line_start;
                for ch in snapshot.rope().chars_at(line_start) {
                    if cursor >= line_end {
                        break;
                    }
                    if !ch.is_whitespace() {
                        found = Some(cursor);
                        break;
                    }
                    cursor += ch.len_utf8();
                }
                let Some(target_offset) = found else {
                    return sel.clone();
                };

                let anchor = snapshot.anchor_at(target_offset, Bias::Right);
                if extend {
                    extend_head(sel, anchor, target_offset, SelectionGoal::None, &snapshot)
                } else {
                    let mut new = sel.clone();
                    new.collapse_to(anchor, SelectionGoal::None);
                    new
                }
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Move every selection's head to offset zero. With
    /// `extend = false`, collapses to a cursor at the file start.
    pub fn handle_goto_file_start(&mut self, extend: bool, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let target_offset = 0;
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let anchor = snapshot.anchor_at(target_offset, Bias::Right);
                if extend {
                    extend_head(sel, anchor, target_offset, SelectionGoal::None, &snapshot)
                } else {
                    let mut new = sel.clone();
                    new.collapse_to(anchor, SelectionGoal::None);
                    new
                }
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        self.request_autoscroll(AutoscrollStrategy::Fit, cx);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Move every selection's head to the start of the last line
    /// that contains content. A trailing empty row (the one that
    /// follows a final `\n`) is skipped so the cursor lands on
    /// real text.
    pub fn handle_goto_last_line(&mut self, extend: bool, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let mut target_row = snapshot.rope().max_point().row;
        if target_row > 0 && snapshot.rope().line_len(target_row) == 0 {
            target_row -= 1;
        }
        let target_offset = snapshot.rope().point_to_offset(Point::new(target_row, 0));
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let anchor = snapshot.anchor_at(target_offset, Bias::Right);
                if extend {
                    extend_head(sel, anchor, target_offset, SelectionGoal::None, &snapshot)
                } else {
                    let mut new = sel.clone();
                    new.collapse_to(anchor, SelectionGoal::None);
                    new
                }
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        self.request_autoscroll(AutoscrollStrategy::Fit, cx);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Jump to the given 1-indexed line. When `count` is `None`,
    /// falls back to [`Editor::handle_goto_last_line`]. The target
    /// row is clamped to the last content row.
    pub fn handle_goto_line_number(&mut self, count: Option<u32>, cx: &mut Context<'_, Self>) {
        let Some(count) = count else {
            self.handle_goto_last_line(false, cx);
            return;
        };
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let mut last_row = snapshot.rope().max_point().row;
        if last_row > 0 && snapshot.rope().line_len(last_row) == 0 {
            last_row -= 1;
        }
        let zero_indexed = count.saturating_sub(1);
        let target_row = (zero_indexed as u64).min(last_row as u64) as u32;
        let target_offset = snapshot.rope().point_to_offset(Point::new(target_row, 0));
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let start_anchor = snapshot.anchor_at(target_offset, Bias::Right);
                let end_anchor = snapshot.anchor_at(target_offset, Bias::Left);
                Selection {
                    id: sel.id,
                    start: start_anchor,
                    end: end_anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Jump to the given 1-indexed column on the newest selection's
    /// current line. The target offset is clamped to the line end.
    /// With `extend = false`, collapses every selection to that
    /// position; with `extend = true`, only the head moves.
    pub fn handle_goto_column(&mut self, count: u32, extend: bool, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let newest = self
            .selections
            .all_anchors()
            .iter()
            .max_by_key(|s| s.id)
            .cloned();
        let Some(newest) = newest else {
            return;
        };
        let head_point = snapshot.point_for_anchor(&newest.head());
        let row = head_point.row;
        let line_start = snapshot.rope().point_to_offset(Point::new(row, 0));
        let line_end = snapshot
            .rope()
            .point_to_offset(Point::new(row, snapshot.rope().line_len(row)));

        let steps = count.saturating_sub(1) as usize;
        let mut target_offset = line_start;
        for ch in snapshot.rope().chars_at(line_start).take(steps) {
            let next = target_offset + ch.len_utf8();
            if next > line_end {
                break;
            }
            target_offset = next;
        }

        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let anchor = snapshot.anchor_at(target_offset, Bias::Right);
                if extend {
                    extend_head(sel, anchor, target_offset, sel.goal, &snapshot)
                } else {
                    let mut new = sel.clone();
                    new.collapse_to(anchor, SelectionGoal::None);
                    new
                }
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Jump every selection to the next or previous LSP diagnostic in
    /// the active buffer. No-op when the editor has no
    /// [`Editor::file_path`], no attached [`crate::diagnostics::DiagnosticSet`],
    /// or no diagnostic on the requested side of the primary cursor.
    pub fn handle_goto_diagnostic(&mut self, dir: DiagnosticDir, cx: &mut Context<'_, Self>) {
        let Some(path) = self.file_path.clone() else {
            return;
        };
        let Some(diagnostic_set) = self.diagnostic_set.clone() else {
            return;
        };

        let snapshot = self.multi_buffer.read(cx).snapshot();
        let mut offsets: Vec<usize> = diagnostic_set
            .read(cx)
            .get(&path)
            .iter()
            .map(|diag| diagnostic_start_offset(diag, &snapshot))
            .collect();
        offsets.sort_unstable();
        offsets.dedup();

        let Some(newest) = self
            .selections
            .all_anchors()
            .iter()
            .max_by_key(|s| s.id)
            .cloned()
        else {
            return;
        };
        let cursor = snapshot.resolve_anchor(&newest.head());

        let target = match dir {
            DiagnosticDir::Next => offsets.into_iter().find(|&o| o > cursor),
            DiagnosticDir::Prev => offsets.into_iter().rev().find(|&o| o < cursor),
        };
        let Some(target_offset) = target else {
            return;
        };

        let target_anchor = snapshot.anchor_at(target_offset, Bias::Right);
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let mut new = sel.clone();
                new.collapse_to(target_anchor, SelectionGoal::None);
                new
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Jump every selection to the start of the next or previous diff
    /// hunk in the active buffer. No-op when the buffer has no diff
    /// hunks or no hunk lies on the requested side of the primary
    /// cursor's row.
    pub fn handle_goto_hunk(&mut self, dir: ChangeDir, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();

        let mut rows: Vec<u32> = self
            .diff_map
            .read(cx)
            .diff()
            .hunks_in_range(0..u32::MAX)
            .iter()
            .map(|h| h.buffer_start_line)
            .collect();
        rows.sort_unstable();
        rows.dedup();

        let Some(newest) = self
            .selections
            .all_anchors()
            .iter()
            .max_by_key(|s| s.id)
            .cloned()
        else {
            return;
        };
        let cursor_row = snapshot.point_for_anchor(&newest.head()).row;

        let target_row = match dir {
            ChangeDir::Next => rows.into_iter().find(|&r| r > cursor_row),
            ChangeDir::Prev => rows.into_iter().rev().find(|&r| r < cursor_row),
        };
        let Some(target_row) = target_row else {
            return;
        };

        let max_row = snapshot.rope().max_point().row;
        let target_row = target_row.min(max_row);
        let target_offset = snapshot.rope().point_to_offset(Point::new(target_row, 0));
        let target_anchor = snapshot.anchor_at(target_offset, Bias::Right);
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let mut new = sel.clone();
                new.collapse_to(target_anchor, SelectionGoal::None);
                new
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }
}

fn diagnostic_start_offset(
    diag: &lsp_types::Diagnostic,
    snapshot: &stoat::multi_buffer::MultiBufferSnapshot,
) -> usize {
    let rope = snapshot.rope();
    let max_row = rope.max_point().row;
    let row = diag.range.start.line.min(max_row);
    let line_len = rope.line_len(row);
    let col = diag.range.start.character.min(line_len);
    rope.point_to_offset(Point::new(row, col))
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

    fn new_editor(cx: &mut TestAppContext, text: &str) -> Entity<Editor> {
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
        cx.update(|cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn cursor_offset(editor: &Entity<Editor>, cx: &mut TestAppContext) -> usize {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            snapshot.resolve_anchor(&ed.selections().all_anchors()[0].head())
        })
    }

    fn seed_at_offset(editor: &Entity<Editor>, cx: &mut TestAppContext, offset: usize) {
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

    #[test]
    fn goto_line_start_collapses_to_column_zero() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abcdef");
        seed_at_offset(&editor, &mut cx, 4);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_line_start(false, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 0);
    }

    #[test]
    fn goto_line_end_jumps_to_end_of_current_line() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abc\ndefgh\nij");
        seed_at_offset(&editor, &mut cx, 5);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_line_end(false, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 9);
    }

    #[test]
    fn goto_first_nonwhitespace_skips_leading_spaces() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "    hello");
        seed_at_offset(&editor, &mut cx, 8);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_first_nonwhitespace(false, cx)
        });

        assert_eq!(cursor_offset(&editor, &mut cx), 4);
    }

    #[test]
    fn goto_first_nonwhitespace_noop_on_blank_line() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "   ");
        seed_at_offset(&editor, &mut cx, 1);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_first_nonwhitespace(false, cx)
        });

        assert_eq!(cursor_offset(&editor, &mut cx), 1);
    }

    #[test]
    fn goto_file_start_jumps_to_zero() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abc\ndef");
        seed_at_offset(&editor, &mut cx, 5);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_file_start(false, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 0);
    }

    #[test]
    fn goto_last_line_skips_trailing_empty_row() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma\n");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_last_line(false, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 11);
    }

    #[test]
    fn goto_line_number_jumps_to_one_indexed_row() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_line_number(Some(2), cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 6);
    }

    #[test]
    fn goto_line_number_clamps_past_last_line() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_line_number(Some(99), cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 11);
    }

    #[test]
    fn goto_line_number_without_count_falls_back_to_last_line() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_line_number(None, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 11);
    }

    #[test]
    fn goto_column_jumps_to_one_indexed_column() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abcdef\nghi");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_column(4, false, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn goto_column_clamps_at_line_end() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abc\ndefghi");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_column(99, false, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn goto_line_end_extend_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abcdef");
        seed_at_offset(&editor, &mut cx, 2);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_line_end(true, cx));

        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = &ed.selections().all_anchors()[0];
            assert_eq!(snapshot.resolve_anchor(&sel.start), 2);
            assert_eq!(snapshot.resolve_anchor(&sel.end), 6);
            assert!(!sel.reversed);
        });
    }

    fn diag_at(line: u32, character: u32) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range::new(
                lsp_types::Position::new(line, character),
                lsp_types::Position::new(line, character + 1),
            ),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: String::new(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn editor_with_diagnostics(
        cx: &mut TestAppContext,
        text: &str,
        path: &std::path::Path,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> Entity<Editor> {
        let editor = new_editor(cx, text);
        let diag_set = cx.update(|cx| cx.new(|_| crate::diagnostics::DiagnosticSet::new()));
        diag_set.update(cx, |s, cx| {
            s.replace_for_path(path.to_path_buf(), diagnostics, cx)
        });
        editor.update(cx, |ed, cx| {
            ed.set_file_path(Some(path.to_path_buf()), cx);
            ed.set_diagnostic_set(Some(diag_set), cx);
        });
        cx.run_until_parked();
        editor
    }

    #[test]
    fn goto_next_diagnostic_jumps_forward_then_to_second() {
        let mut cx = TestAppContext::single();
        let path = std::path::PathBuf::from("/ws/a.rs");
        let editor = editor_with_diagnostics(
            &mut cx,
            "alpha\nbeta\ngamma",
            &path,
            vec![diag_at(0, 2), diag_at(2, 1)],
        );
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_diagnostic(DiagnosticDir::Next, cx)
        });
        assert_eq!(cursor_offset(&editor, &mut cx), 2);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_diagnostic(DiagnosticDir::Next, cx)
        });
        assert_eq!(cursor_offset(&editor, &mut cx), 12);
    }

    #[test]
    fn goto_next_diagnostic_past_last_is_noop() {
        let mut cx = TestAppContext::single();
        let path = std::path::PathBuf::from("/ws/a.rs");
        let editor = editor_with_diagnostics(
            &mut cx,
            "alpha\nbeta\ngamma",
            &path,
            vec![diag_at(0, 2), diag_at(2, 1)],
        );
        seed_at_offset(&editor, &mut cx, 14);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_diagnostic(DiagnosticDir::Next, cx)
        });

        assert_eq!(cursor_offset(&editor, &mut cx), 14);
    }

    #[test]
    fn goto_prev_diagnostic_jumps_backward() {
        let mut cx = TestAppContext::single();
        let path = std::path::PathBuf::from("/ws/a.rs");
        let editor = editor_with_diagnostics(
            &mut cx,
            "alpha\nbeta\ngamma",
            &path,
            vec![diag_at(0, 2), diag_at(2, 1)],
        );
        seed_at_offset(&editor, &mut cx, 14);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_diagnostic(DiagnosticDir::Prev, cx)
        });
        assert_eq!(cursor_offset(&editor, &mut cx), 12);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_diagnostic(DiagnosticDir::Prev, cx)
        });
        assert_eq!(cursor_offset(&editor, &mut cx), 2);
    }

    #[test]
    fn goto_prev_diagnostic_before_first_is_noop() {
        let mut cx = TestAppContext::single();
        let path = std::path::PathBuf::from("/ws/a.rs");
        let editor = editor_with_diagnostics(
            &mut cx,
            "alpha\nbeta\ngamma",
            &path,
            vec![diag_at(0, 2), diag_at(2, 1)],
        );
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_diagnostic(DiagnosticDir::Prev, cx)
        });

        assert_eq!(cursor_offset(&editor, &mut cx), 0);
    }

    #[test]
    fn goto_diagnostic_without_file_path_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta");
        let diag_set = cx.update(|cx| cx.new(|_| crate::diagnostics::DiagnosticSet::new()));
        editor.update(&mut cx, |ed, cx| {
            ed.set_diagnostic_set(Some(diag_set), cx);
        });
        cx.run_until_parked();
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_diagnostic(DiagnosticDir::Next, cx)
        });

        assert_eq!(cursor_offset(&editor, &mut cx), 0);
    }

    #[test]
    fn goto_diagnostic_without_diagnostic_set_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta");
        editor.update(&mut cx, |ed, cx| {
            ed.set_file_path(Some(std::path::PathBuf::from("/ws/a.rs")), cx);
        });
        cx.run_until_parked();
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_goto_diagnostic(DiagnosticDir::Next, cx)
        });

        assert_eq!(cursor_offset(&editor, &mut cx), 0);
    }

    fn added_hunk_at(lines: std::ops::Range<u32>) -> stoat::diff_map::DiffHunk {
        stoat::diff_map::DiffHunk {
            status: stoat::diff_map::DiffHunkStatus::Added,
            staged: false,
            buffer_start_line: lines.start,
            buffer_line_range: lines,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        }
    }

    fn seed_hunks(
        editor: &Entity<Editor>,
        cx: &mut TestAppContext,
        hunks: Vec<stoat::diff_map::DiffHunk>,
    ) {
        editor.update(cx, |ed, cx| {
            let new = stoat::DiffMap::from_hunks(hunks, None);
            ed.diff_map().update(cx, |dm, cx| dm.set_diff(new, cx));
        });
        cx.run_until_parked();
    }

    #[test]
    fn goto_next_hunk_jumps_forward_then_to_second() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma\ndelta\nepsilon");
        seed_hunks(
            &editor,
            &mut cx,
            vec![added_hunk_at(1..2), added_hunk_at(3..4)],
        );
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_hunk(ChangeDir::Next, cx));
        assert_eq!(cursor_offset(&editor, &mut cx), 6);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_hunk(ChangeDir::Next, cx));
        assert_eq!(cursor_offset(&editor, &mut cx), 17);
    }

    #[test]
    fn goto_next_hunk_past_last_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma\ndelta\nepsilon");
        seed_hunks(
            &editor,
            &mut cx,
            vec![added_hunk_at(1..2), added_hunk_at(3..4)],
        );
        seed_at_offset(&editor, &mut cx, 25);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_hunk(ChangeDir::Next, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 25);
    }

    #[test]
    fn goto_prev_hunk_jumps_backward() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma\ndelta\nepsilon");
        seed_hunks(
            &editor,
            &mut cx,
            vec![added_hunk_at(1..2), added_hunk_at(3..4)],
        );
        seed_at_offset(&editor, &mut cx, 25);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_hunk(ChangeDir::Prev, cx));
        assert_eq!(cursor_offset(&editor, &mut cx), 17);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_hunk(ChangeDir::Prev, cx));
        assert_eq!(cursor_offset(&editor, &mut cx), 6);
    }

    #[test]
    fn goto_prev_hunk_before_first_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma\ndelta\nepsilon");
        seed_hunks(
            &editor,
            &mut cx,
            vec![added_hunk_at(1..2), added_hunk_at(3..4)],
        );
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_hunk(ChangeDir::Prev, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 0);
    }

    #[test]
    fn goto_hunk_with_empty_diff_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "alpha\nbeta\ngamma");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_goto_hunk(ChangeDir::Next, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 0);
    }
}
