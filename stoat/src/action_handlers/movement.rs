use crate::{
    action_handlers::focused_editor_mut,
    app::{Stoat, UpdateEffect},
    display_map::DisplayPoint,
    editor_state::EditorState,
    multi_buffer::MultiBufferSnapshot,
    pane::View,
};
use stoat_language::structural_diff::BufferRef;
use stoat_text::{
    cursor_offset, find_number_seeking, next_char_boundary, next_long_word_end,
    next_long_word_start, next_word_end, next_word_start, prev_long_word_end, prev_long_word_start,
    prev_word_end, prev_word_start, Anchor, Bias, NumberKind, Point, Rope, Selection,
    SelectionGoal,
};

#[derive(Copy, Clone, Debug)]
pub(super) enum MoveNavigation {
    FirstSource,
    NextSource,
    PrevSource,
    Target,
}

/// Per-source navigation target carried by [`MoveSummary`]: the line
/// to land on plus the optional foreign-buffer pointer. `buffer ==
/// None` means the source lives in the same file (and same focused
/// editor) as the hunk under the cursor; `buffer == Some(_)` means
/// the source lives in a different file and `move_nav` must open or
/// focus that buffer before positioning the cursor.
#[derive(Clone, Debug)]
pub(super) struct MoveSourceRef {
    pub(super) line: u32,
    pub(super) buffer: Option<BufferRef>,
}

/// Resolved move-provenance summary for the hunk under the editor's
/// cursor. Used by the move-navigation action handlers.
pub(super) struct MoveSummary {
    /// Line the hunk starts on in the buffer.
    pub(super) hunk_line: u32,
    /// Candidate source locations, zero or more.
    pub(super) source_refs: Vec<MoveSourceRef>,
    /// If the hunk is the LHS side of a move, the paired RHS target.
    pub(super) target_ref: Option<MoveSourceRef>,
    /// Number of candidate sources (>1 = ambiguous move).
    pub(super) source_count: usize,
}

pub(super) fn current_move_summary(stoat: &mut Stoat) -> Option<MoveSummary> {
    let editor = focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    let anchor = editor.selections.newest_anchor().start;
    let buffer_snapshot = snapshot.buffer_snapshot();
    let offset = buffer_snapshot.resolve_anchor(&anchor);
    let cursor_line = buffer_snapshot.rope().offset_to_point(offset).row;

    if snapshot.line_diff_status(cursor_line) != crate::host::DiffStatus::Moved {
        return None;
    }
    let detail = snapshot.token_detail_for_line(cursor_line)?;
    let metadata = detail
        .buffer_spans
        .iter()
        .chain(detail.base_spans.iter())
        .find_map(|s| s.move_metadata.clone())?;
    let source_refs: Vec<MoveSourceRef> = metadata
        .sources
        .iter()
        .map(|s| MoveSourceRef {
            line: s.line_range.start,
            buffer: s.buffer.clone(),
        })
        .collect();
    let target_ref = if detail.buffer_spans.is_empty() && !detail.base_spans.is_empty() {
        metadata.sources.first().map(|s| MoveSourceRef {
            line: s.line_range.start,
            buffer: s.buffer.clone(),
        })
    } else {
        None
    };
    Some(MoveSummary {
        hunk_line: cursor_line,
        source_count: metadata.sources.len(),
        source_refs,
        target_ref,
    })
}

pub(super) fn move_nav(stoat: &mut Stoat, nav: MoveNavigation) -> UpdateEffect {
    let Some(summary) = current_move_summary(stoat) else {
        return UpdateEffect::None;
    };
    if summary.source_refs.is_empty() && summary.target_ref.is_none() {
        return UpdateEffect::None;
    }

    let target_ref: Option<MoveSourceRef> = {
        let Some(editor) = focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        match nav {
            MoveNavigation::FirstSource => {
                editor.move_source_cursor = Some((summary.hunk_line, 0));
                summary.source_refs.first().cloned()
            },
            MoveNavigation::NextSource => {
                let idx = match editor.move_source_cursor {
                    Some((line, i)) if line == summary.hunk_line => {
                        (i + 1) % summary.source_refs.len().max(1)
                    },
                    _ => 0,
                };
                editor.move_source_cursor = Some((summary.hunk_line, idx));
                summary.source_refs.get(idx).cloned()
            },
            MoveNavigation::PrevSource => {
                let len = summary.source_refs.len().max(1);
                let idx = match editor.move_source_cursor {
                    Some((line, i)) if line == summary.hunk_line => (i + len - 1) % len,
                    _ => len.saturating_sub(1),
                };
                editor.move_source_cursor = Some((summary.hunk_line, idx));
                summary.source_refs.get(idx).cloned()
            },
            MoveNavigation::Target => summary.target_ref,
        }
    };

    let Some(target_ref) = target_ref else {
        return UpdateEffect::None;
    };

    if let Some(buffer_ref) = target_ref.buffer.as_ref() {
        let focused = stoat.active_workspace().panes.focus();
        if super::file::open_file_in_pane(stoat, focused, &buffer_ref.path).is_none() {
            return UpdateEffect::None;
        }
    }

    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    set_cursor_row(editor, target_ref.line);
    UpdateEffect::Redraw
}

pub(crate) fn set_cursor_row(editor: &mut EditorState, row: u32) {
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let point = Point::new(row, 0);
    let offset = rope.point_to_offset(point);
    let anchor = buffer_snapshot.anchor_at(offset, Bias::Left);
    editor.selections = crate::selection::SelectionsCollection::new();
    editor
        .selections
        .insert_cursor(anchor, SelectionGoal::None, buffer_snapshot);
    editor.scroll_row = row.saturating_sub(2);
}

#[derive(Copy, Clone, Debug)]
pub(super) enum WordTarget {
    NextStart,
    NextEnd,
    PrevStart,
    PrevEnd,
    NextLongStart,
    NextLongEnd,
    PrevLongStart,
    PrevLongEnd,
}

pub(super) fn add_selection_below(stoat: &mut Stoat) -> UpdateEffect {
    add_selection_in_direction(stoat, AddDirection::Below)
}

pub(super) fn add_selection_above(stoat: &mut Stoat) -> UpdateEffect {
    add_selection_in_direction(stoat, AddDirection::Above)
}

#[derive(Copy, Clone)]
enum AddDirection {
    Above,
    Below,
}

fn add_selection_in_direction(stoat: &mut Stoat, dir: AddDirection) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let mut effect = UpdateEffect::None;
    for _ in 0..count {
        match add_selection_in_direction_step(stoat, dir) {
            UpdateEffect::Redraw => effect = UpdateEffect::Redraw,
            UpdateEffect::None => break,
            UpdateEffect::Quit => return UpdateEffect::Quit,
        }
    }
    effect
}

fn add_selection_in_direction_step(stoat: &mut Stoat, dir: AddDirection) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let source = editor.selections.newest_anchor().clone();
    let source_head = source.head();
    let source_point = buffer_snapshot.point_for_anchor(&source_head);
    let source_display = display_snapshot.buffer_to_display(source_point);

    let goal_col = match source.goal {
        SelectionGoal::Column(c) => c,
        SelectionGoal::None => source_display.column,
    };

    let max_row = display_snapshot.max_point().row;
    let mut row = source_display.row;
    let target = loop {
        match dir {
            AddDirection::Below => {
                if row >= max_row {
                    return UpdateEffect::None;
                }
                row += 1;
            },
            AddDirection::Above => {
                if row == 0 {
                    return UpdateEffect::None;
                }
                row -= 1;
            },
        }
        let clamped_col = goal_col.min(display_snapshot.line_len(row));
        let raw = DisplayPoint::new(row, clamped_col);
        let clipped = display_snapshot.clip_point(raw, Bias::Left);
        let Some(buffer_pt) = display_snapshot.display_to_buffer(clipped) else {
            continue;
        };
        let offset = buffer_snapshot.rope().point_to_offset(buffer_pt);
        let anchor = buffer_snapshot.anchor_at(offset, Bias::Right);
        break anchor;
    };

    editor
        .selections
        .insert_cursor(target, SelectionGoal::Column(goal_col), buffer_snapshot);
    UpdateEffect::Redraw
}

pub(super) fn move_horizontal(stoat: &mut Stoat, delta: i32, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        if delta > 0 && extend {
            let tail_offset = buffer_snapshot.resolve_anchor(&sel.tail());
            let cursor = cursor_offset(rope, tail_offset, head_offset);
            let target = step_cursor_right(rope, cursor, count);
            if target == cursor {
                return sel.clone();
            }
            return extend_head_to_cursor(sel, target, SelectionGoal::None, rope, buffer_snapshot);
        }
        let new_offset = if delta > 0 {
            let mut offset = head_offset;
            for ch in rope.chars_at(head_offset).take(count) {
                offset += ch.len_utf8();
            }
            offset
        } else {
            let mut offset = head_offset;
            for ch in rope.reversed_chars_at(head_offset).take(count) {
                offset -= ch.len_utf8();
            }
            offset
        };
        if new_offset == head_offset {
            return sel.clone();
        }
        let anchor = buffer_snapshot.anchor_at(new_offset, Bias::Right);
        if extend {
            extend_head(
                sel,
                anchor,
                new_offset,
                SelectionGoal::None,
                buffer_snapshot,
            )
        } else {
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        }
    });
    UpdateEffect::Redraw
}

pub(super) fn move_vertical(stoat: &mut Stoat, delta: i32, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let delta = (delta as i64).saturating_mul(count as i64);
    // Clip toward the direction of travel so a block row (e.g. a review
    // chunk header) snaps to the buffer row past it rather than back to the
    // one just left, which would strand the cursor at the block boundary.
    let clip_bias = if delta > 0 { Bias::Right } else { Bias::Left };
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let max_row = display_snapshot.max_point().row;
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        let tail_offset = buffer_snapshot.resolve_anchor(&sel.tail());
        let cursor = cursor_offset(rope, tail_offset, head_offset);
        let cursor_display = display_snapshot.buffer_to_display(rope.offset_to_point(cursor));
        let goal_col = match sel.goal {
            SelectionGoal::Column(c) => c,
            SelectionGoal::None => cursor_display.column,
        };
        let new_row_i = (cursor_display.row as i64).saturating_add(delta);
        let new_row = new_row_i.clamp(0, max_row as i64) as u32;
        // A plain j/k at the file edge stays a no-op. An overshooting count
        // jump lands on the clamped edge row rather than doing nothing.
        if new_row == cursor_display.row {
            return sel.clone();
        }
        let clamped_col = goal_col.min(display_snapshot.line_len(new_row));
        let raw = DisplayPoint::new(new_row, clamped_col);
        let clipped = display_snapshot.clip_point(raw, clip_bias);
        let Some(buffer_pt) = display_snapshot.display_to_buffer(clipped) else {
            return sel.clone();
        };
        let offset = rope.point_to_offset(buffer_pt);
        if extend {
            // Keep the block cursor on the last character when the goal column
            // overruns the line, rather than on the line break past it.
            let cursor_target = if clamped_col > 0 && rope.chars_at(offset).next() == Some('\n') {
                rope.reversed_chars_at(offset)
                    .next()
                    .map_or(offset, |c| offset - c.len_utf8())
            } else {
                offset
            };
            extend_head_to_cursor(
                sel,
                cursor_target,
                SelectionGoal::Column(goal_col),
                rope,
                buffer_snapshot,
            )
        } else {
            let anchor = buffer_snapshot.anchor_at(offset, Bias::Right);
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::Column(goal_col));
            new
        }
    });
    UpdateEffect::Redraw
}

pub(super) fn move_word(stoat: &mut Stoat, target: WordTarget, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        // Prev-word motions scan from the block cursor, which a forward
        // selection draws one cell back from the head. Re-base the seed there so
        // `b` and `ge` do not overshoot by the trailing character, matching every
        // other motion and helix's word_move. Next motions keep the raw head.
        let seed = if matches!(
            target,
            WordTarget::PrevStart
                | WordTarget::PrevEnd
                | WordTarget::PrevLongStart
                | WordTarget::PrevLongEnd
        ) {
            cursor_offset(
                rope,
                buffer_snapshot.resolve_anchor(&sel.tail()),
                head_offset,
            )
        } else {
            head_offset
        };
        let mut target_offset = seed;
        for _ in 0..count {
            let next = match target {
                WordTarget::NextStart => next_word_start(rope, target_offset),
                WordTarget::NextEnd => next_word_end(rope, target_offset),
                WordTarget::PrevStart => prev_word_start(rope, target_offset),
                WordTarget::PrevEnd => prev_word_end(rope, target_offset),
                WordTarget::NextLongStart => next_long_word_start(rope, target_offset),
                WordTarget::NextLongEnd => next_long_word_end(rope, target_offset),
                WordTarget::PrevLongStart => prev_long_word_start(rope, target_offset),
                WordTarget::PrevLongEnd => prev_long_word_end(rope, target_offset),
            };
            if next == target_offset {
                break;
            }
            target_offset = next;
        }
        if target_offset == seed {
            return sel.clone();
        }

        let shift_to_prev_char = || {
            rope.reversed_chars_at(target_offset)
                .next()
                .map(|ch| target_offset - ch.len_utf8())
                .unwrap_or(target_offset)
        };

        if extend {
            let new_head_offset = if matches!(target, WordTarget::PrevEnd) {
                shift_to_prev_char()
            } else {
                target_offset
            };
            let head_anchor = buffer_snapshot.anchor_at(new_head_offset, Bias::Right);
            return extend_head(
                sel,
                head_anchor,
                new_head_offset,
                SelectionGoal::None,
                buffer_snapshot,
            );
        }

        if target_offset > seed {
            let end_offset = target_offset;
            let tail_anchor = buffer_snapshot.anchor_at(head_offset, Bias::Right);
            let head_anchor = buffer_snapshot.anchor_at(end_offset, Bias::Right);
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
            let head_anchor = buffer_snapshot.anchor_at(resolved_head_offset, Bias::Right);
            let tail_offset = next_char_boundary(rope, seed);
            let tail_anchor = buffer_snapshot.anchor_at(tail_offset, Bias::Right);
            Selection {
                id: sel.id,
                start: head_anchor,
                end: tail_anchor,
                reversed: true,
                goal: SelectionGoal::None,
            }
        }
    });
    UpdateEffect::Redraw
}

fn extend_head(
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

/// Step a cursor offset right by `count` cells, clamping at the line's last
/// character.
///
/// The cursor never lands on the trailing newline or past the buffer end, so a
/// select-mode forward motion stops at the last character of the line rather
/// than crossing onto the next.
fn step_cursor_right(rope: &Rope, cursor: usize, count: usize) -> usize {
    let mut target = cursor;
    for _ in 0..count {
        let Some(ch) = rope.chars_at(target).next() else {
            break;
        };
        let next = target + ch.len_utf8();
        if rope.chars_at(next).next().is_none_or(|c| c == '\n') {
            break;
        }
        target = next;
    }
    target
}

/// Extend `sel` so its block cursor lands on the cell at `target_cursor`.
///
/// A forward result stores the head one cell past `target_cursor`, so the
/// paint-site [`cursor_offset`] recovers the cell. A reversed result keeps the
/// head on `target_cursor`, where `cursor_offset` is identity. Forward on-cell
/// motions re-base their step on [`cursor_offset`] and route the landing cell
/// through this, so the block cursor renders on the cell moved to rather than
/// one short of it.
fn extend_head_to_cursor(
    sel: &Selection<Anchor>,
    target_cursor: usize,
    goal: SelectionGoal,
    rope: &Rope,
    buffer: &MultiBufferSnapshot,
) -> Selection<Anchor> {
    let tail_offset = buffer.resolve_anchor(&sel.tail());
    let new_head_offset = if target_cursor >= tail_offset {
        next_char_boundary(rope, target_cursor)
    } else {
        target_cursor
    };
    let new_head = buffer.anchor_at(new_head_offset, Bias::Right);
    extend_head(sel, new_head, new_head_offset, goal, buffer)
}

pub(super) fn goto_line_start(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    goto_line_boundary(stoat, LineBoundary::Start, extend)
}

pub(super) fn goto_line_end(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    goto_line_boundary(stoat, LineBoundary::End, extend)
}

#[derive(Copy, Clone)]
enum LineBoundary {
    Start,
    End,
}

fn goto_line_boundary(stoat: &mut Stoat, boundary: LineBoundary, extend: bool) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_anchor = sel.head();
        let head_point = buffer_snapshot.point_for_anchor(&head_anchor);
        let col = match boundary {
            LineBoundary::Start => 0,
            LineBoundary::End => rope.line_len(head_point.row),
        };
        let target_offset = rope.point_to_offset(Point::new(head_point.row, col));
        let anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
        if extend {
            extend_head(
                sel,
                anchor,
                target_offset,
                SelectionGoal::None,
                buffer_snapshot,
            )
        } else {
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        }
    });
    UpdateEffect::Redraw
}

pub(super) fn goto_first_nonwhitespace(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        let tail_offset = buffer_snapshot.resolve_anchor(&sel.tail());
        let head_cursor = cursor_offset(rope, tail_offset, head_offset);
        let row = rope.offset_to_point(head_cursor).row;
        let line_start = rope.point_to_offset(Point::new(row, 0));
        let line_end = rope.point_to_offset(Point::new(row, rope.line_len(row)));

        let mut found = None;
        let mut cursor = line_start;
        for ch in rope.chars_at(line_start) {
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

        if extend {
            extend_head_to_cursor(
                sel,
                target_offset,
                SelectionGoal::None,
                rope,
                buffer_snapshot,
            )
        } else {
            let anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        }
    });
    UpdateEffect::Redraw
}

pub(super) fn goto_file_start(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    super::jump::push_jump(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let target_offset = 0;
    editor.selections.transform(buffer_snapshot, |sel| {
        let anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
        if extend {
            extend_head(
                sel,
                anchor,
                target_offset,
                SelectionGoal::None,
                buffer_snapshot,
            )
        } else {
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        }
    });
    UpdateEffect::Redraw
}

pub(super) fn collapse_selection(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    editor.selections.transform(buffer_snapshot, |sel| {
        let mut new = sel.clone();
        new.collapse_to(sel.head(), sel.goal);
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn flip_selections(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    editor.selections.transform(buffer_snapshot, |sel| {
        let mut new = sel.clone();
        if !new.is_empty() {
            new.reversed = !new.reversed;
        }
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn align_selections(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let entries: Vec<AlignEntry> = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();

        let mut out = Vec::with_capacity(editor.selections.all_anchors().len());
        for sel in editor.selections.all_anchors() {
            let start_offset = buffer_snapshot.resolve_anchor(&sel.start);
            let end_offset = buffer_snapshot.resolve_anchor(&sel.end);
            let start_pt = rope.offset_to_point(start_offset);
            let end_pt = rope.offset_to_point(end_offset);
            if start_pt.row != end_pt.row {
                return UpdateEffect::None;
            }
            let head_pt = if sel.reversed { start_pt } else { end_pt };
            let head_display = display_snapshot.buffer_to_display(head_pt);
            out.push(AlignEntry {
                insert_offset: start_offset,
                head_col: head_display.column,
                head_row: head_display.row,
            });
        }
        out
    };

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    let mut row_indices: Vec<u32> = Vec::new();
    let row_index_for = |row_indices: &mut Vec<u32>, row: u32| -> usize {
        match row_indices.iter().position(|r| *r == row) {
            Some(i) => i,
            None => {
                row_indices.push(row);
                row_indices.len() - 1
            },
        }
    };

    let mut ranked: Vec<RankedEntry> = Vec::with_capacity(entries.len());
    let mut last_row: Option<u32> = None;
    let mut rank: usize = 0;
    for entry in entries {
        if Some(entry.head_row) == last_row {
            rank += 1;
        } else {
            rank = 0;
            last_row = Some(entry.head_row);
        }
        let row_idx = row_index_for(&mut row_indices, entry.head_row);
        ranked.push(RankedEntry {
            insert_offset: entry.insert_offset,
            head_col: entry.head_col,
            row_idx,
            rank,
        });
    }

    let max_rank = ranked
        .iter()
        .map(|e| e.rank)
        .max()
        .expect("entries non-empty");
    let mut offs = vec![0u32; row_indices.len()];
    let mut edits: Vec<(usize, String)> = Vec::new();

    for current_rank in 0..=max_rank {
        let max_col = ranked
            .iter()
            .filter(|e| e.rank == current_rank)
            .map(|e| e.head_col + offs[e.row_idx])
            .max();
        let Some(max_col) = max_col else { continue };

        for entry in ranked.iter().filter(|e| e.rank == current_rank) {
            let actual = entry.head_col + offs[entry.row_idx];
            if max_col > actual {
                let pad = (max_col - actual) as usize;
                edits.push((entry.insert_offset, " ".repeat(pad)));
                offs[entry.row_idx] += pad as u32;
            }
        }
    }

    if edits.is_empty() {
        return UpdateEffect::None;
    }

    edits.sort_by_key(|(offset, _)| *offset);

    let buffer_id = ws.editors.get_mut(editor_id).expect("editor").buffer_id;
    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (offset, text) in edits.iter().rev() {
            guard.edit(*offset..*offset, text);
        }
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| sel.clone());
    UpdateEffect::Redraw
}

struct AlignEntry {
    insert_offset: usize,
    head_col: u32,
    head_row: u32,
}

struct RankedEntry {
    insert_offset: usize,
    head_col: u32,
    row_idx: usize,
    rank: usize,
}

pub(super) fn split_selection_on_newline(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.split_each(buffer_snapshot, |sel| {
        let start_offset = buffer_snapshot.resolve_anchor(&sel.start);
        let end_offset = buffer_snapshot.resolve_anchor(&sel.end);
        if start_offset == end_offset {
            return Vec::new();
        }

        let mut newline_positions: Vec<usize> = Vec::new();
        let mut byte_pos = start_offset;
        for ch in rope.chars_at(start_offset) {
            if byte_pos >= end_offset {
                break;
            }
            if ch == '\n' {
                newline_positions.push(byte_pos);
            }
            byte_pos += ch.len_utf8();
        }

        if newline_positions.is_empty() {
            return Vec::new();
        }

        let mut pieces: Vec<Selection<Anchor>> = Vec::with_capacity(newline_positions.len() + 1);
        let mut prev = start_offset;
        for nl in &newline_positions {
            if *nl > prev {
                pieces.push(Selection {
                    id: 0,
                    start: buffer_snapshot.anchor_at(prev, Bias::Right),
                    end: buffer_snapshot.anchor_at(*nl, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                });
            }
            prev = nl + 1;
        }
        if prev < end_offset {
            pieces.push(Selection {
                id: 0,
                start: buffer_snapshot.anchor_at(prev, Bias::Right),
                end: buffer_snapshot.anchor_at(end_offset, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            });
        }
        pieces
    });
    UpdateEffect::Redraw
}

pub(super) fn switch_case(stoat: &mut Stoat) -> UpdateEffect {
    transform_primary_selection(stoat, toggle_case)
}

pub(super) fn switch_to_uppercase(stoat: &mut Stoat) -> UpdateEffect {
    transform_primary_selection(stoat, str::to_uppercase)
}

pub(super) fn switch_to_lowercase(stoat: &mut Stoat) -> UpdateEffect {
    transform_primary_selection(stoat, str::to_lowercase)
}

fn transform_primary_selection<F>(stoat: &mut Stoat, transform: F) -> UpdateEffect
where
    F: Fn(&str) -> String,
{
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, mut edits) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let edits: Vec<(usize, usize, usize, String)> = editor
            .selections
            .all_anchors()
            .iter()
            .filter_map(|sel| {
                let s = buffer_snapshot.resolve_anchor(&sel.start);
                let e = buffer_snapshot.resolve_anchor(&sel.end);
                if s == e {
                    return None;
                }
                let text = buffer_snapshot.rope().slice(s..e).to_string();
                let new_text = transform(&text);
                if new_text == text {
                    return None;
                }
                Some((sel.id, s, e, new_text))
            })
            .collect();
        (buffer_id, edits)
    };

    if edits.is_empty() {
        return UpdateEffect::None;
    }

    edits.sort_by_key(|(_, s, _, _)| *s);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (_, s, e, new_text) in edits.iter().rev() {
            guard.edit(*s..*e, new_text);
        }
    }

    let edited_ranges: std::collections::HashMap<usize, (usize, usize)> = edits
        .iter()
        .map(|(id, s, _, new_text)| (*id, (*s, *s + new_text.len())))
        .collect();

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some((s, e)) = edited_ranges.get(&sel.id) {
            new.start = new_buf.anchor_at(*s, Bias::Left);
            new.end = new_buf.anchor_at(*e, Bias::Right);
        }
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn increment(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as i64;
    apply_number_delta(stoat, count)
}

pub(super) fn decrement(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as i64;
    apply_number_delta(stoat, -count)
}

fn apply_number_delta(stoat: &mut Stoat, delta: i64) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, mut edits) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let mut seen = std::collections::HashSet::<(usize, usize)>::new();
        let edits: Vec<(usize, usize, usize, String)> = editor
            .selections
            .all_anchors()
            .iter()
            .filter_map(|sel| {
                let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
                let num_match = find_number_seeking(rope, head_offset)?;
                let key = (num_match.range.start, num_match.range.end);
                if !seen.insert(key) {
                    return None;
                }
                let text = rope
                    .slice(num_match.range.start..num_match.range.end)
                    .to_string();
                let new_text = compute_number_delta(&text, num_match.kind, delta)?;
                if new_text == text {
                    return None;
                }
                Some((sel.id, num_match.range.start, num_match.range.end, new_text))
            })
            .collect();
        (buffer_id, edits)
    };

    if edits.is_empty() {
        return UpdateEffect::None;
    }

    edits.sort_by_key(|(_, s, _, _)| *s);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (_, s, e, new_text) in edits.iter().rev() {
            guard.edit(*s..*e, new_text);
        }
    }

    let edited_ranges: std::collections::HashMap<usize, (usize, usize)> = edits
        .iter()
        .map(|(id, s, _, new_text)| (*id, (*s, *s + new_text.len())))
        .collect();

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some((s, e)) = edited_ranges.get(&sel.id) {
            new.start = new_buf.anchor_at(*s, Bias::Left);
            new.end = new_buf.anchor_at(*e, Bias::Right);
        }
        new
    });
    UpdateEffect::Redraw
}

fn compute_number_delta(text: &str, kind: NumberKind, delta: i64) -> Option<String> {
    match kind {
        NumberKind::Decimal => {
            let parsed = text.parse::<i64>().ok()?;
            Some(parsed.saturating_add(delta).to_string())
        },
        _ => {
            let mut chars = text.chars();
            chars.next()?;
            let marker = chars.next()?;
            let body = &text[2..];

            let digits_only: String = body.chars().filter(|c| *c != '_').collect();
            if digits_only.is_empty() {
                return None;
            }

            let parsed = u64::from_str_radix(&digits_only, kind.radix()).ok()?;
            let new_value = if delta < 0 {
                parsed.saturating_sub(delta.unsigned_abs())
            } else {
                parsed.saturating_add(delta as u64)
            };

            let body_uppercase = matches!(kind, NumberKind::Hex)
                && (marker.is_ascii_uppercase()
                    || body
                        .chars()
                        .any(|c| c.is_ascii_uppercase() && c.is_ascii_alphabetic()));
            let new_body = match (kind, body_uppercase) {
                (NumberKind::Hex, true) => format!("{new_value:X}"),
                (NumberKind::Hex, false) => format!("{new_value:x}"),
                (NumberKind::Binary, _) => format!("{new_value:b}"),
                (NumberKind::Octal, _) => format!("{new_value:o}"),
                _ => unreachable!(),
            };

            let padded = if new_body.len() < digits_only.len() {
                format!("{new_body:0>width$}", width = digits_only.len())
            } else {
                new_body
            };

            let formatted = match group_size_for_body(body) {
                Some(g) => regroup_right(&padded, g),
                None => padded,
            };

            Some(format!("0{marker}{formatted}"))
        },
    }
}

fn group_size_for_body(body: &str) -> Option<usize> {
    let trimmed = body.trim_matches('_');
    let last = trimmed.rfind('_')?;
    Some(trimmed.len() - last - 1)
}

fn regroup_right(digits: &str, group_size: usize) -> String {
    let n = digits.len();
    if n == 0 || group_size == 0 || n <= group_size {
        return digits.to_string();
    }
    let first_size = if n.is_multiple_of(group_size) {
        group_size
    } else {
        n % group_size
    };
    let mut out = String::with_capacity(n + (n - 1) / group_size);
    out.push_str(&digits[..first_size]);
    let mut idx = first_size;
    while idx < n {
        out.push('_');
        out.push_str(&digits[idx..idx + group_size]);
        idx += group_size;
    }
    out
}

pub(super) fn delete_selection(stoat: &mut Stoat) -> UpdateEffect {
    delete_selection_impl(stoat, true)
}

pub(super) fn delete_selection_no_yank(stoat: &mut Stoat) -> UpdateEffect {
    delete_selection_impl(stoat, false)
}

fn delete_selection_impl(stoat: &mut Stoat, yank: bool) -> UpdateEffect {
    // With yank on, copy the to-be-deleted text to the selected register first,
    // like Helix, so d then p round-trips. Alt-d/Alt-c pass yank off. Runs before
    // ws is borrowed below.
    if yank
        && let Some(fragments) = crate::action_handlers::yank::selection_fragments(stoat)
        && !fragments.is_empty()
    {
        let target = stoat.consume_selected_register();
        crate::action_handlers::yank::write_fragments_to_register(stoat, target, fragments);
    }

    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, mut deletions) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let deletions: Vec<(usize, usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .filter_map(|sel| {
                let s = buffer_snapshot.resolve_anchor(&sel.start);
                let e = buffer_snapshot.resolve_anchor(&sel.end);
                if s != e {
                    Some((sel.id, s, e))
                } else {
                    None
                }
            })
            .collect();
        (buffer_id, deletions)
    };

    if deletions.is_empty() {
        return UpdateEffect::None;
    }

    deletions.sort_by_key(|(_, s, _)| *s);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (_, s, e) in deletions.iter().rev() {
            guard.edit(*s..*e, "");
        }
    }

    let deleted_ids: std::collections::HashSet<usize> =
        deletions.iter().map(|(id, _, _)| *id).collect();

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if deleted_ids.contains(&sel.id) {
            let post_offset = new_buf.resolve_anchor(&sel.start);
            let anchor = new_buf.anchor_at(post_offset, Bias::Left);
            new.start = anchor;
            new.end = anchor;
            new.reversed = false;
            new.goal = SelectionGoal::None;
        }
        new
    });
    UpdateEffect::Redraw
}

/// Yank and delete every non-empty selection. When every selection covered
/// whole lines, open a fresh auto-indented line above the deletion so a
/// following insert types on its own line, matching Helix's linewise change. A
/// partial-line selection is deleted in place.
pub(super) fn change_selection(stoat: &mut Stoat) -> UpdateEffect {
    let whole_lines = selections_are_whole_lines(stoat);
    let deleted = delete_selection_impl(stoat, true);
    if whole_lines {
        open_line(stoat, OpenDir::Above)
    } else {
        deleted
    }
}

/// Whether the focused editor has selections and every one spans whole lines:
/// starting at a line start and ending at a later line start or the buffer end.
/// Empty and partial-line selections make it false, matching Helix's
/// `selection_is_linewise`. False when the focused pane is not an editor.
fn selections_are_whole_lines(stoat: &mut Stoat) -> bool {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Editor(editor_id) = ws.panes.pane(focused).view else {
        return false;
    };
    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let rope = buf_snap.rope();

    let mut any = false;
    for sel in editor.selections.all_anchors().iter() {
        any = true;
        let s = buf_snap.resolve_anchor(&sel.start);
        let e = buf_snap.resolve_anchor(&sel.end);
        let (lo, hi) = if s <= e { (s, e) } else { (e, s) };
        if lo >= hi {
            return false;
        }
        let start = rope.offset_to_point(lo);
        let end = rope.offset_to_point(hi);
        let whole =
            start.column == 0 && end.row > start.row && (end.column == 0 || hi == rope.len());
        if !whole {
            return false;
        }
    }
    any
}

#[derive(Copy, Clone, Debug)]
pub(super) enum OpenDir {
    Above,
    Below,
}

/// One new line to open. It records the selection it belongs to, the offset
/// where its `\n` (with any leading indent) is inserted, the inserted text, and
/// how far past the insert point the cursor lands.
struct OpenInsert {
    id: usize,
    offset: usize,
    text: String,
    cursor_within: usize,
}

pub(super) fn open_line(stoat: &mut Stoat, dir: OpenDir) -> UpdateEffect {
    let editor_id = {
        let ws = stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => return UpdateEffect::None,
        }
    };

    let (buffer_id, entries) = {
        let ws = stoat.active_workspace_mut();
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let mut seen_rows = std::collections::HashSet::new();
        let mut entries: Vec<(usize, usize, u32)> = Vec::new();
        for sel in editor.selections.all_anchors().iter() {
            let head_point = buffer_snapshot.point_for_anchor(&sel.head());
            if !seen_rows.insert(head_point.row) {
                continue;
            }
            let insert_offset = match dir {
                OpenDir::Above => rope.point_to_offset(Point::new(head_point.row, 0)),
                OpenDir::Below => {
                    rope.point_to_offset(Point::new(head_point.row, rope.line_len(head_point.row)))
                },
            };
            entries.push((sel.id, insert_offset, head_point.row));
        }
        (buffer_id, entries)
    };

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    // A line opened below inherits the freshly opened block through the indents
    // query. One opened above copies the current line's indentation.
    let mut inserts: Vec<OpenInsert> = entries
        .iter()
        .map(|&(id, offset, row)| {
            let (text, cursor_within) = match dir {
                OpenDir::Below => {
                    let indent = stoat.newline_indent_string(buffer_id, offset);
                    (format!("\n{indent}"), 1 + indent.len())
                },
                OpenDir::Above => {
                    let indent = stoat.line_indent_string(buffer_id, row);
                    (format!("{indent}\n"), indent.len())
                },
            };
            OpenInsert {
                id,
                offset,
                text,
                cursor_within,
            }
        })
        .collect();
    inserts.sort_by_key(|i| i.offset);

    {
        let ws = stoat.active_workspace_mut();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for ins in inserts.iter().rev() {
            guard.edit(ins.offset..ins.offset, &ins.text);
        }
    }

    let bias = match dir {
        OpenDir::Above => Bias::Left,
        OpenDir::Below => Bias::Right,
    };
    let by_id: std::collections::HashMap<usize, (usize, usize)> = inserts
        .iter()
        .map(|i| (i.id, (i.offset, i.cursor_within)))
        .collect();

    let ws = stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some(&(pre_offset, cursor_within)) = by_id.get(&sel.id) {
            let earlier_len: usize = inserts
                .iter()
                .filter(|i| i.offset < pre_offset)
                .map(|i| i.text.len())
                .sum();
            let cursor_offset = pre_offset + earlier_len + cursor_within;
            let anchor = new_buf.anchor_at(cursor_offset, bias);
            new.start = anchor;
            new.end = anchor;
            new.reversed = false;
            new.goal = SelectionGoal::None;
        }
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn set_pending_replace(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_replace = true;
    UpdateEffect::Redraw
}

pub(crate) fn execute_replace(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, mut entries) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let entries: Vec<(usize, usize, usize, String)> = editor
            .selections
            .all_anchors()
            .iter()
            .filter_map(|sel| {
                let s = buffer_snapshot.resolve_anchor(&sel.start);
                let e = buffer_snapshot.resolve_anchor(&sel.end);
                if s == e {
                    return None;
                }
                let mut chars = 0usize;
                let mut byte_pos = s;
                for c in rope.chars_at(s) {
                    if byte_pos >= e {
                        break;
                    }
                    byte_pos += c.len_utf8();
                    chars += 1;
                }
                let mut replacement = String::with_capacity(chars * ch.len_utf8());
                for _ in 0..chars {
                    replacement.push(ch);
                }
                Some((sel.id, s, e, replacement))
            })
            .collect();
        (buffer_id, entries)
    };

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    entries.sort_by_key(|(_, s, _, _)| *s);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (_, s, e, text) in entries.iter().rev() {
            guard.edit(*s..*e, text);
        }
    }

    let mut id_to_post: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::with_capacity(entries.len());
    let mut shift: i64 = 0;
    for (id, s, e, text) in entries.iter() {
        let post_start = (*s as i64 + shift) as usize;
        let post_end = post_start + text.len();
        id_to_post.insert(*id, (post_start, post_end));
        shift += text.len() as i64 - (*e as i64 - *s as i64);
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some(&(post_start, post_end)) = id_to_post.get(&sel.id) {
            let start_anchor = new_buf.anchor_at(post_start, Bias::Left);
            let end_anchor = new_buf.anchor_at(post_end, Bias::Right);
            new.start = start_anchor;
            new.end = end_anchor;
            new.goal = SelectionGoal::None;
        }
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn undo(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    apply_buffer_history(stoat, count, |buf| buf.undo())
}

pub(super) fn redo(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    apply_buffer_history(stoat, count, |buf| buf.redo())
}

pub(super) fn commit_undo_checkpoint(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };
    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let mut guard = buffer.write().expect("poisoned");
    guard.checkpoint(None);
    UpdateEffect::None
}

fn apply_buffer_history<F>(stoat: &mut Stoat, count: u32, op: F) -> UpdateEffect
where
    F: Fn(&mut crate::buffer::TextBuffer) -> bool,
{
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;

    let any_changed = {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let mut any = false;
        for _ in 0..count {
            if !op(&mut guard) {
                break;
            }
            any = true;
        }
        any
    };

    if !any_changed {
        return UpdateEffect::None;
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| sel.clone());
    UpdateEffect::Redraw
}

pub(super) fn toggle_comments(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let Some(language) = ws.buffers.language_for(buffer_id) else {
        return UpdateEffect::None;
    };
    let Some(prefix) = language.line_comment else {
        return UpdateEffect::None;
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let mut rows: Vec<u32> = Vec::new();
    for sel in editor.selections.all_anchors() {
        let start_offset = buffer_snapshot.resolve_anchor(&sel.start);
        let end_offset = buffer_snapshot.resolve_anchor(&sel.end);
        let start_row = rope.offset_to_point(start_offset).row;
        let end_point = rope.offset_to_point(end_offset);
        let end_row = if end_offset > start_offset && end_point.column == 0 {
            end_point.row.saturating_sub(1)
        } else {
            end_point.row
        };
        for row in start_row..=end_row {
            rows.push(row);
        }
    }
    rows.sort_unstable();
    rows.dedup();

    let mut edits: Vec<(usize, usize, String)> = Vec::with_capacity(rows.len());
    for row in rows {
        let line_start = rope.point_to_offset(Point::new(row, 0));
        let line_len = rope.line_len(row) as usize;
        let line_end = line_start + line_len;
        let mut content_start = line_start;
        for ch in rope.chars_at(line_start) {
            if content_start >= line_end {
                break;
            }
            if !ch.is_whitespace() {
                break;
            }
            content_start += ch.len_utf8();
        }
        if content_start >= line_end {
            continue;
        }

        let after_prefix = content_start + prefix.len();
        let prefix_matches = after_prefix <= line_end
            && rope
                .chars_at(content_start)
                .take(prefix.chars().count())
                .collect::<String>()
                == prefix;

        if prefix_matches {
            let next_char_offset = after_prefix;
            let next_char = rope.chars_at(next_char_offset).next();
            let drop_trailing_space = matches!(next_char, Some(' '));
            let remove_end = if drop_trailing_space {
                next_char_offset + 1
            } else {
                next_char_offset
            };
            edits.push((content_start, remove_end, String::new()));
        } else {
            edits.push((content_start, content_start, format!("{prefix} ")));
        }
    }

    if edits.is_empty() {
        return UpdateEffect::None;
    }

    edits.sort_by_key(|(start, _, _)| *start);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (start, end, replacement) in edits.iter().rev() {
            guard.edit(*start..*end, replacement);
        }
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| sel.clone());
    UpdateEffect::Redraw
}

pub(super) fn indent_selection(stoat: &mut Stoat) -> UpdateEffect {
    apply_line_indent(stoat, IndentDir::In)
}

pub(super) fn unindent_selection(stoat: &mut Stoat) -> UpdateEffect {
    apply_line_indent(stoat, IndentDir::Out)
}

#[derive(Copy, Clone)]
enum IndentDir {
    In,
    Out,
}

const INDENT_WIDTH: usize = 4;

fn apply_line_indent(stoat: &mut Stoat, dir: IndentDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, mut edits) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();

        let mut rows: Vec<u32> = Vec::new();
        for sel in editor.selections.all_anchors() {
            let start_offset = buffer_snapshot.resolve_anchor(&sel.start);
            let end_offset = buffer_snapshot.resolve_anchor(&sel.end);
            let start_row = rope.offset_to_point(start_offset).row;
            let end_point = rope.offset_to_point(end_offset);
            let end_row = if end_offset > start_offset && end_point.column == 0 {
                end_point.row.saturating_sub(1)
            } else {
                end_point.row
            };
            for row in start_row..=end_row {
                rows.push(row);
            }
        }
        rows.sort_unstable();
        rows.dedup();

        let mut edits: Vec<(usize, usize, String)> = Vec::with_capacity(rows.len());
        for row in rows {
            let line_start = rope.point_to_offset(Point::new(row, 0));
            match dir {
                IndentDir::In => {
                    edits.push((line_start, line_start, "\t".repeat(count)));
                },
                IndentDir::Out => {
                    let head: Vec<char> = rope
                        .chars_at(line_start)
                        .take(count.saturating_mul(INDENT_WIDTH))
                        .collect();
                    let mut consumed = 0usize;
                    let mut idx = 0usize;
                    for _ in 0..count {
                        if idx >= head.len() {
                            break;
                        }
                        match head[idx] {
                            '\t' => {
                                idx += 1;
                                consumed += 1;
                            },
                            ' ' => {
                                let group_start = idx;
                                while idx < head.len()
                                    && head[idx] == ' '
                                    && idx - group_start < INDENT_WIDTH
                                {
                                    idx += 1;
                                }
                                consumed += idx - group_start;
                            },
                            _ => break,
                        }
                    }
                    if consumed > 0 {
                        edits.push((line_start, line_start + consumed, String::new()));
                    }
                },
            }
        }
        (buffer_id, edits)
    };

    if edits.is_empty() {
        return UpdateEffect::None;
    }

    edits.sort_by_key(|(start, _, _)| *start);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (start, end, replacement) in edits.iter().rev() {
            guard.edit(*start..*end, replacement);
        }
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| sel.clone());
    UpdateEffect::Redraw
}

fn toggle_case(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_lowercase() {
                c.to_uppercase().collect::<Vec<_>>()
            } else if c.is_uppercase() {
                c.to_lowercase().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

pub(super) fn select_all(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let end_offset = buffer_snapshot.rope().len();
    let start_anchor = buffer_snapshot.anchor_at(0, Bias::Left);
    let end_anchor = buffer_snapshot.anchor_at(end_offset, Bias::Right);
    editor
        .selections
        .set_single_range(start_anchor, end_anchor, SelectionGoal::None);
    UpdateEffect::Redraw
}

pub(super) fn select_line_below(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let max_row = rope.max_point().row;
    let rope_len = rope.len();

    editor.selections.transform(buffer_snapshot, |sel| {
        let line_start = |row: u32| -> usize {
            if row > max_row {
                rope_len
            } else {
                rope.point_to_offset(Point::new(row, 0))
            }
        };

        let start_offset = buffer_snapshot.resolve_anchor(&sel.start);
        let end_offset = buffer_snapshot.resolve_anchor(&sel.end);
        let start_row = rope.offset_to_point(start_offset).row;
        let end_point = rope.offset_to_point(end_offset);
        let end_row = if end_offset > start_offset && end_point.column == 0 {
            end_point.row.saturating_sub(1)
        } else {
            end_point.row
        };

        let current_line_start = line_start(start_row);
        let current_line_end = line_start(end_row + 1);
        let already_line_shaped =
            start_offset == current_line_start && end_offset == current_line_end;

        let extension_rows = if already_line_shaped {
            count
        } else {
            count.saturating_sub(1)
        };
        let target_end_row = end_row.saturating_add(extension_rows);
        let new_end_offset = line_start(target_end_row.saturating_add(1));

        let start_anchor = buffer_snapshot.anchor_at(current_line_start, Bias::Left);
        let end_anchor = buffer_snapshot.anchor_at(new_end_offset, Bias::Right);
        let mut new = sel.clone();
        new.start = start_anchor;
        new.end = end_anchor;
        new.reversed = false;
        new.goal = SelectionGoal::None;
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn keep_primary_selection(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    editor.selections.keep_primary();
    UpdateEffect::Redraw
}

pub(super) fn remove_primary_selection(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    editor.selections.remove_primary();
    UpdateEffect::Redraw
}

pub(super) fn rotate_selections_forward(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    editor.selections.rotate_primary_by(true, count);
    UpdateEffect::Redraw
}

pub(super) fn rotate_selections_backward(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    editor.selections.rotate_primary_by(false, count);
    UpdateEffect::Redraw
}

pub(super) fn trim_selections(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let trimmed: Vec<Selection<Anchor>> = editor
        .selections
        .all_anchors()
        .iter()
        .filter_map(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            let (new_start, new_end) = trim_whitespace(rope, start, end)?;

            let mut new = sel.clone();
            new.start = buffer_snapshot.anchor_at(new_start, Bias::Left);
            new.end = buffer_snapshot.anchor_at(new_end, Bias::Right);
            Some(new)
        })
        .collect();

    if trimmed.is_empty() {
        editor.selections.transform(buffer_snapshot, |sel| {
            let mut new = sel.clone();
            new.collapse_to(sel.head(), sel.goal);
            new
        });
        editor.selections.keep_primary();
    } else {
        editor.selections.replace_with(trimmed, buffer_snapshot);
    }
    UpdateEffect::Redraw
}

/// Skip leading and trailing whitespace within `[start, end)`. Returns
/// `None` if the range is empty or contains only whitespace.
fn trim_whitespace(rope: &Rope, start: usize, end: usize) -> Option<(usize, usize)> {
    if start >= end {
        return None;
    }
    let mut new_start: Option<usize> = None;
    let mut last_non_ws_end: Option<usize> = None;
    let mut cursor = start;
    for ch in rope.chars_at(start) {
        if cursor >= end {
            break;
        }
        let next_cursor = cursor + ch.len_utf8();
        if !ch.is_whitespace() {
            new_start.get_or_insert(cursor);
            last_non_ws_end = Some(next_cursor);
        }
        cursor = next_cursor;
    }
    Some((new_start?, last_non_ws_end?))
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ChangeDir {
    Next,
    Prev,
}

pub(super) fn expand_selection(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let mut effect = UpdateEffect::None;
    for _ in 0..count {
        match expand_selection_step(stoat) {
            UpdateEffect::Redraw => effect = UpdateEffect::Redraw,
            UpdateEffect::None => break,
            UpdateEffect::Quit => return UpdateEffect::Quit,
        }
    }
    effect
}

fn expand_selection_step(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, sel_start, sel_end) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buffer_snapshot.resolve_anchor(&sel.start);
        let end = buffer_snapshot.resolve_anchor(&sel.end);
        (buffer_id, start, end)
    };

    let target = {
        let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
            return UpdateEffect::None;
        };
        let snapshot = syntax_map.snapshot();
        let Some(layer) = deepest_containing_layer(snapshot, sel_start, sel_end) else {
            return UpdateEffect::None;
        };
        let root = layer.tree.root_node();
        let Some(node) = root.descendant_for_byte_range(sel_start, sel_end) else {
            return UpdateEffect::None;
        };
        let node_range = node.byte_range();
        if node_range.start == sel_start && node_range.end == sel_end {
            match node.parent() {
                Some(parent) => parent.byte_range(),
                None => return UpdateEffect::None,
            }
        } else {
            node_range
        }
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let current_range = sel_start..sel_end;
    if editor.expansion_tip.as_ref() != Some(&current_range) {
        editor.expansion_history.clear();
    }
    editor.expansion_history.push(current_range);
    editor.expansion_tip = Some(target.clone());
    apply_primary_range(editor, target);
    UpdateEffect::Redraw
}

fn deepest_containing_layer(
    snapshot: &stoat_language::SyntaxSnapshot,
    sel_start: usize,
    sel_end: usize,
) -> Option<&stoat_language::SyntaxLayer> {
    snapshot.iter_layers().fold(None, |acc, layer| {
        let start = layer.start_offset as usize;
        let end = layer.end_offset as usize;
        if start <= sel_start && end >= sel_end {
            match acc {
                Some(prev) if prev.depth >= layer.depth => acc,
                _ => Some(layer),
            }
        } else {
            acc
        }
    })
}

pub(super) fn shrink_selection(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let mut target: Option<std::ops::Range<usize>> = None;
    for _ in 0..count {
        match editor.expansion_history.pop() {
            Some(t) => target = Some(t),
            None => break,
        }
    }
    let Some(target) = target else {
        return UpdateEffect::None;
    };
    editor.expansion_tip = Some(target.clone());
    apply_primary_range(editor, target);
    UpdateEffect::Redraw
}

#[derive(Copy, Clone, Debug)]
pub(super) enum SiblingDir {
    Next,
    Prev,
}

pub(super) fn select_sibling(stoat: &mut Stoat, dir: SiblingDir, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, sel_start, sel_end) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buffer_snapshot.resolve_anchor(&sel.start);
        let end = buffer_snapshot.resolve_anchor(&sel.end);
        (buffer_id, start, end)
    };

    let target = {
        let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
            return UpdateEffect::None;
        };
        let snapshot = syntax_map.snapshot();
        let Some(layer) = deepest_containing_layer(snapshot, sel_start, sel_end) else {
            return UpdateEffect::None;
        };
        let root = layer.tree.root_node();
        let Some(node) = root.descendant_for_byte_range(sel_start, sel_end) else {
            return UpdateEffect::None;
        };
        let mut current = node;
        let mut moved = false;
        for _ in 0..count {
            let next = match dir {
                SiblingDir::Next => current.next_named_sibling(),
                SiblingDir::Prev => current.prev_named_sibling(),
            };
            match next {
                Some(s) => {
                    current = s;
                    moved = true;
                },
                None => break,
            }
        }
        if !moved {
            return UpdateEffect::None;
        }
        current.byte_range()
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    if extend {
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let head_offset = match dir {
            SiblingDir::Next => target.end,
            SiblingDir::Prev => target.start,
        };
        let new_head = new_buf.anchor_at(head_offset, Bias::Right);
        editor.selections.transform(new_buf, |sel| {
            extend_head(sel, new_head, head_offset, sel.goal, new_buf)
        });
    } else {
        apply_primary_range(editor, target);
    }
    UpdateEffect::Redraw
}

pub(super) fn select_all_siblings(stoat: &mut Stoat) -> UpdateEffect {
    fan_selections_to_children(stoat, true)
}

pub(super) fn select_all_children(stoat: &mut Stoat) -> UpdateEffect {
    fan_selections_to_children(stoat, false)
}

fn fan_selections_to_children(stoat: &mut Stoat, walk_to_multichild_parent: bool) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let buffer_id = editor.buffer_id;
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
        return UpdateEffect::None;
    };
    let snapshot = syntax_map.snapshot();

    let editor = ws.editors.get_mut(editor_id).expect("editor");
    editor.selections.split_each(buffer_snapshot, |sel| {
        let sel_start = buffer_snapshot.resolve_anchor(&sel.start);
        let sel_end = buffer_snapshot.resolve_anchor(&sel.end);
        let Some(layer) = deepest_containing_layer(snapshot, sel_start, sel_end) else {
            return Vec::new();
        };
        let root = layer.tree.root_node();
        let Some(node) = root.descendant_for_byte_range(sel_start, sel_end) else {
            return Vec::new();
        };
        let parent_node = if walk_to_multichild_parent {
            let mut current = node.parent();
            while let Some(p) = current {
                if p.named_child_count() > 1 {
                    break;
                }
                current = p.parent();
            }
            current
        } else {
            Some(node)
        };
        let Some(parent_node) = parent_node else {
            return Vec::new();
        };
        let mut pieces: Vec<Selection<Anchor>> =
            Vec::with_capacity(parent_node.named_child_count());
        let mut walker = parent_node.walk();
        for child in parent_node.named_children(&mut walker) {
            let range = child.byte_range();
            pieces.push(Selection {
                id: 0,
                start: buffer_snapshot.anchor_at(range.start, Bias::Right),
                end: buffer_snapshot.anchor_at(range.end, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            });
        }
        pieces
    });
    UpdateEffect::Redraw
}

#[derive(Copy, Clone, Debug)]
pub(super) enum NodeBound {
    Start,
    End,
}

pub(super) fn move_to_parent_bound(
    stoat: &mut Stoat,
    bound: NodeBound,
    extend: bool,
) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, sel_start, sel_end) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buffer_snapshot.resolve_anchor(&sel.start);
        let end = buffer_snapshot.resolve_anchor(&sel.end);
        (buffer_id, start, end)
    };

    let target_offset = {
        let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
            return UpdateEffect::None;
        };
        let snapshot = syntax_map.snapshot();
        let Some(layer) = deepest_containing_layer(snapshot, sel_start, sel_end) else {
            return UpdateEffect::None;
        };
        let root = layer.tree.root_node();
        let Some(node) = root.descendant_for_byte_range(sel_start, sel_end) else {
            return UpdateEffect::None;
        };
        let mut current = node;
        let mut moved = false;
        for _ in 0..count {
            match current.parent() {
                Some(p) => {
                    current = p;
                    moved = true;
                },
                None => break,
            }
        }
        if !moved {
            return UpdateEffect::None;
        }
        match bound {
            NodeBound::Start => current.start_byte(),
            NodeBound::End => current.end_byte(),
        }
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    if extend {
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let new_head = new_buf.anchor_at(target_offset, Bias::Right);
        editor.selections.transform(new_buf, |sel| {
            extend_head(sel, new_head, target_offset, sel.goal, new_buf)
        });
    } else {
        apply_primary_range(editor, target_offset..target_offset);
    }
    UpdateEffect::Redraw
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum FindKind {
    NextChar,
    PrevChar,
    TillNextChar,
    TillPrevChar,
}

pub(super) fn set_pending_find(stoat: &mut Stoat, kind: FindKind, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    stoat.pending_find = Some((kind, extend, count));
    UpdateEffect::Redraw
}

pub(super) fn repeat_last_motion(stoat: &mut Stoat) -> UpdateEffect {
    let Some((kind, ch)) = stoat.last_find else {
        return UpdateEffect::None;
    };
    let extend = stoat.focused_mode() == "select";
    let count = stoat.take_pending_count().unwrap_or(1);
    execute_find(stoat, kind, ch, extend, count)
}

pub(crate) fn execute_find(
    stoat: &mut Stoat,
    kind: FindKind,
    ch: char,
    extend: bool,
    count: u32,
) -> UpdateEffect {
    stoat.last_find = Some((kind, ch));
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let head_offset = buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().head());
    let tail_offset = buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().tail());
    let cursor = cursor_offset(rope, tail_offset, head_offset);
    let head_point = rope.offset_to_point(cursor);
    let line_start = rope.point_to_offset(Point::new(head_point.row, 0));
    let max_row = rope.max_point().row;
    let line_end = if head_point.row >= max_row {
        rope.len()
    } else {
        rope.point_to_offset(Point::new(head_point.row + 1, 0))
            .saturating_sub(1)
    };

    let count = count.max(1);
    let target = match kind {
        FindKind::NextChar | FindKind::TillNextChar => {
            let scan_start =
                cursor.saturating_add(rope.chars_at(cursor).next().map_or(0, |c| c.len_utf8()));
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
            let Some(target) = found else {
                return UpdateEffect::None;
            };
            if matches!(kind, FindKind::TillNextChar) {
                rope.reversed_chars_at(target)
                    .next()
                    .map(|c| target - c.len_utf8())
                    .unwrap_or(target)
            } else {
                target
            }
        },
        FindKind::PrevChar | FindKind::TillPrevChar => {
            let mut offset = cursor;
            let mut found = None;
            let mut remaining = count;
            for c in rope.reversed_chars_at(cursor) {
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
            let Some(target) = found else {
                return UpdateEffect::None;
            };
            if matches!(kind, FindKind::TillPrevChar) {
                let len = rope.chars_at(target).next().map_or(0, |c| c.len_utf8());
                target + len
            } else {
                target
            }
        },
    };

    if extend {
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let new_rope = new_buf.rope();
        editor.selections.transform(new_buf, |sel| {
            extend_head_to_cursor(sel, target, sel.goal, new_rope, new_buf)
        });
    } else {
        apply_primary_range(editor, target..target);
    }
    UpdateEffect::Redraw
}

fn apply_primary_range(editor: &mut EditorState, target: std::ops::Range<usize>) {
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    let new_start = new_buf.anchor_at(target.start, Bias::Right);
    let new_end = new_buf.anchor_at(target.end, Bias::Left);
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        new.start = new_start;
        new.end = new_end;
        new.reversed = false;
        new.goal = SelectionGoal::None;
        new
    });
}

pub(super) fn goto_change(stoat: &mut Stoat, dir: ChangeDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let origin = super::jump::live_entry(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let head = editor.selections.newest_anchor().head();
    let cursor_row = buffer_snapshot.point_for_anchor(&head).row;

    let Some(diff_map) = display_snapshot.diff_map() else {
        return UpdateEffect::None;
    };

    let target_row = match dir {
        ChangeDir::Next => {
            let next: Vec<_> = diff_map
                .hunks_in_range(cursor_row.saturating_add(1)..u32::MAX)
                .into_iter()
                .filter(|h| h.buffer_start_line > cursor_row)
                .collect();
            if next.is_empty() {
                None
            } else {
                let idx = (count.saturating_sub(1)).min(next.len() - 1);
                Some(next[idx].buffer_start_line)
            }
        },
        ChangeDir::Prev => {
            let prev: Vec<_> = diff_map
                .hunks_in_range(0..cursor_row)
                .into_iter()
                .filter(|h| h.buffer_start_line < cursor_row)
                .collect();
            if prev.is_empty() {
                None
            } else {
                let idx = prev.len().saturating_sub(count);
                Some(prev[idx].buffer_start_line)
            }
        },
    };
    let Some(target_row) = target_row else {
        return UpdateEffect::None;
    };

    let target_offset = buffer_snapshot
        .rope()
        .point_to_offset(Point::new(target_row, 0));
    editor.selections.transform(buffer_snapshot, |sel| {
        let anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
        let mut new = sel.clone();
        new.collapse_to(anchor, SelectionGoal::None);
        new
    });
    if let Some(entry) = origin {
        super::jump::push_entry(stoat, entry);
    }
    UpdateEffect::Redraw
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ParaDir {
    Next,
    Prev,
}

pub(super) fn goto_paragraph(stoat: &mut Stoat, dir: ParaDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let head = editor.selections.newest_anchor().head();
    let cursor_row = buffer_snapshot.point_for_anchor(&head).row;
    let mut last_content_row = rope.max_point().row;
    if last_content_row > 0 && rope.line_len(last_content_row) == 0 {
        last_content_row -= 1;
    }
    let is_empty = |r: u32| rope.line_len(r) == 0;

    let step = |current: u32| -> Option<u32> {
        match dir {
            ParaDir::Next => {
                if current >= last_content_row {
                    return None;
                }
                let mut row = current;
                while row <= last_content_row && !is_empty(row) {
                    row += 1;
                }
                if row > last_content_row {
                    return None;
                }
                while row <= last_content_row && is_empty(row) {
                    row += 1;
                }
                if row > last_content_row {
                    return None;
                }
                Some(row)
            },
            ParaDir::Prev => {
                if current == 0 {
                    return None;
                }
                let mut row = current - 1;
                while row > 0 && is_empty(row) {
                    row -= 1;
                }
                while row > 0 && !is_empty(row) {
                    row -= 1;
                }
                if is_empty(row) && row < last_content_row {
                    row += 1;
                }
                if row == current {
                    return None;
                }
                Some(row)
            },
        }
    };

    let mut target_row = cursor_row;
    for _ in 0..count {
        match step(target_row) {
            Some(next) => target_row = next,
            None => break,
        }
    }
    if target_row == cursor_row {
        return UpdateEffect::None;
    }

    let target_offset = rope.point_to_offset(Point::new(target_row, 0));
    apply_primary_range(editor, target_offset..target_offset);
    UpdateEffect::Redraw
}

pub(super) fn match_brackets(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, head_offset, ch) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let head = editor.selections.newest_anchor().head();
        let head_offset = buffer_snapshot.resolve_anchor(&head);
        let Some(ch) = rope.chars_at(head_offset).next() else {
            return UpdateEffect::None;
        };
        (buffer_id, head_offset, ch)
    };

    let tree_opt: Option<stoat_language::Tree> = ws
        .buffers
        .syntax_map(buffer_id)
        .and_then(|sm| sm.snapshot().iter_layers().next().map(|l| l.tree.clone()));

    let language = ws.buffers.language_for(buffer_id);
    let bracket_query = language
        .as_ref()
        .and_then(|lang| lang.bracket_query.as_ref());

    // A brackets query captures only structural delimiters, so a bracket inside
    // a string, char, or comment literal resolves to no pair instead of
    // false-matching. When the language ships one it is authoritative and matches
    // from within a pair, not only on a delimiter, so the char under the cursor
    // does not gate the query path. The text scanner below only runs for
    // languages without a query (e.g. toml).
    if let (Some(query), Some(tree)) = (bracket_query, tree_opt.as_ref()) {
        let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();

        let Some(target) =
            stoat_language::matching_bracket(query, tree.root_node(), rope, head_offset)
        else {
            return UpdateEffect::None;
        };

        apply_primary_range(editor, target..target);
        return UpdateEffect::Redraw;
    }

    // No brackets query (e.g. toml). The text scan matches only when the cursor
    // is on a bracket delimiter, Helix's plaintext behavior. From-within
    // matching is a syntax-path feature.
    let Some((open, close, forward)) = bracket_pair(ch) else {
        return UpdateEffect::None;
    };

    if let Some(ref tree) = tree_opt
        && is_in_string_or_comment(tree, head_offset)
    {
        return UpdateEffect::None;
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let Some(target) = scan_bracket_match(
        rope,
        head_offset,
        ch,
        open,
        close,
        forward,
        tree_opt.as_ref(),
    ) else {
        return UpdateEffect::None;
    };

    apply_primary_range(editor, target..target);
    UpdateEffect::Redraw
}

fn bracket_pair(ch: char) -> Option<(char, char, bool)> {
    match ch {
        '(' => Some(('(', ')', true)),
        ')' => Some(('(', ')', false)),
        '[' => Some(('[', ']', true)),
        ']' => Some(('[', ']', false)),
        '{' => Some(('{', '}', true)),
        '}' => Some(('{', '}', false)),
        _ => None,
    }
}

pub(crate) fn is_in_string_or_comment(tree: &stoat_language::Tree, offset: usize) -> bool {
    let Some(mut node) = tree.root_node().descendant_for_byte_range(offset, offset) else {
        return false;
    };
    loop {
        let kind = node.kind();
        if kind.contains("string") || kind.contains("comment") {
            return true;
        }
        match node.parent() {
            Some(p) => node = p,
            None => return false,
        }
    }
}

fn scan_bracket_match(
    rope: &Rope,
    start: usize,
    start_ch: char,
    open: char,
    close: char,
    forward: bool,
    tree: Option<&stoat_language::Tree>,
) -> Option<usize> {
    let mut depth: u32 = 1;
    let in_skip_zone = |offset: usize| match tree {
        Some(t) => is_in_string_or_comment(t, offset),
        None => false,
    };
    if forward {
        let mut cur = start + start_ch.len_utf8();
        for c in rope.chars_at(cur) {
            if (c == open || c == close) && !in_skip_zone(cur) {
                if c == open {
                    depth += 1;
                } else {
                    depth -= 1;
                    if depth == 0 {
                        return Some(cur);
                    }
                }
            }
            cur += c.len_utf8();
        }
        None
    } else {
        let mut cur = start;
        for c in rope.reversed_chars_at(start) {
            cur -= c.len_utf8();
            if (c == open || c == close) && !in_skip_zone(cur) {
                if c == close {
                    depth += 1;
                } else {
                    depth -= 1;
                    if depth == 0 {
                        return Some(cur);
                    }
                }
            }
        }
        None
    }
}

pub(super) fn goto_line_number(stoat: &mut Stoat) -> UpdateEffect {
    let Some(count) = stoat.take_pending_count() else {
        return goto_last_line(stoat, false);
    };
    super::jump::push_jump(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let mut last_row = rope.max_point().row;
    if last_row > 0 && rope.line_len(last_row) == 0 {
        last_row -= 1;
    }
    let zero_indexed = count.saturating_sub(1);
    let target_row = (zero_indexed as u64).min(last_row as u64) as u32;
    let target_offset = rope.point_to_offset(Point::new(target_row, 0));
    apply_primary_range(editor, target_offset..target_offset);
    UpdateEffect::Redraw
}

pub(super) fn goto_column(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let head_offset = buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().head());
    let tail_offset = buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().tail());
    let cursor = cursor_offset(rope, tail_offset, head_offset);
    let row = rope.offset_to_point(cursor).row;
    let line_start = rope.point_to_offset(Point::new(row, 0));
    let line_end = rope.point_to_offset(Point::new(row, rope.line_len(row)));

    let steps = count.saturating_sub(1) as usize;
    let mut target_offset = line_start;
    for ch in rope.chars_at(line_start).take(steps) {
        let next = target_offset + ch.len_utf8();
        if next > line_end {
            break;
        }
        target_offset = next;
    }

    if extend {
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        let new_rope = new_buf.rope();
        editor.selections.transform(new_buf, |sel| {
            extend_head_to_cursor(sel, target_offset, sel.goal, new_rope, new_buf)
        });
    } else {
        apply_primary_range(editor, target_offset..target_offset);
    }
    UpdateEffect::Redraw
}

pub(super) fn goto_last_line(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    super::jump::push_jump(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let mut target_row = rope.max_point().row;
    if target_row > 0 && rope.line_len(target_row) == 0 {
        target_row -= 1;
    }
    let target_offset = rope.point_to_offset(Point::new(target_row, 0));
    editor.selections.transform(buffer_snapshot, |sel| {
        if extend {
            extend_head_to_cursor(
                sel,
                target_offset,
                SelectionGoal::None,
                rope,
                buffer_snapshot,
            )
        } else {
            let anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        }
    });
    UpdateEffect::Redraw
}

#[derive(Copy, Clone, Debug)]
pub(super) enum PageDir {
    Up,
    Down,
}

/// Fallback viewport height when the focused editor has not been
/// rendered yet (e.g. a unit test that dispatches a page action
/// without running a render pass).
pub(crate) const DEFAULT_VIEWPORT_ROWS: u32 = 20;

pub(super) fn page_motion(stoat: &mut Stoat, dir: PageDir, half: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);
    let base_delta = if half { viewport.div_ceil(2) } else { viewport };
    let delta = base_delta.saturating_mul(count);

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let max_row = rope.max_point().row;

    let head = editor.selections.newest_anchor().head();
    let current_row = buffer_snapshot.point_for_anchor(&head).row;
    let target_row = match dir {
        PageDir::Up => current_row.saturating_sub(delta),
        PageDir::Down => current_row.saturating_add(delta).min(max_row),
    };
    if target_row == current_row {
        return UpdateEffect::None;
    }

    let prev = editor.scroll_row;
    let max_scroll = max_scroll_row(display_snapshot.line_count(), viewport);
    editor.scroll_row = match dir {
        PageDir::Up => editor.scroll_row.saturating_sub(delta),
        PageDir::Down => editor.scroll_row.saturating_add(delta).min(max_scroll),
    };

    let target_offset = rope.point_to_offset(Point::new(target_row, 0));
    let target_anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
    editor.selections.transform(buffer_snapshot, |sel| {
        let mut new = sel.clone();
        new.collapse_to(target_anchor, SelectionGoal::None);
        new
    });

    // Ease scroll_offset from the visible position up to the scroll_row target
    // the jump set, so a page motion glides instead of teleporting. The cursor
    // moved with scroll_row by the same delta, so it stays pinned to its screen
    // row and the post-key view-follow is a no-op.
    if editor.scroll_offset.floor() as u32 != prev {
        editor.scroll_offset = prev as f32;
    }
    editor.scroll_velocity = 0.0;
    editor.scroll_glide = true;
    UpdateEffect::Redraw
}

#[derive(Copy, Clone, Debug)]
pub(super) enum WindowAlign {
    Top,
    Center,
    Bottom,
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ViewAlign {
    Top,
    Center,
    Bottom,
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ScrollDir {
    Up,
    Down,
}

pub(super) fn scroll_view(stoat: &mut Stoat, dir: ScrollDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    if scroll_editor(editor, matches!(dir, ScrollDir::Down), count) {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

/// Scrolls `editor` by `count` display rows, down when `down` and up
/// otherwise, clamping `scroll_row` so the last document row stays in view.
/// Returns whether `scroll_row` changed.
///
/// When `scroll_row` changes, resets `scroll_offset` to the new integer row and
/// zeroes `scroll_velocity` and `scroll_glide`, so a keyboard line scroll cancels
/// any in-flight momentum or page glide and keeps the fractional position in step
/// with the integer row.
pub(crate) fn scroll_editor(editor: &mut EditorState, down: bool, count: u32) -> bool {
    let max_scroll = max_scroll_offset(editor) as u32;

    let new_scroll = if down {
        editor.scroll_row.saturating_add(count).min(max_scroll)
    } else {
        editor.scroll_row.saturating_sub(count)
    };
    if new_scroll == editor.scroll_row {
        return false;
    }
    editor.scroll_row = new_scroll;
    editor.scroll_offset = new_scroll as f32;
    editor.scroll_velocity = 0.0;
    editor.scroll_glide = false;
    true
}

/// Impart an inertial scroll impulse to `editor` from one wheel notch, down
/// when `down` and up otherwise.
///
/// Adds a fixed impulse to `scroll_velocity` (clamped), so rapid notches
/// accumulate into a faster glide that the per-frame tick integrates. Re-seeds
/// `scroll_offset` from `scroll_row` first when another path moved the integer
/// row out from under the fraction, so the glide starts from the visible
/// position. Never touches the cursor or `scroll_row`, so a coast never drags
/// the selection.
///
/// Marks the view [`EditorState::scroll_decoupled`], so the next key re-couples
/// the view to the cursor even when the wheel scrolled the cursor off screen.
pub(crate) fn wheel_impulse(editor: &mut EditorState, down: bool) {
    const IMPULSE: f32 = 60.0;
    const MAX_VEL: f32 = 240.0;

    if editor.scroll_offset.floor() as u32 != editor.scroll_row {
        editor.scroll_offset = editor.scroll_row as f32;
    }
    let delta = if down { IMPULSE } else { -IMPULSE };
    editor.scroll_velocity = (editor.scroll_velocity + delta).clamp(-MAX_VEL, MAX_VEL);
    editor.scroll_decoupled = true;
}

/// Largest `scroll_row` (a display row) that keeps the last display row in
/// view for a document of `display_line_count` display rows.
///
/// `display_line_count` counts display rows -- buffer lines plus block rows
/// such as review chunk headers and deleted-line blocks -- so the bound tracks
/// what is on screen rather than the buffer's row count. A clamp on the buffer
/// row count stops one row short per block, stranding the last rows.
fn max_scroll_row(display_line_count: u32, viewport: u32) -> u32 {
    display_line_count
        .saturating_sub(1)
        .saturating_sub(viewport.saturating_sub(1))
}

/// Largest top-row position that keeps the last display row in view, as a
/// float so the integer scroll path and the momentum path clamp to one shared
/// bound.
pub(crate) fn max_scroll_offset(editor: &mut EditorState) -> f32 {
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);
    let display_snapshot = editor.display_map.snapshot();
    max_scroll_row(display_snapshot.line_count(), viewport) as f32
}

/// Advance an inertial scroll by `dt` seconds, integrating `offset` by
/// `velocity * dt` clamped to `[0, max_offset]`, then decaying `velocity`.
///
/// The decay is frame-rate-independent. `FRICTION` is the fraction of velocity
/// kept per `NOMINAL_DT`, raised to `dt / NOMINAL_DT`, so a long frame decays
/// proportionally more and a glide lasts the same wall-clock time however often
/// it ticks. Decaying by a fixed amount per tick instead runs the glide in slow
/// motion whenever the real frame interval overruns `NOMINAL_DT`.
///
/// Returns the new offset, the new velocity (zero once the glide has settled),
/// and whether it settled. Settled is true when the decayed speed falls below
/// the minimum or the offset reaches a bound, so the caller can stop ticking.
pub(crate) fn step_scroll_momentum(
    offset: f32,
    velocity: f32,
    dt: f32,
    max_offset: f32,
) -> (f32, f32, bool) {
    const FRICTION: f32 = 0.85;
    // Below ~2 rows/s the coast is visually stationary. Ending it here hands the
    // last sub-row fraction to the ease glide rather than dribbling to a stop.
    const MIN_VEL: f32 = 2.0;
    const NOMINAL_DT: f32 = 0.008;

    let next = (offset + velocity * dt).clamp(0.0, max_offset);
    let at_bound = next <= 0.0 || next >= max_offset;
    let velocity = velocity * FRICTION.powf(dt / NOMINAL_DT);
    let settled = velocity.abs() < MIN_VEL || at_bound;

    (next, if settled { 0.0 } else { velocity }, settled)
}

/// Ease `offset` toward `target` by `dt` seconds, returning the new offset and
/// whether it settled onto the target.
///
/// A keyboard page motion jumps `scroll_row` to the destination and lets
/// `scroll_offset` glide up to it. Each `NOMINAL_DT` closes `EASE_PER_NOMINAL`
/// of the remaining gap, raised to `dt / NOMINAL_DT` so the glide lasts the same
/// wall-clock time however often it ticks (the frame-rate independence
/// [`step_scroll_momentum`] uses for its decay). Within `EPSILON` of the target
/// it snaps exactly onto it and reports settled, so the caller can stop ticking.
pub(crate) fn step_scroll_ease(offset: f32, target: f32, dt: f32) -> (f32, bool) {
    const EASE_PER_NOMINAL: f32 = 0.35;
    const NOMINAL_DT: f32 = 0.008;
    const EPSILON: f32 = 0.01;

    let kept = (1.0 - EASE_PER_NOMINAL).powf(dt / NOMINAL_DT);
    let next = target - (target - offset) * kept;
    if (target - next).abs() < EPSILON {
        (target, true)
    } else {
        (next, false)
    }
}

/// Collapse every selection to a single point at `offset`. Returns
/// `UpdateEffect::None` when there is no focused editor or `offset`
/// is past the end of the focused buffer.
pub(crate) fn jump_to_offset(stoat: &mut Stoat, offset: usize) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let clamped = offset.min(rope.len());
    let target_anchor = buffer_snapshot.anchor_at(clamped, Bias::Right);
    editor.selections.transform(buffer_snapshot, |sel| {
        let mut new = sel.clone();
        new.collapse_to(target_anchor, SelectionGoal::None);
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn goto_word(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);
    let scroll_row = editor.scroll_row;

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let first_row = scroll_row;
    let last_row = scroll_row.saturating_add(viewport.saturating_sub(1));

    let max_targets = crate::goto_word::ALPHABET.len() * crate::goto_word::ALPHABET.len();
    let targets = crate::goto_word::find_word_starts(rope, first_row, last_row, max_targets);
    let labels = crate::goto_word::assign_labels(&targets, crate::goto_word::ALPHABET);

    if labels.is_empty() {
        stoat.pending_goto_word = None;
        stoat.pending_goto_word_input.clear();
        return UpdateEffect::None;
    }

    stoat.pending_goto_word = Some(labels);
    stoat.pending_goto_word_input.clear();
    UpdateEffect::Redraw
}

pub(super) fn align_view(stoat: &mut Stoat, align: ViewAlign) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let head = editor.selections.newest_anchor().head();
    let cursor_row = buffer_snapshot.point_for_anchor(&head).row;

    let desired_scroll = match align {
        ViewAlign::Top => cursor_row,
        ViewAlign::Center => cursor_row.saturating_sub(viewport / 2),
        ViewAlign::Bottom => cursor_row.saturating_sub(viewport.saturating_sub(1)),
    };
    let max_scroll = max_scroll_row(display_snapshot.line_count(), viewport);
    editor.scroll_row = desired_scroll.min(max_scroll);
    UpdateEffect::Redraw
}

/// Scroll the viewport the minimum amount to keep the primary cursor at least
/// `scrolloff` display rows from the top and bottom edges, returning whether the
/// view actually moved.
///
/// The central view-follow step. The key loop runs it after a key moves the
/// cursor, so a `50j` or `G` whose target leaves the margin pulls the view along
/// instead of dropping the cursor onto the edge, and also to re-couple a view a
/// mouse-wheel scroll stranded. The clamp self-gates to a no-op when the cursor
/// already sits inside the margin.
///
/// The returned bool lets the caller force a repaint when a wheel-stranded view
/// re-couples on a key that left the cursor put, which a cursor-position gate
/// would miss.
///
/// `scroll_row` is a display row, so the cursor is resolved through the display
/// map (folds and soft-wraps included) rather than its buffer row. The margin is
/// capped to half the viewport so it cannot exceed the space available, and the
/// bottom branch clamps to `max_scroll` so the last document row pins to the
/// bottom rather than scrolling blank space into view.
pub(crate) fn ensure_cursor_in_view(editor: &mut EditorState, scrolloff: u32) -> bool {
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);

    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let head = editor.selections.newest_anchor().head();
    let head_offset = buffer_snapshot.resolve_anchor(&head);
    let cursor_row = snapshot
        .buffer_to_display(rope.offset_to_point(head_offset))
        .row;

    let top = scrolloff.min(viewport.saturating_sub(1) / 2);
    let bottom = scrolloff.min(viewport / 2);
    let max_scroll = max_scroll_row(snapshot.line_count(), viewport);

    let before = editor.scroll_row;
    if cursor_row < editor.scroll_row + top {
        editor.scroll_row = cursor_row.saturating_sub(top);
    } else if cursor_row + bottom >= editor.scroll_row + viewport {
        editor.scroll_row = (cursor_row + bottom + 1)
            .saturating_sub(viewport)
            .min(max_scroll);
    }
    editor.scroll_row != before
}

pub(super) fn goto_window(stoat: &mut Stoat, align: WindowAlign, extend: bool) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);
    let scroll_row = editor.scroll_row;

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let max_row = rope.max_point().row;

    let offset = match align {
        WindowAlign::Top => 0,
        WindowAlign::Center => viewport / 2,
        WindowAlign::Bottom => viewport.saturating_sub(1),
    };
    let target_row = scroll_row.saturating_add(offset).min(max_row);

    let target_offset = rope.point_to_offset(Point::new(target_row, 0));
    editor.selections.transform(buffer_snapshot, |sel| {
        if extend {
            extend_head_to_cursor(
                sel,
                target_offset,
                SelectionGoal::None,
                rope,
                buffer_snapshot,
            )
        } else {
            let target_anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
            let mut new = sel.clone();
            new.collapse_to(target_anchor, SelectionGoal::None);
            new
        }
    });
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        diff_map::{
            ChangeKind as DmChangeKind, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, TokenDetail,
        },
        pane::View,
        test_harness::TestHarness,
    };
    use std::sync::Arc;
    use stoat_language::structural_diff::{MoveMetadata, MoveSource, Side};

    fn install_moved_hunk_to_other_file(
        h: &mut TestHarness,
        moved_line: u32,
        target_path: &std::path::Path,
        target_line: u32,
    ) {
        let buffer_ref = BufferRef {
            path: target_path.to_path_buf(),
            fingerprint: [7u8; 32],
        };
        let metadata = Arc::new(MoveMetadata {
            sources: vec![MoveSource {
                buffer: Some(buffer_ref),
                side: Side::Lhs,
                byte_range: 0..0,
                line_range: target_line..(target_line + 1),
            }],
        });
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 0..0,
                kind: DmChangeKind::Moved,
                move_metadata: Some(metadata),
            }],
            base_spans: Vec::new(),
        });
        let hunk = DiffHunk {
            status: DiffHunkStatus::Moved,
            buffer_start_line: moved_line,
            buffer_line_range: moved_line..(moved_line + 1),
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(detail),
        };
        let dm = DiffMap::from_hunks([hunk], None);
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        guard.diff_map = Some(dm);
    }

    fn focused_buffer_path(h: &TestHarness) -> std::path::PathBuf {
        let ws = h.stoat.active_workspace();
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

    fn focused_head_row(h: &mut TestHarness) -> u32 {
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let offset = buffer_snapshot.resolve_anchor(&head);
        buffer_snapshot.rope().offset_to_point(offset).row
    }

    #[test]
    fn ensure_cursor_in_view_follows_cursor_and_noops_when_visible() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        set_cursor_row(editor, 50);
        editor.scroll_row = 0;
        assert!(
            ensure_cursor_in_view(editor, 0),
            "a below-viewport cursor scrolls the view",
        );
        assert_eq!(
            editor.scroll_row, 41,
            "a below-viewport cursor pulls the view down to it",
        );

        set_cursor_row(editor, 45);
        editor.scroll_row = 41;
        assert!(
            !ensure_cursor_in_view(editor, 0),
            "an already-visible cursor does not scroll",
        );
        assert_eq!(
            editor.scroll_row, 41,
            "an already-visible cursor leaves the view put",
        );

        set_cursor_row(editor, 8);
        editor.scroll_row = 41;
        assert!(
            ensure_cursor_in_view(editor, 0),
            "an above-viewport cursor scrolls the view",
        );
        assert_eq!(
            editor.scroll_row, 8,
            "an above-viewport cursor pulls the view up to it",
        );
    }

    #[test]
    fn ensure_cursor_in_view_holds_scrolloff_margin() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        // A downward jump past the viewport keeps 3 rows below the cursor, so
        // the cursor lands on row 6 of the 10-row view (scroll_row = 50 - 6).
        set_cursor_row(editor, 50);
        editor.scroll_row = 0;
        assert!(ensure_cursor_in_view(editor, 3));
        assert_eq!(
            editor.scroll_row, 44,
            "downward jump keeps a 3-row margin below the cursor",
        );

        // An upward jump keeps 3 rows above the cursor (scroll_row = 8 - 3).
        set_cursor_row(editor, 8);
        editor.scroll_row = 44;
        assert!(ensure_cursor_in_view(editor, 3));
        assert_eq!(
            editor.scroll_row, 5,
            "upward jump keeps a 3-row margin above the cursor",
        );
    }

    #[test]
    fn max_scroll_row_bounds_on_display_rows() {
        assert_eq!(
            max_scroll_row(100, 10),
            90,
            "last display row pins to bottom"
        );
        assert_eq!(max_scroll_row(5, 10), 0, "fewer rows than the viewport");
        assert_eq!(max_scroll_row(0, 10), 0, "empty document");
        assert_eq!(
            max_scroll_row(104, 10),
            94,
            "four block rows raise the bound one-for-one",
        );
    }

    /// A plain editor has no block rows, so the display-row bound equals the
    /// old buffer-row bound exactly and non-review scrolling is unchanged.
    #[test]
    fn max_scroll_offset_matches_buffer_rows_without_blocks() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..30).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("plain.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        let expected = {
            let snapshot = editor.display_map.snapshot();
            let buffer_snapshot = snapshot.buffer_snapshot();
            buffer_snapshot.rope().max_point().row.saturating_sub(9)
        };
        assert_eq!(
            max_scroll_offset(editor) as u32,
            expected,
            "no blocks means the display bound equals the buffer-row bound",
        );
    }

    /// Block rows -- review chunk headers or deleted-line blocks -- add display
    /// rows the buffer does not have. The scroll bound must count them, and the
    /// cursor-follow must reach the last display row, or the last content sits
    /// below a false bottom. A deletion diff is the cache-coherent way to add
    /// block rows in a test (a `diff_version` bump forces the snapshot rebuild).
    #[test]
    fn scroll_bound_reaches_last_row_past_block_rows() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..20).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("diff.rs", &body);
        h.open_file(&path);

        let dm = {
            let mut dm = DiffMap::default();
            dm.set_base_text(Arc::new("a\nb\nc\n".to_string()));
            dm.push_hunk(DiffHunk {
                status: DiffHunkStatus::Deleted,
                buffer_start_line: 1,
                buffer_line_range: 1..1,
                base_byte_range: 0..5,
                anchor_range: None,
                token_detail: None,
            });
            dm
        };
        {
            let ws = h.stoat.active_workspace();
            let focused = ws.panes.focus();
            let editor_id = match ws.panes.pane(focused).view {
                View::Editor(id) => id,
                _ => panic!("focused pane is not an editor"),
            };
            let buffer_id = ws.editors[editor_id].buffer_id;
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            buffer.write().expect("poisoned").diff_map = Some(dm);
        }

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        let (buffer_rows, line_count) = {
            let s = editor.display_map.snapshot();
            (s.buffer_line_count(), s.line_count())
        };
        assert!(
            line_count > buffer_rows,
            "the deletion adds block rows: {line_count} display vs {buffer_rows} buffer",
        );

        assert_eq!(
            max_scroll_offset(editor) as u32,
            line_count - 10,
            "the bound clamps on display rows, not buffer rows",
        );

        set_cursor_row(editor, buffer_rows - 1);
        editor.scroll_row = 0;
        ensure_cursor_in_view(editor, 0);
        assert_eq!(
            editor.scroll_row,
            line_count - 10,
            "following the cursor to the last line reaches the display bound",
        );
    }

    #[test]
    fn wheel_stranded_view_refollows_on_clamped_key() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            // Simulate a settled mouse-wheel scroll. The view moved down and is
            // marked decoupled, stranding the row-0 cursor off screen above it.
            editor.scroll_row = 40;
            wheel_impulse(editor, true);
        }

        // The cursor is on row 0, off-screen above the stranded view, so `k` is
        // a clamped no-op that never moves it -- the view must re-follow anyway.
        h.type_keys("k");

        assert_eq!(
            focused_editor_mut(&mut h.stoat)
                .expect("focused editor")
                .scroll_row,
            0,
            "a clamped no-op key re-couples a wheel-stranded view to the cursor",
        );
    }

    #[test]
    fn snapshot_count_jump_keeps_cursor_visible() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..80).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        h.stoat.pending_count = Some(50);
        h.type_keys("j");
        h.assert_snapshot("count_jump_keeps_cursor_visible");
    }

    #[test]
    fn count_vertical_motion_clamps_at_buffer_edge() {
        let mut h = TestHarness::with_size(40, 12);
        let path = h.write_file("short.rs", "a\nb\nc");
        h.open_file(&path);

        h.stoat.pending_count = Some(10000);
        h.type_keys("j");
        assert_eq!(
            focused_head_row(&mut h),
            2,
            "an overshooting count-down clamps to the last line",
        );

        h.stoat.pending_count = Some(10000);
        h.type_keys("k");
        assert_eq!(
            focused_head_row(&mut h),
            0,
            "an overshooting count-up clamps to the first line",
        );
    }

    #[test]
    fn move_nav_jumps_to_foreign_buffer_path() {
        let mut h = TestHarness::with_size(40, 10);
        let a_path = h.write_file("a.rs", "a0\na1\na2\na3\na4\n");
        let b_path = h.write_file("b.rs", "b0\nb1\nb2\nb3\nb4\n");
        h.open_file(&a_path);
        install_moved_hunk_to_other_file(&mut h, 2, &b_path, 3);

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            set_cursor_row(editor, 2);
        }
        assert_eq!(
            focused_head_row(&mut h),
            2,
            "cursor on the moved hunk in a.rs"
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveSource);

        assert_eq!(
            focused_buffer_path(&h),
            b_path,
            "focused pane switched to b.rs"
        );
        assert_eq!(
            focused_head_row(&mut h),
            3,
            "cursor on the source line in b.rs"
        );
    }

    #[test]
    fn move_nav_intra_file_stays_in_buffer() {
        let mut h = TestHarness::with_size(40, 10);
        let a_path = h.write_file("a.rs", "a0\na1\na2\na3\na4\n");
        h.open_file(&a_path);

        let metadata = Arc::new(MoveMetadata {
            sources: vec![MoveSource {
                buffer: None,
                side: Side::Lhs,
                byte_range: 0..0,
                line_range: 4..5,
            }],
        });
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 0..0,
                kind: DmChangeKind::Moved,
                move_metadata: Some(metadata),
            }],
            base_spans: Vec::new(),
        });
        let hunk = DiffHunk {
            status: DiffHunkStatus::Moved,
            buffer_start_line: 2,
            buffer_line_range: 2..3,
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: Some(detail),
        };
        {
            let dm = DiffMap::from_hunks([hunk], None);
            let ws = h.stoat.active_workspace();
            let focused = ws.panes.focus();
            let editor_id = match ws.panes.pane(focused).view {
                View::Editor(id) => id,
                _ => panic!("focused pane is not an editor"),
            };
            let buffer_id = ws.editors[editor_id].buffer_id;
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let mut guard = buffer.write().expect("poisoned");
            guard.diff_map = Some(dm);
        }

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            set_cursor_row(editor, 2);
        }
        assert_eq!(focused_head_row(&mut h), 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveSource);

        assert_eq!(
            focused_buffer_path(&h),
            a_path,
            "stayed in a.rs (intra-file move)"
        );
        assert_eq!(
            focused_head_row(&mut h),
            4,
            "cursor on the source line in a.rs"
        );
    }

    #[test]
    fn momentum_from_rest_advances_and_decays_to_settled_rest() {
        let (mut offset, mut velocity) = (0.0_f32, 20.0_f32);
        let mut last_speed = f32::INFINITY;
        loop {
            let (next, vel, settled) = step_scroll_momentum(offset, velocity, 0.05, 1000.0);
            assert!(next > offset, "offset advances under positive velocity");
            assert!(
                vel.abs() < last_speed,
                "velocity magnitude decays each step"
            );
            offset = next;
            velocity = vel;
            last_speed = vel.abs();
            if settled {
                break;
            }
        }
        assert_eq!(velocity, 0.0, "settled state has zero velocity");
    }

    #[test]
    fn momentum_into_bound_clamps_offset_and_settles() {
        assert_eq!(
            step_scroll_momentum(95.0, 50.0, 1.0, 100.0),
            (100.0, 0.0, true)
        );
    }

    #[test]
    fn momentum_decay_is_frame_rate_independent() {
        let (_, one_step, _) = step_scroll_momentum(0.0, 100.0, 0.016, 1000.0);
        let (_, half, _) = step_scroll_momentum(0.0, 100.0, 0.008, 1000.0);
        let (_, two_steps, _) = step_scroll_momentum(0.0, half, 0.008, 1000.0);
        assert!(
            (one_step - two_steps).abs() < 0.01,
            "one 16ms decay {one_step} should equal two 8ms decays {two_steps}"
        );
    }

    #[test]
    fn ease_advances_toward_target_and_settles() {
        let mut offset = 0.0_f32;
        let target = 10.0_f32;
        let mut last_gap = f32::INFINITY;
        let mut settled = false;
        for _ in 0..1000 {
            let (next, done) = step_scroll_ease(offset, target, 0.016);
            let gap = (target - next).abs();
            assert!(gap < last_gap, "each ease step closes the gap");
            offset = next;
            last_gap = gap;
            if done {
                settled = true;
                break;
            }
        }
        assert!(settled, "the ease settles");
        assert_eq!(offset, target, "it settles exactly on the target");
    }

    #[test]
    fn ease_is_frame_rate_independent() {
        let (one_step, _) = step_scroll_ease(0.0, 10.0, 0.016);
        let (half, _) = step_scroll_ease(0.0, 10.0, 0.008);
        let (two_steps, _) = step_scroll_ease(half, 10.0, 0.008);
        assert!(
            (one_step - two_steps).abs() < 0.01,
            "one 16ms ease {one_step} should equal two 8ms eases {two_steps}"
        );
    }

    fn harness_with_long_buffer() -> TestHarness {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("glide.rs", &body);
        h.open_file(&path);
        h
    }

    #[test]
    fn wheel_impulse_builds_clamped_velocity() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");

        wheel_impulse(editor, true);
        let v1 = editor.scroll_velocity;
        assert!(v1 > 0.0, "a down notch imparts positive velocity");

        wheel_impulse(editor, true);
        assert!(editor.scroll_velocity > v1, "rapid notches accumulate");

        for _ in 0..50 {
            wheel_impulse(editor, true);
        }
        let saturated = editor.scroll_velocity;
        wheel_impulse(editor, true);
        assert!(
            editor.scroll_velocity <= saturated,
            "velocity saturates at the clamp"
        );

        editor.scroll_velocity = 0.0;
        wheel_impulse(editor, false);
        assert!(
            editor.scroll_velocity < 0.0,
            "an up notch imparts negative velocity"
        );
    }

    #[test]
    fn wheel_impulse_reseeds_offset_when_scroll_row_drifted() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");

        editor.scroll_row = 10;
        editor.scroll_offset = 0.0;
        wheel_impulse(editor, true);
        assert_eq!(
            editor.scroll_offset as u32, 10,
            "a drifted offset reseeds from scroll_row before gliding"
        );
    }

    #[test]
    fn keyboard_scroll_syncs_offset_and_clears_momentum() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");

        editor.scroll_velocity = 50.0;
        assert!(
            scroll_editor(editor, true, 3),
            "scrolling down moves scroll_row"
        );
        assert_eq!(
            editor.scroll_velocity, 0.0,
            "keyboard scroll clears momentum"
        );
        assert_eq!(
            editor.scroll_offset as u32, editor.scroll_row,
            "offset syncs to the integer row"
        );
    }
}
