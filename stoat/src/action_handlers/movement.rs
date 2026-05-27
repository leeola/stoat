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
    find_number_seeking, next_long_word_end, next_long_word_start, next_word_end, next_word_start,
    prev_long_word_end, prev_long_word_start, prev_word_end, prev_word_start, Anchor, Bias,
    NumberKind, Point, Selection, SelectionGoal,
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
        let new_row_i = (head_display.row as i64).saturating_add(delta);
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
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        let mut target_offset = head_offset;
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

pub fn compute_number_delta(text: &str, kind: NumberKind, delta: i64) -> Option<String> {
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

#[derive(Copy, Clone, Debug)]
pub(super) enum OpenDir {
    Above,
    Below,
}

pub(super) fn open_line(stoat: &mut Stoat, dir: OpenDir) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, entries) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let mut seen_rows = std::collections::HashSet::new();
        let mut entries: Vec<(usize, usize)> = Vec::new();
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
            entries.push((sel.id, insert_offset));
        }
        (buffer_id, entries)
    };

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    let mut sorted_offsets: Vec<usize> = entries.iter().map(|(_, o)| *o).collect();
    sorted_offsets.sort_unstable();

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for offset in sorted_offsets.iter().rev() {
            guard.edit(*offset..*offset, "\n");
        }
    }

    let id_to_pre_offset: std::collections::HashMap<usize, usize> =
        entries.iter().copied().collect();
    let bias = match dir {
        OpenDir::Above => Bias::Left,
        OpenDir::Below => Bias::Right,
    };
    let cursor_delta: usize = match dir {
        OpenDir::Above => 0,
        OpenDir::Below => 1,
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some(&pre_offset) = id_to_pre_offset.get(&sel.id) {
            let earlier_inserts = sorted_offsets.iter().filter(|o| **o < pre_offset).count();
            let cursor_offset = pre_offset + earlier_inserts + cursor_delta;
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
    let extend = stoat.mode == "select";
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
    let head_point = rope.offset_to_point(head_offset);
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
        let new_head = new_buf.anchor_at(target, Bias::Right);
        editor.selections.transform(new_buf, |sel| {
            extend_head(sel, new_head, target, sel.goal, new_buf)
        });
    } else {
        apply_primary_range(editor, target..target);
    }
    UpdateEffect::Redraw
}

pub(super) fn save_selection(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let head = editor.selections.newest_anchor().head();
    let offset = buffer_snapshot.resolve_anchor(&head);
    editor.jumplist.save(offset);
    UpdateEffect::None
}

pub(super) fn jump_backward(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let mut target = None;
    for _ in 0..count {
        match editor.jumplist.backward() {
            Some(pos) => target = Some(pos),
            None => break,
        }
    }
    let Some(target) = target else {
        return UpdateEffect::None;
    };
    apply_primary_range(editor, target..target);
    UpdateEffect::Redraw
}

pub(super) fn jump_forward(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let mut target = None;
    for _ in 0..count {
        match editor.jumplist.forward() {
            Some(pos) => target = Some(pos),
            None => break,
        }
    }
    let Some(target) = target else {
        return UpdateEffect::None;
    };
    apply_primary_range(editor, target..target);
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

    let Some((open, close, forward)) = bracket_pair(ch) else {
        return UpdateEffect::None;
    };

    let tree_opt: Option<stoat_language::Tree> = ws
        .buffers
        .syntax_map(buffer_id)
        .and_then(|sm| sm.snapshot().iter_layers().next().map(|l| l.tree.clone()));

    if let Some(ref tree) = tree_opt {
        if is_in_string_or_comment(tree, head_offset) {
            return UpdateEffect::None;
        }
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

/// Find the matching bracket for the character at `head_offset`
/// in `rope`. Returns the byte offset of the matched bracket, or
/// `None` when the char at `head_offset` is not a bracket, the
/// cursor sits inside a string/comment node (when `tree` is
/// provided), or no matching bracket exists in the requested
/// direction. Bracket characters inside string/comment nodes are
/// skipped during the scan when `tree` is provided.
pub fn match_bracket_target(
    rope: &stoat_text::Rope,
    head_offset: usize,
    tree: Option<&stoat_language::Tree>,
) -> Option<usize> {
    let ch = rope.chars_at(head_offset).next()?;
    let (open, close, forward) = bracket_pair(ch)?;
    if let Some(t) = tree {
        if is_in_string_or_comment(t, head_offset) {
            return None;
        }
    }
    scan_bracket_match(rope, head_offset, ch, open, close, forward, tree)
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
    rope: &stoat_text::Rope,
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

    let head = editor.selections.newest_anchor().head();
    let head_point = buffer_snapshot.point_for_anchor(&head);
    let row = head_point.row;
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
        let new_head = new_buf.anchor_at(target_offset, Bias::Right);
        editor.selections.transform(new_buf, |sel| {
            extend_head(sel, new_head, target_offset, sel.goal, new_buf)
        });
    } else {
        apply_primary_range(editor, target_offset..target_offset);
    }
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
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let max_row = buffer_snapshot.rope().max_point().row;
    let max_scroll = max_row.saturating_sub(viewport.saturating_sub(1));

    let new_scroll = match dir {
        ScrollDir::Up => editor.scroll_row.saturating_sub(count),
        ScrollDir::Down => editor.scroll_row.saturating_add(count).min(max_scroll),
    };
    if new_scroll == editor.scroll_row {
        return UpdateEffect::None;
    }
    editor.scroll_row = new_scroll;
    UpdateEffect::Redraw
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
    let target_anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
    editor.selections.transform(buffer_snapshot, |sel| {
        if extend {
            extend_head(
                sel,
                target_anchor,
                target_offset,
                SelectionGoal::None,
                buffer_snapshot,
            )
        } else {
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
            staged: false,
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
            staged: false,
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
}
