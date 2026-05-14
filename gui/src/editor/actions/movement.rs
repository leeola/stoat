use crate::editor::{Editor, EditorEvent};
use gpui::Context;
use stoat::{multi_buffer::MultiBufferSnapshot, DisplayPoint};
use stoat_text::{
    next_long_word_end, next_long_word_start, next_word_end, next_word_start, prev_long_word_end,
    prev_long_word_start, prev_word_end, prev_word_start, Anchor, Bias, Selection, SelectionGoal,
};

/// Vertical-only page-style motion direction. `Up` walks toward
/// row 0; `Down` walks toward the last buffer row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PageDir {
    Up,
    Down,
}

/// Word-boundary target for [`Editor::handle_move_word`]. `Long`
/// variants treat any run of non-whitespace as one word, ignoring
/// punctuation-vs-alphanumeric boundaries; the non-`Long` variants
/// split on those boundaries.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WordTarget {
    NextStart,
    NextEnd,
    PrevStart,
    PrevEnd,
    NextLongStart,
    NextLongEnd,
    PrevLongStart,
    PrevLongEnd,
}

/// Fallback viewport row count when the editor's text-region
/// bounds have not been reported yet. Matches the TUI's default
/// (`stoat::action_handlers::movement::DEFAULT_VIEWPORT_ROWS`).
pub const DEFAULT_VIEWPORT_ROWS: u32 = 20;

/// Direction + landing offset for the after-key
/// [`Editor::handle_find_char`] chord. `Next`/`Prev` land on the
/// matched character; `TillNext`/`TillPrev` land one position
/// before/after it so the cursor sits adjacent to the target.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum FindKind {
    NextChar,
    PrevChar,
    TillNextChar,
    TillPrevChar,
}

impl Editor {
    /// Move every selection's head by `count * delta` characters
    /// along the buffer. Negative `delta` walks backward.
    /// `extend = false` collapses each selection to a cursor at
    /// the new head; `extend = true` preserves the tail and
    /// updates only the head.
    pub fn handle_move_horizontal(
        &mut self,
        delta: i32,
        count: u32,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let count_usize = count as usize;
        let mut moved = false;
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let head_offset = snapshot.resolve_anchor(&sel.head());
                let new_offset = if delta > 0 {
                    let mut offset = head_offset;
                    for ch in snapshot.rope().chars_at(head_offset).take(count_usize) {
                        offset += ch.len_utf8();
                    }
                    offset
                } else {
                    let mut offset = head_offset;
                    for ch in snapshot
                        .rope()
                        .reversed_chars_at(head_offset)
                        .take(count_usize)
                    {
                        offset -= ch.len_utf8();
                    }
                    offset
                };
                if new_offset == head_offset {
                    return sel.clone();
                }
                moved = true;
                let anchor = snapshot.anchor_at(new_offset, Bias::Right);
                if extend {
                    extend_head(sel, anchor, new_offset, SelectionGoal::None, &snapshot)
                } else {
                    let mut new = sel.clone();
                    new.collapse_to(anchor, SelectionGoal::None);
                    new
                }
            })
            .collect();
        if !moved {
            return;
        }
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Move every selection's head `count * delta` display rows.
    /// Negative `delta` walks up. Preserves the goal column across
    /// short lines so reversing the motion restores the original
    /// column where possible.
    pub fn handle_move_vertical(
        &mut self,
        delta: i32,
        count: u32,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let max_row = display_snapshot.max_point().row;
        let scaled_delta = (delta as i64).saturating_mul(count as i64);

        let mut moved = false;
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let head_anchor = sel.head();
                let head_point = buffer_snapshot.point_for_anchor(&head_anchor);
                let head_display = display_snapshot.buffer_to_display(head_point);
                let goal_col = match sel.goal {
                    SelectionGoal::Column(c) => c,
                    SelectionGoal::None => head_display.column,
                };
                let new_row_i = (head_display.row as i64).saturating_add(scaled_delta);
                if new_row_i < 0 || new_row_i > max_row as i64 {
                    return sel.clone();
                }
                let new_row = new_row_i as u32;
                let clamped_col = goal_col.min(display_snapshot.line_len(new_row));
                let raw = DisplayPoint::new(new_row, clamped_col);
                let clipped = display_snapshot.clip_point(raw, Bias::Left);
                let Some(buffer_pt) = display_snapshot.display_to_buffer(clipped) else {
                    return sel.clone();
                };
                let offset = buffer_snapshot.rope().point_to_offset(buffer_pt);
                if offset == buffer_snapshot.resolve_anchor(&head_anchor) {
                    return sel.clone();
                }
                moved = true;
                let anchor = buffer_snapshot.anchor_at(offset, Bias::Right);
                if extend {
                    extend_head(
                        sel,
                        anchor,
                        offset,
                        SelectionGoal::Column(goal_col),
                        buffer_snapshot,
                    )
                } else {
                    let mut new = sel.clone();
                    new.collapse_to(anchor, SelectionGoal::Column(goal_col));
                    new
                }
            })
            .collect();
        if !moved {
            return;
        }
        self.selections.replace_with(new_disjoint, buffer_snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Page motion in the configured direction. `half = true`
    /// steps `viewport_rows / 2` rounded up; `half = false` steps
    /// the full viewport. The viewport row count is derived from
    /// the editor's text-region bounds and cell metrics; falls
    /// back to [`DEFAULT_VIEWPORT_ROWS`] when neither has been
    /// reported yet. Each step collapses the moved selections to
    /// a cursor on the target row.
    pub fn handle_page_motion(
        &mut self,
        dir: PageDir,
        half: bool,
        count: u32,
        cx: &mut Context<'_, Self>,
    ) {
        let viewport_rows = self.viewport_rows_for_page().max(1);
        let base_delta = if half {
            viewport_rows.div_ceil(2)
        } else {
            viewport_rows
        };
        let delta = base_delta.saturating_mul(count);

        let snapshot = self.multi_buffer.read(cx).snapshot();
        let max_row = snapshot.rope().max_point().row;

        let mut moved = false;
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let head = sel.head();
                let current_row = snapshot.point_for_anchor(&head).row;
                let target_row = match dir {
                    PageDir::Up => current_row.saturating_sub(delta),
                    PageDir::Down => current_row.saturating_add(delta).min(max_row),
                };
                if target_row == current_row {
                    return sel.clone();
                }
                moved = true;
                let target_offset = snapshot
                    .rope()
                    .point_to_offset(stoat_text::Point::new(target_row, 0));
                let target_anchor = snapshot.anchor_at(target_offset, Bias::Right);
                let mut new = sel.clone();
                new.collapse_to(target_anchor, SelectionGoal::None);
                new
            })
            .collect();
        if !moved {
            return;
        }
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Move every selection's head to the next or previous word
    /// boundary `count` times. When `extend = false`, the selection
    /// covers the traversed word (Helix/Kakoune-style); when
    /// `extend = true`, only the head moves and the tail is
    /// preserved.
    pub fn handle_move_word(
        &mut self,
        target: WordTarget,
        count: u32,
        extend: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let mut moved = false;
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let head_offset = snapshot.resolve_anchor(&sel.head());
                let mut target_offset = head_offset;
                for _ in 0..count {
                    let next = match target {
                        WordTarget::NextStart => next_word_start(snapshot.rope(), target_offset),
                        WordTarget::NextEnd => next_word_end(snapshot.rope(), target_offset),
                        WordTarget::PrevStart => prev_word_start(snapshot.rope(), target_offset),
                        WordTarget::PrevEnd => prev_word_end(snapshot.rope(), target_offset),
                        WordTarget::NextLongStart => {
                            next_long_word_start(snapshot.rope(), target_offset)
                        },
                        WordTarget::NextLongEnd => {
                            next_long_word_end(snapshot.rope(), target_offset)
                        },
                        WordTarget::PrevLongStart => {
                            prev_long_word_start(snapshot.rope(), target_offset)
                        },
                        WordTarget::PrevLongEnd => {
                            prev_long_word_end(snapshot.rope(), target_offset)
                        },
                    };
                    if next == target_offset {
                        break;
                    }
                    target_offset = next;
                }
                if target_offset == head_offset {
                    return sel.clone();
                }
                moved = true;

                let shift_to_prev_char = || {
                    snapshot
                        .rope()
                        .reversed_chars_at(target_offset)
                        .next()
                        .map(|ch| target_offset - ch.len_utf8())
                        .unwrap_or(target_offset)
                };

                if extend {
                    let new_head_offset =
                        if target_offset > head_offset || matches!(target, WordTarget::PrevEnd) {
                            shift_to_prev_char()
                        } else {
                            target_offset
                        };
                    let head_anchor = snapshot.anchor_at(new_head_offset, Bias::Right);
                    return extend_head(
                        sel,
                        head_anchor,
                        new_head_offset,
                        SelectionGoal::None,
                        &snapshot,
                    );
                }

                if target_offset > head_offset {
                    let end_offset = shift_to_prev_char();
                    let tail_anchor = snapshot.anchor_at(head_offset, Bias::Right);
                    let head_anchor = snapshot.anchor_at(end_offset, Bias::Right);
                    Selection {
                        id: sel.id,
                        start: tail_anchor,
                        end: head_anchor,
                        reversed: false,
                        goal: SelectionGoal::None,
                    }
                } else {
                    let resolved_head_offset = if matches!(target, WordTarget::PrevEnd) {
                        shift_to_prev_char()
                    } else {
                        target_offset
                    };
                    let head_anchor = snapshot.anchor_at(resolved_head_offset, Bias::Right);
                    let tail_offset = match snapshot.rope().chars_at(head_offset).next() {
                        Some(ch) => head_offset + ch.len_utf8(),
                        None => head_offset,
                    };
                    let tail_anchor = snapshot.anchor_at(tail_offset, Bias::Right);
                    Selection {
                        id: sel.id,
                        start: head_anchor,
                        end: tail_anchor,
                        reversed: true,
                        goal: SelectionGoal::None,
                    }
                }
            })
            .collect();
        if !moved {
            return;
        }
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Land the primary selection on the `count`th occurrence of
    /// `ch` on the cursor's current line. `kind` picks the direction
    /// and whether to land on the matched char (`NextChar`/`PrevChar`)
    /// or one position adjacent to it (`TillNextChar`/`TillPrevChar`).
    /// `extend = true` keeps the selection anchor in place and moves
    /// only the head. No-op when the char is not found in the
    /// requested direction within the current line.
    pub fn handle_find_char(
        &mut self,
        kind: FindKind,
        ch: char,
        extend: bool,
        count: u32,
        cx: &mut Context<'_, Self>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope();
        let head_offset = {
            let sel = self
                .selections
                .all_anchors()
                .iter()
                .max_by_key(|s| s.id)
                .expect("at least one selection");
            snapshot.resolve_anchor(&sel.head())
        };
        let head_point = rope.offset_to_point(head_offset);
        let max_row = rope.max_point().row;
        let line_start = rope.point_to_offset(stoat_text::Point::new(head_point.row, 0));
        let line_end = if head_point.row >= max_row {
            rope.len()
        } else {
            rope.point_to_offset(stoat_text::Point::new(head_point.row + 1, 0))
                .saturating_sub(1)
        };

        let count = count.max(1);
        let target = match kind {
            FindKind::NextChar | FindKind::TillNextChar => {
                let scan_start = head_offset.saturating_add(
                    rope.chars_at(head_offset)
                        .next()
                        .map_or(0, |c| c.len_utf8()),
                );
                let mut offset = scan_start;
                let mut found = None;
                let mut remaining = count;
                for c in rope.chars_at(scan_start) {
                    if offset >= line_end || c == '\n' {
                        break;
                    }
                    if c == ch {
                        remaining -= 1;
                        if remaining == 0 {
                            found = Some(offset);
                            break;
                        }
                    }
                    offset += c.len_utf8();
                }
                let Some(found) = found else {
                    return;
                };
                if matches!(kind, FindKind::TillNextChar) {
                    rope.reversed_chars_at(found)
                        .next()
                        .map(|c| found - c.len_utf8())
                        .unwrap_or(found)
                } else {
                    found
                }
            },
            FindKind::PrevChar | FindKind::TillPrevChar => {
                let mut offset = head_offset;
                let mut found = None;
                let mut remaining = count;
                for c in rope.reversed_chars_at(head_offset) {
                    if offset == 0 {
                        break;
                    }
                    offset -= c.len_utf8();
                    if offset < line_start || c == '\n' {
                        break;
                    }
                    if c == ch {
                        remaining -= 1;
                        if remaining == 0 {
                            found = Some(offset);
                            break;
                        }
                    }
                }
                let Some(found) = found else {
                    return;
                };
                if matches!(kind, FindKind::TillPrevChar) {
                    let len = rope.chars_at(found).next().map_or(0, |c| c.len_utf8());
                    found + len
                } else {
                    found
                }
            },
        };

        let target_anchor = snapshot.anchor_at(target, Bias::Right);
        let new_disjoint: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                if extend {
                    extend_head(sel, target_anchor, target, sel.goal, &snapshot)
                } else {
                    let mut new = sel.clone();
                    new.collapse_to(target_anchor, SelectionGoal::None);
                    new
                }
            })
            .collect();
        self.selections.replace_with(new_disjoint, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn viewport_rows_for_page(&self) -> u32 {
        let Some(bounds) = self.text_region_bounds else {
            return DEFAULT_VIEWPORT_ROWS;
        };
        let Some(cell) = self.cell_size else {
            return DEFAULT_VIEWPORT_ROWS;
        };
        let line_height = f32::from(cell.height);
        if line_height <= 0.0 {
            return DEFAULT_VIEWPORT_ROWS;
        }
        let rows = (f32::from(bounds.size.height) / line_height).floor() as u32;
        rows.max(1)
    }
}

pub(super) fn extend_head(
    sel: &Selection<Anchor>,
    new_head: Anchor,
    new_head_offset: usize,
    goal: SelectionGoal,
    buffer: &MultiBufferSnapshot,
) -> Selection<Anchor> {
    let tail_anchor = sel.tail();
    let tail_offset = buffer.resolve_anchor(&tail_anchor);
    let mut new = sel.clone();
    new.goal = goal;
    if new_head_offset < tail_offset {
        new.start = new_head;
        new.end = tail_anchor;
        new.reversed = true;
    } else {
        new.start = tail_anchor;
        new.end = new_head;
        new.reversed = false;
    }
    new
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        multi_buffer::MultiBuffer,
    };
    use gpui::{px, AppContext, Bounds, Entity, Point, TestAppContext};
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
            snapshot.resolve_anchor(&ed.selections().all_anchors()[0].start)
        })
    }

    fn cursor_head_offset(editor: &Entity<Editor>, cx: &mut TestAppContext) -> usize {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            snapshot.resolve_anchor(&ed.selections().all_anchors()[0].head())
        })
    }

    fn cursor_display_row(editor: &Entity<Editor>, cx: &mut TestAppContext) -> u32 {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.display_map().update(cx, |dm, _| dm.snapshot());
            let buffer_snap = ed.multi_buffer().read(cx).snapshot();
            let head_anchor = ed.selections().all_anchors()[0].head();
            let head_point = buffer_snap.point_for_anchor(&head_anchor);
            snapshot.buffer_to_display(head_point).row
        })
    }

    fn selection_span(editor: &Entity<Editor>, cx: &mut TestAppContext) -> (usize, usize, bool) {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = &ed.selections().all_anchors()[0];
            (
                snapshot.resolve_anchor(&sel.start),
                snapshot.resolve_anchor(&sel.end),
                sel.reversed,
            )
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
    fn move_horizontal_right_advances_cursor_by_count() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello world");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_move_horizontal(1, 3, false, cx));

        assert_eq!(cursor_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn move_horizontal_left_walks_backward() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello world");
        seed_at_offset(&editor, &mut cx, 5);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_horizontal(-1, 2, false, cx)
        });

        assert_eq!(cursor_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn move_horizontal_clamps_at_buffer_end() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abc");
        seed_at_offset(&editor, &mut cx, 2);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_horizontal(1, 99, false, cx)
        });

        assert_eq!(cursor_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn move_horizontal_extend_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abcdef");
        seed_at_offset(&editor, &mut cx, 2);

        editor.update(&mut cx, |ed, cx| ed.handle_move_horizontal(1, 2, true, cx));

        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = &ed.selections().all_anchors()[0];
            assert_eq!(snapshot.resolve_anchor(&sel.start), 2);
            assert_eq!(snapshot.resolve_anchor(&sel.end), 4);
            assert!(!sel.reversed);
        });
    }

    #[test]
    fn move_vertical_down_advances_display_row() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abc\ndef\nghi\njkl");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_move_vertical(1, 2, false, cx));

        assert_eq!(cursor_display_row(&editor, &mut cx), 2);
    }

    #[test]
    fn move_vertical_up_walks_backward() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abc\ndef\nghi");
        // Place cursor on "ghi" row.
        seed_at_offset(&editor, &mut cx, 8);

        editor.update(&mut cx, |ed, cx| ed.handle_move_vertical(-1, 1, false, cx));

        assert_eq!(cursor_display_row(&editor, &mut cx), 1);
    }

    #[test]
    fn move_vertical_preserves_goal_across_short_line() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abcdef\nab\nghijkl");
        // Cursor at column 5 of first line ('f').
        seed_at_offset(&editor, &mut cx, 5);

        editor.update(&mut cx, |ed, cx| ed.handle_move_vertical(1, 1, false, cx));
        // Short line: cursor lands on end-of-line at column 2.
        assert_eq!(cursor_head_offset(&editor, &mut cx), 9);

        editor.update(&mut cx, |ed, cx| ed.handle_move_vertical(1, 1, false, cx));
        // Goal column 5 restored on the longer line.
        assert_eq!(cursor_head_offset(&editor, &mut cx), 15);
    }

    #[test]
    fn move_vertical_clamps_at_buffer_top() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abc\ndef");
        seed_at_offset(&editor, &mut cx, 1);

        editor.update(&mut cx, |ed, cx| ed.handle_move_vertical(-1, 5, false, cx));

        assert_eq!(cursor_display_row(&editor, &mut cx), 0);
        assert_eq!(cursor_offset(&editor, &mut cx), 1);
    }

    fn multiline(rows: usize) -> String {
        (0..rows)
            .map(|i| format!("row{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn set_viewport(editor: &Entity<Editor>, cx: &mut TestAppContext, rows: u32) {
        editor.update(cx, |ed, cx| {
            ed.set_cell_size(gpui::size(px(8.0), px(16.0)), cx);
            ed.set_text_region_bounds(
                Bounds {
                    origin: Point::new(px(0.0), px(0.0)),
                    size: gpui::size(px(160.0), px(rows as f32 * 16.0)),
                },
                cx,
            );
        });
    }

    #[test]
    fn page_motion_down_steps_full_viewport() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, &multiline(50));
        seed_at_offset(&editor, &mut cx, 0);
        set_viewport(&editor, &mut cx, 10);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_page_motion(PageDir::Down, false, 1, cx)
        });

        assert_eq!(cursor_display_row(&editor, &mut cx), 10);
    }

    #[test]
    fn page_motion_up_steps_full_viewport() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, &multiline(50));
        // Row 20, column 0 -> offset = sum of "rowN\n" lengths up to that row.
        // "row0\n" = 5, "row1\n" = 5, ..., "row9\n" = 5 (10 rows), then "row10\n" = 6, ...
        // 0..10: 10 * 5 = 50; 10..20: 10 * 6 = 60; total 110.
        seed_at_offset(&editor, &mut cx, 110);
        set_viewport(&editor, &mut cx, 10);
        assert_eq!(cursor_display_row(&editor, &mut cx), 20);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_page_motion(PageDir::Up, false, 1, cx)
        });

        assert_eq!(cursor_display_row(&editor, &mut cx), 10);
    }

    #[test]
    fn page_motion_half_step_advances_half_viewport() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, &multiline(50));
        seed_at_offset(&editor, &mut cx, 0);
        set_viewport(&editor, &mut cx, 10);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_page_motion(PageDir::Down, true, 1, cx)
        });

        assert_eq!(cursor_display_row(&editor, &mut cx), 5);
    }

    #[test]
    fn page_motion_uses_default_viewport_when_bounds_missing() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, &multiline(50));
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_page_motion(PageDir::Down, false, 1, cx)
        });

        assert_eq!(cursor_display_row(&editor, &mut cx), DEFAULT_VIEWPORT_ROWS);
    }

    #[test]
    fn page_motion_down_clamps_at_last_row() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, &multiline(8));
        seed_at_offset(&editor, &mut cx, 0);
        set_viewport(&editor, &mut cx, 10);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_page_motion(PageDir::Down, false, 1, cx)
        });

        assert_eq!(cursor_display_row(&editor, &mut cx), 7);
    }

    #[test]
    fn move_word_next_start_creates_selection() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo bar");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextStart, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (0, 3, false));
        assert_eq!(cursor_head_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn move_word_next_start_repeated_snaps_tail() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo bar baz");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextStart, 1, false, cx)
        });
        assert_eq!(selection_span(&editor, &mut cx), (0, 3, false));

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextStart, 1, false, cx)
        });
        assert_eq!(selection_span(&editor, &mut cx), (3, 7, false));
    }

    #[test]
    fn move_word_next_end_creates_selection() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo bar");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextEnd, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (0, 2, false));
    }

    #[test]
    fn move_word_next_end_at_eof_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo");
        seed_at_offset(&editor, &mut cx, 3);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextEnd, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (3, 3, false));
    }

    #[test]
    fn move_word_prev_start_creates_reversed_selection() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo bar");
        seed_at_offset(&editor, &mut cx, 6);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::PrevStart, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (4, 7, true));
        assert_eq!(cursor_head_offset(&editor, &mut cx), 4);
    }

    #[test]
    fn move_word_prev_start_at_start_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo bar");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::PrevStart, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (0, 0, false));
    }

    #[test]
    fn move_word_prev_end_lands_on_last_char_of_prev_word() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo bar");
        seed_at_offset(&editor, &mut cx, 6);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::PrevEnd, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (2, 7, true));
        assert_eq!(cursor_head_offset(&editor, &mut cx), 2);
    }

    #[test]
    fn move_word_count_accumulates_across_words() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo bar baz qux");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextStart, 3, false, cx)
        });

        // count repeats next_word_start three times (0 -> 4 -> 8 -> 12),
        // then collapses the trailing position to the char before, leaving
        // a selection that spans every word traversed.
        assert_eq!(selection_span(&editor, &mut cx), (0, 11, false));
    }

    #[test]
    fn move_word_long_start_skips_punctuation_within_word() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo.bar baz");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextLongStart, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (0, 7, false));
    }

    #[test]
    fn move_word_short_start_stops_on_punctuation_boundary() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo.bar baz");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextStart, 1, false, cx)
        });

        // next_word_start lands on the '.' (offset 3); the forward branch
        // shifts back one char so the selection covers "foo" only.
        assert_eq!(selection_span(&editor, &mut cx), (0, 2, false));
    }

    #[test]
    fn move_word_long_end_advances_past_punctuation() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo.bar baz");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::NextLongEnd, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (0, 6, false));
    }

    #[test]
    fn move_word_prev_long_start_walks_full_run_backward() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo.bar baz");
        seed_at_offset(&editor, &mut cx, 10);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::PrevLongStart, 1, false, cx)
        });

        assert_eq!(selection_span(&editor, &mut cx), (8, 11, true));
    }

    #[test]
    fn move_word_prev_long_end_lands_just_after_prev_run() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "foo.bar baz");
        seed_at_offset(&editor, &mut cx, 10);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_move_word(WordTarget::PrevLongEnd, 1, false, cx)
        });

        // The backward branch only shifts the head for PrevEnd; long-end
        // leaves the head at the position returned by prev_long_word_end.
        assert_eq!(selection_span(&editor, &mut cx), (7, 11, true));
    }

    #[test]
    fn find_next_char_lands_on_target() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello world");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_find_char(FindKind::NextChar, 'o', false, 1, cx)
        });

        assert_eq!(cursor_head_offset(&editor, &mut cx), 4);
    }

    #[test]
    fn find_next_char_count_advances_to_nth_occurrence() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "abacada");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_find_char(FindKind::NextChar, 'a', false, 3, cx)
        });

        assert_eq!(cursor_head_offset(&editor, &mut cx), 6);
    }

    #[test]
    fn till_next_char_lands_one_before_target() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello world");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_find_char(FindKind::TillNextChar, 'o', false, 1, cx)
        });

        assert_eq!(cursor_head_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn find_prev_char_walks_backward() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello world");
        seed_at_offset(&editor, &mut cx, 9);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_find_char(FindKind::PrevChar, 'l', false, 1, cx)
        });

        assert_eq!(cursor_head_offset(&editor, &mut cx), 3);
    }

    #[test]
    fn till_prev_char_lands_one_after_target() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello world");
        seed_at_offset(&editor, &mut cx, 9);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_find_char(FindKind::TillPrevChar, 'l', false, 1, cx)
        });

        assert_eq!(cursor_head_offset(&editor, &mut cx), 4);
    }

    #[test]
    fn find_next_char_does_not_cross_newline() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello\nworld");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_find_char(FindKind::NextChar, 'w', false, 1, cx)
        });

        assert_eq!(cursor_head_offset(&editor, &mut cx), 0);
    }

    #[test]
    fn find_next_char_no_match_is_noop() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_find_char(FindKind::NextChar, 'z', false, 1, cx)
        });

        assert_eq!(cursor_head_offset(&editor, &mut cx), 0);
    }

    #[test]
    fn find_next_char_extend_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let editor = new_editor(&mut cx, "hello world");
        seed_at_offset(&editor, &mut cx, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.handle_find_char(FindKind::NextChar, 'o', true, 1, cx)
        });

        let (start, end, reversed) = selection_span(&editor, &mut cx);
        assert_eq!((start, end), (0, 4));
        assert!(!reversed);
    }
}
