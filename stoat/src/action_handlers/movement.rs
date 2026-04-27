use crate::{
    action_handlers::focused_editor_mut,
    app::{Stoat, UpdateEffect},
    display_map::DisplayPoint,
    editor_state::EditorState,
    multi_buffer::MultiBufferSnapshot,
    pane::View,
};
use stoat_text::{
    find_decimal_number_at, next_long_word_end, next_long_word_start, next_word_end,
    next_word_start, prev_long_word_end, prev_long_word_start, prev_word_end, prev_word_start,
    Anchor, Bias, Point, Selection, SelectionGoal,
};

#[derive(Copy, Clone, Debug)]
pub(super) enum MoveNavigation {
    FirstSource,
    NextSource,
    PrevSource,
    Target,
}

/// Resolved move-provenance summary for the hunk under the editor's
/// cursor. Used by the move-navigation action handlers.
pub(super) struct MoveSummary {
    /// Line the hunk starts on in the buffer.
    pub(super) hunk_line: u32,
    /// Candidate source line numbers, zero or more.
    pub(super) source_lines: Vec<u32>,
    /// If the hunk is the LHS side of a move, the paired RHS target line.
    pub(super) target_line: Option<u32>,
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
    let source_lines: Vec<u32> = metadata
        .sources
        .iter()
        .map(|s| s.line_range.start)
        .collect();
    let target_line = if detail.buffer_spans.is_empty() && !detail.base_spans.is_empty() {
        metadata.sources.first().map(|s| s.line_range.start)
    } else {
        None
    };
    Some(MoveSummary {
        hunk_line: cursor_line,
        source_count: metadata.sources.len(),
        source_lines,
        target_line,
    })
}

pub(super) fn move_nav(stoat: &mut Stoat, nav: MoveNavigation) -> UpdateEffect {
    let Some(summary) = current_move_summary(stoat) else {
        return UpdateEffect::None;
    };
    if summary.source_lines.is_empty() && summary.target_line.is_none() {
        return UpdateEffect::None;
    }

    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };

    let target_row = match nav {
        MoveNavigation::FirstSource => {
            editor.move_source_cursor = Some((summary.hunk_line, 0));
            summary.source_lines.first().copied()
        },
        MoveNavigation::NextSource => {
            let idx = match editor.move_source_cursor {
                Some((line, i)) if line == summary.hunk_line => {
                    (i + 1) % summary.source_lines.len().max(1)
                },
                _ => 0,
            };
            editor.move_source_cursor = Some((summary.hunk_line, idx));
            summary.source_lines.get(idx).copied()
        },
        MoveNavigation::PrevSource => {
            let len = summary.source_lines.len().max(1);
            let idx = match editor.move_source_cursor {
                Some((line, i)) if line == summary.hunk_line => (i + len - 1) % len,
                _ => len.saturating_sub(1),
            };
            editor.move_source_cursor = Some((summary.hunk_line, idx));
            summary.source_lines.get(idx).copied()
        },
        MoveNavigation::Target => summary.target_line,
    };

    let Some(row) = target_row else {
        return UpdateEffect::None;
    };
    // Move the cursor to the resolved row. Full cross-file navigation
    // (opening a different buffer when MoveSource.buffer is Some) lands
    // in Phase 9 alongside the workspace-wide move index.
    set_cursor_row(editor, row);
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
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        let new_offset = if delta > 0 {
            match rope.chars_at(head_offset).next() {
                Some(ch) => head_offset + ch.len_utf8(),
                None => head_offset,
            }
        } else {
            match rope.reversed_chars_at(head_offset).next() {
                Some(ch) => head_offset - ch.len_utf8(),
                None => head_offset,
            }
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
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let max_row = display_snapshot.max_point().row;
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_anchor = sel.head();
        let head_point = buffer_snapshot.point_for_anchor(&head_anchor);
        let head_display = display_snapshot.buffer_to_display(head_point);
        let goal_col = match sel.goal {
            SelectionGoal::Column(c) => c,
            SelectionGoal::None => head_display.column,
        };
        let new_row_i = head_display.row as i64 + delta as i64;
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
    });
    UpdateEffect::Redraw
}

pub(super) fn move_word(stoat: &mut Stoat, target: WordTarget, extend: bool) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        let target_offset = match target {
            WordTarget::NextStart => next_word_start(rope, head_offset),
            WordTarget::NextEnd => next_word_end(rope, head_offset),
            WordTarget::PrevStart => prev_word_start(rope, head_offset),
            WordTarget::PrevEnd => prev_word_end(rope, head_offset),
            WordTarget::NextLongStart => next_long_word_start(rope, head_offset),
            WordTarget::NextLongEnd => next_long_word_end(rope, head_offset),
            WordTarget::PrevLongStart => prev_long_word_start(rope, head_offset),
            WordTarget::PrevLongEnd => prev_long_word_end(rope, head_offset),
        };
        if target_offset == head_offset {
            return sel.clone();
        }

        let shift_to_prev_char = || {
            rope.reversed_chars_at(target_offset)
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
            let head_anchor = buffer_snapshot.anchor_at(new_head_offset, Bias::Right);
            return extend_head(
                sel,
                head_anchor,
                new_head_offset,
                SelectionGoal::None,
                buffer_snapshot,
            );
        }

        if target_offset > head_offset {
            let end_offset = shift_to_prev_char();
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
            let tail_offset = match rope.chars_at(head_offset).next() {
                Some(ch) => head_offset + ch.len_utf8(),
                None => head_offset,
            };
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
        let head_anchor = sel.head();
        let head_point = buffer_snapshot.point_for_anchor(&head_anchor);
        let row = head_point.row;
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

pub(super) fn goto_file_start(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
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

    let (buffer_id, primary_id, start, end, new_text) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let primary_id = sel.id;
        let start = buffer_snapshot.resolve_anchor(&sel.start);
        let end = buffer_snapshot.resolve_anchor(&sel.end);
        if start == end {
            return UpdateEffect::None;
        }
        let text = buffer_snapshot.rope().slice(start..end).to_string();
        let new_text = transform(&text);
        if new_text == text {
            return UpdateEffect::None;
        }
        (buffer_id, primary_id, start, end, new_text)
    };

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        guard.edit(start..end, &new_text);
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    let new_end = start + new_text.len();
    let start_anchor = new_buf.anchor_at(start, Bias::Left);
    let end_anchor = new_buf.anchor_at(new_end, Bias::Right);
    editor.selections.transform(new_buf, |s| {
        let mut new = s.clone();
        if new.id == primary_id {
            new.start = start_anchor;
            new.end = end_anchor;
        }
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn increment(stoat: &mut Stoat) -> UpdateEffect {
    apply_decimal_delta(stoat, 1)
}

pub(super) fn decrement(stoat: &mut Stoat) -> UpdateEffect {
    apply_decimal_delta(stoat, -1)
}

fn apply_decimal_delta(stoat: &mut Stoat, delta: i64) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, primary_id, start, end, new_text) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let primary_id = sel.id;
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        let rope = buffer_snapshot.rope();
        let Some(range) = find_decimal_number_at(rope, head_offset) else {
            return UpdateEffect::None;
        };
        let text = rope.slice(range.start..range.end).to_string();
        let Ok(parsed) = text.parse::<i64>() else {
            return UpdateEffect::None;
        };
        let new_text = parsed.saturating_add(delta).to_string();
        if new_text == text {
            return UpdateEffect::None;
        }
        (buffer_id, primary_id, range.start, range.end, new_text)
    };

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        guard.edit(start..end, &new_text);
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    let new_end = start + new_text.len();
    let start_anchor = new_buf.anchor_at(start, Bias::Left);
    let end_anchor = new_buf.anchor_at(new_end, Bias::Right);
    editor.selections.transform(new_buf, |s| {
        let mut new = s.clone();
        if new.id == primary_id {
            new.start = start_anchor;
            new.end = end_anchor;
        }
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn delete_selection(stoat: &mut Stoat) -> UpdateEffect {
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

pub(super) fn undo(stoat: &mut Stoat) -> UpdateEffect {
    apply_buffer_history(stoat, |buf| buf.undo())
}

pub(super) fn redo(stoat: &mut Stoat) -> UpdateEffect {
    apply_buffer_history(stoat, |buf| buf.redo())
}

fn apply_buffer_history<F>(stoat: &mut Stoat, op: F) -> UpdateEffect
where
    F: FnOnce(&mut crate::buffer::TextBuffer) -> bool,
{
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;

    let did_change = {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        op(&mut guard)
    };

    if !did_change {
        return UpdateEffect::None;
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

        let mut edits: Vec<(usize, usize, &'static str)> = Vec::with_capacity(rows.len());
        for row in rows {
            let line_start = rope.point_to_offset(Point::new(row, 0));
            match dir {
                IndentDir::In => edits.push((line_start, line_start, "\t")),
                IndentDir::Out => {
                    let mut chars = rope.chars_at(line_start);
                    let first = chars.next();
                    if first == Some('\t') {
                        edits.push((line_start, line_start + 1, ""));
                    } else if first == Some(' ') {
                        let mut count = 1;
                        for ch in chars {
                            if ch == ' ' && count < INDENT_WIDTH {
                                count += 1;
                            } else {
                                break;
                            }
                        }
                        edits.push((line_start, line_start + count, ""));
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

        let (new_start_offset, new_end_offset) = if already_line_shaped {
            (current_line_start, line_start(end_row + 2))
        } else {
            (current_line_start, current_line_end)
        };

        let start_anchor = buffer_snapshot.anchor_at(new_start_offset, Bias::Left);
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

pub(super) fn rotate_selections_forward(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    editor.selections.rotate_primary(true);
    UpdateEffect::Redraw
}

pub(super) fn rotate_selections_backward(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    editor.selections.rotate_primary(false);
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
fn trim_whitespace(rope: &stoat_text::Rope, start: usize, end: usize) -> Option<(usize, usize)> {
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
        let Some(layer) = syntax_map.snapshot().iter_layers().next() else {
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
    UpdateEffect::Redraw
}

pub(super) fn goto_change(stoat: &mut Stoat, dir: ChangeDir) -> UpdateEffect {
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
        ChangeDir::Next => diff_map
            .hunks_in_range(cursor_row.saturating_add(1)..u32::MAX)
            .into_iter()
            .find(|h| h.buffer_start_line > cursor_row)
            .map(|h| h.buffer_start_line),
        ChangeDir::Prev => diff_map
            .hunks_in_range(0..cursor_row)
            .into_iter()
            .rfind(|h| h.buffer_start_line < cursor_row)
            .map(|h| h.buffer_start_line),
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
    UpdateEffect::Redraw
}

pub(super) fn goto_last_line(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
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

#[derive(Copy, Clone, Debug)]
pub(super) enum PageDir {
    Up,
    Down,
}

/// Fallback viewport height when the focused editor has not been
/// rendered yet (e.g. a unit test that dispatches a page action
/// without running a render pass).
const DEFAULT_VIEWPORT_ROWS: u32 = 20;

pub(super) fn page_motion(stoat: &mut Stoat, dir: PageDir, half: bool) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);
    let delta = if half { viewport.div_ceil(2) } else { viewport };

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

    let max_scroll = max_row.saturating_sub(viewport.saturating_sub(1));
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
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let max_row = buffer_snapshot.rope().max_point().row;
    let max_scroll = max_row.saturating_sub(viewport.saturating_sub(1));

    let new_scroll = match dir {
        ScrollDir::Up => editor.scroll_row.saturating_sub(1),
        ScrollDir::Down => editor.scroll_row.saturating_add(1).min(max_scroll),
    };
    if new_scroll == editor.scroll_row {
        return UpdateEffect::None;
    }
    editor.scroll_row = new_scroll;
    UpdateEffect::Redraw
}

pub(super) fn align_view(stoat: &mut Stoat, align: ViewAlign) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let max_row = rope.max_point().row;

    let head = editor.selections.newest_anchor().head();
    let cursor_row = buffer_snapshot.point_for_anchor(&head).row;

    let desired_scroll = match align {
        ViewAlign::Top => cursor_row,
        ViewAlign::Center => cursor_row.saturating_sub(viewport / 2),
        ViewAlign::Bottom => cursor_row.saturating_sub(viewport.saturating_sub(1)),
    };
    let max_scroll = max_row.saturating_sub(viewport.saturating_sub(1));
    editor.scroll_row = desired_scroll.min(max_scroll);
    UpdateEffect::Redraw
}

pub(super) fn goto_window(stoat: &mut Stoat, align: WindowAlign) -> UpdateEffect {
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
    let target_anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
    editor.selections.transform(buffer_snapshot, |sel| {
        let mut new = sel.clone();
        new.collapse_to(target_anchor, SelectionGoal::None);
        new
    });
    UpdateEffect::Redraw
}
