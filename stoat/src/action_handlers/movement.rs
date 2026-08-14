use super::{split_selection, view};
use crate::{
    action_handlers::focused_editor_mut,
    app::{Stoat, UpdateEffect},
    diff_map,
    display_map::DisplaySnapshot,
    editor_state::EditorState,
    host::{FsHost, GitHost, GitRepo},
    jumplist::JumpEntry,
    multi_buffer::MultiBufferSnapshot,
    pane::View,
    selection::{
        anchor_selection, forward_block_cursor, land_block_cursor, merge_overlapping_spans,
        ResolvedRead, SelectionsCollection, SpanLanding,
    },
};
use std::{
    cmp::Ordering,
    ops::Range,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
};
use stoat_language::structural_diff;
use stoat_scheduler::Task;
use stoat_text::{
    compute_number_delta, cursor_offset, find_number_seeking, next_char_boundary,
    next_long_word_end_range, next_long_word_start_range, next_word_end_range,
    next_word_start_range, prev_long_word_end_range, prev_long_word_start_range,
    prev_word_end_range, prev_word_start_range, Anchor, Bias, Point, Rope, Selection,
    SelectionGoal,
};

pub(crate) fn set_cursor_row(editor: &mut EditorState, row: u32) {
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let point = Point::new(row, 0);
    let offset = buffer_snapshot.rope().point_to_offset(point);
    // Replaces the set rather than adding to it. A fresh collection carries a
    // seeded selection at offset 0, so appending would land the cursor here and
    // leave a second one at the top of the file.
    editor.selections.set_block_cursor(offset, buffer_snapshot);
    editor.scroll_row = snapshot.buffer_to_display(point).row.saturating_sub(2);
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

/// Copy each selection's shape onto the nearest eligible lines in `dir`,
/// following Helix's `copy_selection_on_line`.
///
/// A copy preserves the source's width and direction, landing on buffer lines a
/// full selection-height apart, so soft wrap does not change where copies land. A
/// line too short to hold the anchor or head column is skipped rather than
/// clamped onto, so the copy keeps its shape or does not appear at all.
fn add_selection_in_direction(stoat: &mut Stoat, dir: AddDirection) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display = editor.display_map.snapshot();
    let buffer = display.buffer_snapshot();
    let rope = buffer.rope();
    let max_row = rope.max_point().row;

    let sources = editor.selections.all_anchors().to_vec();
    let mut copies: Vec<Selection<usize>> = Vec::new();
    for source in &sources {
        // Both ends read as the cells the source covers, never the boundary
        // past one. Whichever end faces forward is the exclusive one, so a
        // forward selection steps its head back and a reversed one its tail.
        // Reading an unstepped tail makes a reversed copy a cell too wide, and
        // where that tail sits on the next row it inflates the height too, so
        // the copy lands a whole selection further than its source.
        let tail_off = buffer.resolve_anchor(&source.tail());
        let head_raw = buffer.resolve_anchor(&source.head());
        let (anchor_off, head_off) = match source.reversed {
            true => (rope.prev_grapheme_boundary(tail_off), head_raw),
            false => (tail_off, cursor_offset(rope, tail_off, head_raw)),
        };
        let anchor_pt = rope.offset_to_point(anchor_off);
        let head_pt = rope.offset_to_point(head_off);
        // Columns travel as visual cells, which is what a vertical motion
        // carries. A tab is one byte and several cells, so copying the byte
        // column would put the copy somewhere j never goes.
        let anchor_col = display.visual_column(anchor_pt);
        let head_col = display.visual_column(head_pt);
        let height = anchor_pt.row.max(head_pt.row) - anchor_pt.row.min(head_pt.row) + 1;

        let mut made = 0usize;
        let mut step = 1u32;
        loop {
            if made >= count {
                break;
            }
            let offset = step * height;
            let (anchor_row, head_row) = match dir {
                AddDirection::Below => (anchor_pt.row + offset, head_pt.row + offset),
                AddDirection::Above => {
                    match (
                        anchor_pt.row.checked_sub(offset),
                        head_pt.row.checked_sub(offset),
                    ) {
                        (Some(a), Some(h)) => (a, h),
                        _ => break,
                    }
                },
            };
            if anchor_row > max_row || head_row > max_row {
                break;
            }

            if let (Some(new_anchor), Some(new_head)) = (
                offset_at_exact_col(&display, buffer, anchor_row, anchor_col),
                offset_at_exact_col(&display, buffer, head_row, head_col),
            ) {
                let point = Selection {
                    id: source.id,
                    start: new_anchor,
                    end: new_anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                };
                copies.push(point.put_cursor(rope, new_head, true));
                made += 1;
            }

            if anchor_row == 0 && head_row == 0 {
                break;
            }
            step += 1;
        }
    }

    if copies.is_empty() {
        return UpdateEffect::None;
    }

    // Copying onto another row works by column, and that row can hold different
    // text, so a column that was a boundary on the source can land inside a
    // cluster here. Starts clamp down and ends clamp up, growing a span out to
    // the character it was splitting.
    let rope = buffer.rope();
    for copy in &mut copies {
        copy.start = rope.clip_to_grapheme_boundary(copy.start, Bias::Left);
        copy.end = rope.clip_to_grapheme_boundary(copy.end, Bias::Right);
    }

    // Starts left, ends right, matching what the anchors would have been minted
    // with one at a time. Two walks for the whole set rather than a root descent
    // per endpoint.
    let starts = buffer.anchors_at_batch(
        &copies.iter().map(|c| c.start).collect::<Vec<_>>(),
        Bias::Left,
    );
    let ends = buffer.anchors_at_batch(
        &copies.iter().map(|c| c.end).collect::<Vec<_>>(),
        Bias::Right,
    );

    let added: Vec<Selection<Anchor>> = copies
        .iter()
        .zip(starts)
        .zip(ends)
        .map(|((copy, start), end)| Selection {
            id: 0,
            start,
            end,
            reversed: copy.reversed,
            goal: copy.goal,
        })
        .collect();
    editor.selections.extend_with_fresh_ids(added, buffer);
    UpdateEffect::Redraw
}

/// Byte offset of the buffer position `visual` cells along `row`, or `None` when
/// the line is too short to reach that column exactly or `row` is hidden in a
/// fold (so the caller skips it rather than clamping onto a different line).
///
/// The column is in visual cells because that is what a vertical motion carries,
/// and a copied selection lands where such a motion would.
fn offset_at_exact_col(
    display: &DisplaySnapshot,
    buffer: &MultiBufferSnapshot,
    row: u32,
    visual: u32,
) -> Option<usize> {
    let rope = buffer.rope();
    let col = display.buffer_column_at_visual(row, visual, Bias::Left);
    // Route the buffer target through display space so a folded row snaps to a
    // different buffer row. A mismatch means the target line is hidden, so skip.
    let clipped = display.clip_point(display.buffer_to_display(Point::new(row, col)), Bias::Left);
    let buffer_pt = display.display_to_buffer(clipped)?;
    if buffer_pt.row != row {
        return None;
    }
    // A line too short to reach the column answers its own end, and one whose
    // cell boundaries fall elsewhere answers the cell before. Either way the
    // round trip differs from what was asked for, which is the skip.
    if display.visual_column(buffer_pt) != visual {
        return None;
    }
    Some(rope.point_to_offset(buffer_pt))
}

pub(super) fn move_horizontal(stoat: &mut Stoat, delta: i32, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    // Step the block-cursor cell, not the raw head. A forward 1-wide selection
    // stores its head one cell past the cursor. Both move and extend step whole
    // grapheme clusters, crossing line boundaries like Helix. A step at the rope
    // end returns the offset it was given, so a count that runs off the buffer
    // stalls there instead of overshooting.
    let step = |offset| {
        if delta > 0 {
            rope.next_grapheme_boundary(offset)
        } else {
            rope.prev_grapheme_boundary(offset)
        }
    };

    move_cursors(&mut editor.selections, buffer_snapshot, extend, |read| {
        let cursor = cursor_offset(rope, read.tail, read.head);
        let target = (0..count).fold(cursor, |t, _| step(t));
        (target != cursor).then_some((target, SelectionGoal::None))
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
    let rope = buffer_snapshot.rope();
    let max_point = rope.max_point();
    let max_row = max_point.row;
    // The line a trailing newline opens, which holds no characters at all. A
    // plain motion lands on it, that being a cursor position of its own, but an
    // extend stops short. Growing onto it only takes in the newline ending the
    // line above, leaving a selection that covers nothing the user aimed at.
    //
    // Column zero on the last row is what identifies it. A blank line in the
    // middle of a buffer owns its own newline, so this never catches one.
    let empty_final_row = (max_point.column == 0 && max_row > 0).then_some(max_row);

    // Where a cursor at `head`/`tail` carrying `goal` lands, with the goal
    // column it takes along. Both arms land on the same cell. Extending decides
    // whether the anchor is left behind, never where the cursor ends up.
    let landing_for = |head: usize, tail: usize, goal: SelectionGoal| {
        let cursor = cursor_offset(rope, tail, head);
        let cursor_pt = rope.offset_to_point(cursor);
        // Cells from the line start, not bytes. The two agree only while every
        // character is one byte and one cell, so carrying bytes between lines
        // drifts across wide glyphs and tabs.
        let goal_col = match goal {
            SelectionGoal::Column(c) => c,
            SelectionGoal::None => display_snapshot.visual_column(cursor_pt),
        };
        let new_row = (cursor_pt.row as i64)
            .saturating_add(delta)
            .clamp(0, max_row as i64) as u32;
        // A plain j/k at the file edge stays a no-op. An overshooting count jump
        // lands on the clamped edge row rather than doing nothing.
        if new_row == cursor_pt.row {
            return None;
        }
        if extend && Some(new_row) == empty_final_row {
            return None;
        }
        // Back to a byte column on the landing line, which leaves the display
        // round-trip below to place it across wraps and folds. A display point
        // built from the goal directly would clip against one wrapped segment's
        // width rather than the whole line's.
        let col = display_snapshot.buffer_column_at_visual(new_row, goal_col, Bias::Left);
        // The target is the same column of the target buffer line. Snap it
        // through display space so a row hidden in a fold or beside a diff block
        // row lands on the next visible buffer row in the travel direction
        // rather than inside the hidden region.
        let target = display_snapshot.buffer_to_display(Point::new(new_row, col));
        let clipped = display_snapshot.clip_point(target, clip_bias);
        let buffer_pt = display_snapshot.display_to_buffer(clipped)?;
        Some((
            rope.point_to_offset(buffer_pt),
            SelectionGoal::Column(goal_col),
        ))
    };

    move_cursors(&mut editor.selections, buffer_snapshot, extend, |read| {
        landing_for(read.head, read.tail, read.goal)
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
    let is_prev = matches!(
        target,
        WordTarget::PrevStart
            | WordTarget::PrevEnd
            | WordTarget::PrevLongStart
            | WordTarget::PrevLongEnd
    );

    // The `(anchor, head)` the scan reaches from this cursor, or `None` where it
    // had nowhere to go.
    let scan_from = |read: &ResolvedRead| {
        let cursor = cursor_offset(rope, read.tail, read.head);
        // Scan from the block cursor cell, matching Helix's word_move. The
        // origin puts head one cell past the cursor forward, or on the cursor
        // backward, and the fold threads that pair across the count so the
        // anchor advances onto each new span rather than accumulating every word
        // crossed.
        let edge = next_char_boundary(rope, cursor);
        let origin = match is_prev {
            true => (edge, cursor),
            false => (cursor, edge),
        };

        let (mut anchor, mut head) = origin;
        for _ in 0..count {
            let next = word_range_step(rope, target, anchor, head);
            if next == (anchor, head) {
                break;
            }
            (anchor, head) = next;
        }
        ((anchor, head) != origin).then_some((anchor, head))
    };

    if extend {
        // The extend wants the cell to land the block cursor on, not the scan's
        // head. Forward the head sits one past the last cell, so it steps back.
        // Backward it is already on the cell.
        move_cursors(&mut editor.selections, buffer_snapshot, true, |read| {
            let (anchor, head) = scan_from(read)?;
            let target_cursor = if is_prev {
                head
            } else {
                cursor_offset(rope, anchor, head)
            };
            Some((target_cursor, SelectionGoal::None))
        });
        return UpdateEffect::Redraw;
    }

    // A plain word motion covers the span it crossed rather than landing a bare
    // cursor on its end, which is why this does not go through `move_cursors`.
    let landings = span_landings(&editor.selections, buffer_snapshot, |read| {
        let (anchor, head) = scan_from(read)?;

        // A scan with nothing left to cover returns an empty range, which at the
        // buffer end is what a forward motion on the trailing newline produces.
        // Widening it here keeps the min-width-1 block cursor every other
        // landing path maintains, and makes the motion a no-op there rather than
        // a collapse. A range that covered anything is returned untouched,
        // direction included.
        let landed = match is_prev {
            false => Selection {
                id: read.id,
                start: anchor,
                end: head,
                reversed: false,
                goal: SelectionGoal::None,
            },
            true => Selection {
                id: read.id,
                start: head,
                end: anchor,
                reversed: true,
                goal: SelectionGoal::None,
            },
        }
        .min_width_1(rope);

        Some(SpanLanding {
            id: read.id,
            start: landed.start,
            end: landed.end,
            reversed: landed.reversed,
            goal: SelectionGoal::None,
        })
    });
    editor
        .selections
        .replace_from_offsets(&landings, buffer_snapshot);
    UpdateEffect::Redraw
}

/// One threaded `(anchor, head)` step for `target`, dispatching to the matching
/// `stoat_text` `*_range` scan. Feeding the result back in as the next origin
/// advances Helix's anchor rule across a count.
fn word_range_step(rope: &Rope, target: WordTarget, anchor: usize, head: usize) -> (usize, usize) {
    match target {
        WordTarget::NextStart => next_word_start_range(rope, anchor, head),
        WordTarget::NextEnd => next_word_end_range(rope, anchor, head),
        WordTarget::NextLongStart => next_long_word_start_range(rope, anchor, head),
        WordTarget::NextLongEnd => next_long_word_end_range(rope, anchor, head),
        WordTarget::PrevStart => prev_word_start_range(rope, anchor, head),
        WordTarget::PrevEnd => prev_word_end_range(rope, anchor, head),
        WordTarget::PrevLongStart => prev_long_word_start_range(rope, anchor, head),
        WordTarget::PrevLongEnd => prev_long_word_end_range(rope, anchor, head),
    }
}

/// Extend `sel` so its block cursor lands on the cell at `target_cursor`.
///
/// A forward result stores the head one cell past `target_cursor`, so the
/// paint-site [`cursor_offset`] recovers the cell. A reversed result keeps the
/// head on `target_cursor`, where `cursor_offset` is identity. Forward on-cell
/// motions re-base their step on [`cursor_offset`] and route the landing cell
/// through this, so the block cursor renders on the cell moved to rather than
/// one short of it.
///
/// Crossing the tail steps it one cluster, so the anchor's cell stays covered
/// and the result is never zero-width. Both rules live in
/// [`Selection::put_cursor`], which this defers to rather than restating.
///
/// `head_offset` and `tail_offset` must be where `sel.head()` and `sel.tail()`
/// resolve. They are parameters because a motion driven by
/// [`Selections::transform_resolved`] already holds them, and resolving them
/// here would descend the fragment tree again for every selection.
fn extend_head_to_cursor(
    id: usize,
    reversed: bool,
    target_cursor: usize,
    head_offset: usize,
    tail_offset: usize,
    goal: SelectionGoal,
    rope: &Rope,
) -> SpanLanding {
    let (start, end) = match reversed {
        true => (head_offset, tail_offset),
        false => (tail_offset, head_offset),
    };
    let resolved = Selection {
        id,
        start,
        end,
        reversed,
        goal,
    };

    // put_cursor clears the goal as a horizontal move, but the vertical callers
    // carry a column across rows, so the caller's goal is restored over it.
    let landed = resolved.put_cursor(rope, target_cursor, true);

    SpanLanding {
        id,
        start: landed.start,
        end: landed.end,
        reversed: landed.reversed,
        goal,
    }
}

/// Move every selection's block cursor to the cell `target_for` names for it,
/// extending the selection or landing a bare cursor there.
///
/// The shape nearly every motion has. `target_for` answers with the offset to
/// move to and the goal to carry, or [`None`] for a selection this motion
/// leaves where it is.
///
/// Both arms land in one batch, so a motion over N cursors mints anchors in two
/// walks rather than two root descents each.
pub(crate) fn move_cursors<F>(
    selections: &mut SelectionsCollection,
    buffer: &MultiBufferSnapshot,
    extend: bool,
    target_for: F,
) where
    F: Fn(&ResolvedRead) -> Option<(usize, SelectionGoal)>,
{
    if !extend {
        let landings = block_cursor_landings(selections, buffer, target_for);
        selections.land_block_cursors(&landings, buffer);
        return;
    }

    let rope = buffer.rope();
    let landings = span_landings(selections, buffer, |read| {
        let (target, goal) = target_for(read)?;
        Some(extend_head_to_cursor(
            read.id,
            read.reversed,
            target,
            read.head,
            read.tail,
            goal,
            rope,
        ))
    });
    selections.replace_from_offsets(&landings, buffer);
}

/// Work out where each selection's span lands, in the shape
/// [`SelectionsCollection::replace_from_offsets`] takes.
///
/// The span counterpart to [`block_cursor_landings`]. `landing_for` answers
/// with the span a selection lands on, or [`None`] for one this motion leaves
/// where it is.
fn span_landings<F>(
    selections: &SelectionsCollection,
    buffer: &MultiBufferSnapshot,
    landing_for: F,
) -> Vec<SpanLanding>
where
    F: Fn(&ResolvedRead) -> Option<SpanLanding>,
{
    selections
        .resolved_reads(buffer)
        .iter()
        .filter_map(landing_for)
        .collect()
}

/// Work out where each selection's block cursor lands, in the shape
/// [`SelectionsCollection::land_block_cursors`] takes.
///
/// `target_for` answers with the offset the cursor lands on and the goal to
/// keep, or [`None`] for a selection this motion leaves where it is. An unnamed
/// selection keeps its span, so returning `None` is how a motion declines to
/// move one.
///
/// Sorted by id, which is what the landing lookup binary-searches.
fn block_cursor_landings<F>(
    selections: &SelectionsCollection,
    buffer: &MultiBufferSnapshot,
    target_for: F,
) -> Vec<(usize, usize, SelectionGoal)>
where
    F: Fn(&ResolvedRead) -> Option<(usize, SelectionGoal)>,
{
    let mut landings: Vec<(usize, usize, SelectionGoal)> = selections
        .resolved_reads(buffer)
        .iter()
        .filter_map(|read| target_for(read).map(|(target, goal)| (read.id, target, goal)))
        .collect();
    landings.sort_unstable_by_key(|(id, _, _)| *id);
    landings
}

/// Reorient each selection so its head sits at its start, keeping the span,
/// so entering insert lands the insert point before the selection.
///
/// The min-width-1 counterpart to Helix's `i`. Swapping to head-at-start makes
/// [`cursor_offset`] resolve to the selection start, so typing inserts before a
/// multi-char selection rather than near its end. A bare cursor's span is
/// unchanged and its cursor cell stays put, so this is a no-op there.
pub(super) fn enter_insert_mode(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    editor.selections.transform(buffer_snapshot, |sel| {
        let mut new = sel.clone();
        new.reversed = true;
        new
    });
    UpdateEffect::Redraw
}

/// Extend each selection one grapheme past its end, so typing appends after it.
///
/// Helix's `a`. The anchor stays where it was, so the selection survives the
/// round trip. Esc steps the head back and leaves what was selected before,
/// where a collapse to a cursor loses it.
///
/// A selection ending at the buffer's end has nothing to extend over, so a
/// line ending is inserted there first to make the room. That inserted ending
/// is `"\n"` whatever the file uses, since a buffer holds its text in LF and
/// the file's own ending is restored only on write.
pub(super) fn append_mode(stoat: &mut Stoat) -> UpdateEffect {
    stoat.restore_cursor = true;
    let Some((editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let needs_room = {
        let Some(editor) = focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        // Helix asks the last range alone, which is the only one that reaches
        // the buffer's end once the set is sorted and disjoint.
        editor.selections.all_anchors().last().is_some_and(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            start != end && end == buffer_snapshot.rope().len()
        })
    };

    if needs_room {
        let ws = stoat.active_workspace_mut();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let at = guard.rope().len();
        guard.edit_batch(&[(at..at, "\n")]);
    }

    let ws = stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor
        .selections
        .transform_resolved(buffer_snapshot, |sel, head_offset, tail_offset| {
            let (from, to) = (head_offset.min(tail_offset), head_offset.max(tail_offset));
            Selection {
                id: sel.id,
                start: buffer_snapshot.anchor_at(from, Bias::Right),
                end: buffer_snapshot.anchor_at(rope.next_grapheme_boundary(to), Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            }
        });
    UpdateEffect::Redraw
}

/// Position each cursor to append at the end of its line, auto-indenting empty
/// lines.
///
/// The min-width-1 counterpart to Helix's `A`. A non-empty line lands via
/// [`forward_block_cursor`] at the insert point after the last character. An
/// empty line is auto-indented per [`insert_with_indent`].
pub(super) fn insert_at_line_end(stoat: &mut Stoat) -> UpdateEffect {
    insert_with_indent(stoat, IndentFallback::LineEnd)
}

/// Position each cursor to insert at its line's first non-whitespace character,
/// auto-indenting empty lines.
///
/// The min-width-1 counterpart to Helix's `I`. A non-empty line lands on its
/// first non-whitespace character, falling back to the line start when the line
/// is all whitespace. An empty line is auto-indented per [`insert_with_indent`].
pub(super) fn insert_at_line_start(stoat: &mut Stoat) -> UpdateEffect {
    insert_with_indent(stoat, IndentFallback::LineStart)
}

/// Fallback cursor landing on a non-empty line for [`insert_with_indent`].
#[derive(Copy, Clone)]
enum IndentFallback {
    /// The first non-whitespace character, or the line start when the line is
    /// all whitespace. Backs `I`.
    LineStart,
    /// The insert point after the line's last character. Backs `A`.
    LineEnd,
}

/// One cursor's landing plan for [`insert_with_indent`], captured before the
/// edit is applied.
struct IndentPlan {
    id: usize,
    /// `Some((line_start, row))` when the cursor sits on an empty line, marking
    /// where the computed indentation is inserted and the row it re-indents.
    /// `None` on a non-empty line.
    blank: Option<(usize, u32)>,
    /// Fallback landing offset for a non-empty line. Ignored when `blank` is set.
    target: usize,
}

/// The indentation inserted at one empty line by [`insert_with_indent`].
struct IndentInsert {
    id: usize,
    at: usize,
    indent: String,
}

/// Position each cursor to insert at its line, auto-indenting empty lines.
///
/// Backs `I` ([`IndentFallback::LineStart`]) and `A`
/// ([`IndentFallback::LineEnd`]). On an empty line the indentation computed from
/// the surrounding syntax is inserted and the cursor lands after it, so entering
/// insert on a blank line inside a block starts at the block's indent. A
/// non-empty line inserts nothing and moves the cursor to the fallback position.
fn insert_with_indent(stoat: &mut Stoat, fallback: IndentFallback) -> UpdateEffect {
    // Appending past a line's last character (LineEnd) leaves the block cursor
    // one cell beyond the content, so leaving insert must step it back. LineStart
    // inserts before its target and needs no restore.
    if matches!(fallback, IndentFallback::LineEnd) {
        stoat.restore_cursor = true;
    }

    let editor_id = {
        let ws = stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => return UpdateEffect::None,
        }
    };

    let (buffer_id, plans) = {
        let ws = stoat.active_workspace_mut();
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let plans: Vec<IndentPlan> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let cursor = cursor_offset(
                    rope,
                    buffer_snapshot.resolve_anchor(&sel.tail()),
                    buffer_snapshot.resolve_anchor(&sel.head()),
                );
                let row = rope.offset_to_point(cursor).row;
                let line_start = rope.point_to_offset(Point::new(row, 0));
                let line_len = rope.line_len(row);
                if line_len == 0 {
                    return IndentPlan {
                        id: sel.id,
                        blank: Some((line_start, row)),
                        target: line_start,
                    };
                }
                let line_end = rope.point_to_offset(Point::new(row, line_len));
                let target = match fallback {
                    IndentFallback::LineEnd => line_end,
                    IndentFallback::LineStart => {
                        first_nonwhitespace(rope, line_start, line_end).unwrap_or(line_start)
                    },
                };
                IndentPlan {
                    id: sel.id,
                    blank: None,
                    target,
                }
            })
            .collect();
        (buffer_id, plans)
    };

    let mut inserts: Vec<IndentInsert> = plans
        .iter()
        .filter_map(|plan| {
            plan.blank.map(|(at, row)| IndentInsert {
                id: plan.id,
                at,
                indent: stoat.suggested_indent_string(buffer_id, row),
            })
        })
        .collect();
    inserts.sort_by_key(|ins| ins.at);

    {
        let ws = stoat.active_workspace_mut();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = inserts
            .iter()
            .rev()
            .filter(|ins| !ins.indent.is_empty())
            .map(|ins| (ins.at..ins.at, ins.indent.as_str()))
            .collect();
        guard.edit_batch(&batch);
    }

    // Every inserted indent before an offset pushes it further right in the
    // edited buffer.
    let shift_before = |off: usize| -> usize {
        inserts
            .iter()
            .filter(|ins| ins.at < off)
            .map(|ins| ins.indent.len())
            .sum()
    };

    let mut landings: std::collections::HashMap<usize, (usize, bool)> =
        std::collections::HashMap::new();
    for ins in &inserts {
        let landed = ins.at + shift_before(ins.at) + ins.indent.len();
        landings.insert(ins.id, (landed, true));
    }
    for plan in &plans {
        if plan.blank.is_none() {
            let landed = plan.target + shift_before(plan.target);
            let forward = matches!(fallback, IndentFallback::LineEnd);
            landings.insert(plan.id, (landed, forward));
        }
    }

    let ws = stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor
        .selections
        .transform(new_buf, |sel| match landings.get(&sel.id) {
            Some(&(off, true)) => {
                forward_block_cursor(sel.id, off, SelectionGoal::None, new_buf.rope(), new_buf)
            },
            Some(&(off, false)) => {
                land_block_cursor(sel.id, off, SelectionGoal::None, new_buf.rope(), new_buf)
            },
            None => sel.clone(),
        });

    stoat.auto_indent_cursors = inserts.iter().map(|ins| ins.id).collect();
    UpdateEffect::Redraw
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

    // The cell to land on, from a cursor at `head`/`tail`. The end boundary sits
    // one past the line's last character and a block cursor has to land on a
    // cell rather than the boundary, so it steps back. An empty line has no cell
    // to step back to and stays at its start.
    let target_for = |head: usize, tail: usize| {
        // Use the block-cursor cell's row, not the raw head. A 1-wide cursor
        // sitting at a line's end has its head on the next line's first cell,
        // which would move the boundary to the wrong line.
        let cursor_row = rope.offset_to_point(cursor_offset(rope, tail, head)).row;
        let line_start = rope.point_to_offset(Point::new(cursor_row, 0));
        match boundary {
            LineBoundary::Start => line_start,
            LineBoundary::End => {
                let end = rope.point_to_offset(Point::new(cursor_row, rope.line_len(cursor_row)));
                match end > line_start {
                    true => rope.prev_grapheme_boundary(end),
                    false => end,
                }
            },
        }
    };

    move_cursors(&mut editor.selections, buffer_snapshot, extend, |read| {
        Some((target_for(read.head, read.tail), SelectionGoal::None))
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

    // A line that is all whitespace has nothing to go to, which leaves its
    // cursor where it is.
    let target_for = |head: usize, tail: usize| {
        let row = rope.offset_to_point(cursor_offset(rope, tail, head)).row;
        let line_start = rope.point_to_offset(Point::new(row, 0));
        let line_end = rope.point_to_offset(Point::new(row, rope.line_len(row)));
        first_nonwhitespace(rope, line_start, line_end)
    };

    move_cursors(&mut editor.selections, buffer_snapshot, extend, |read| {
        target_for(read.head, read.tail).map(|target| (target, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

/// Offset of the first non-whitespace character in `[line_start, line_end)`, or
/// `None` when the range is empty or all whitespace.
fn first_nonwhitespace(rope: &Rope, line_start: usize, line_end: usize) -> Option<usize> {
    let mut cursor = line_start;
    for ch in rope.chars_at(line_start) {
        if cursor >= line_end {
            break;
        }
        if !ch.is_whitespace() {
            return Some(cursor);
        }
        cursor += ch.len_utf8();
    }
    None
}

/// The comment token opening the line, paired with the offset it starts at, or
/// `None` when the line's first non-whitespace run is not one of `tokens`.
///
/// The longest matching token wins, because a language lists tokens that prefix
/// one another. Rust's `///` also matches `//`, and the shorter one drops a
/// slash off a doc comment when the line continues with it.
///
/// The offset is the caller's means of telling how much of the line precedes the
/// token, which is what separates a cursor sitting inside the leading whitespace
/// from one sitting in the comment body.
pub(crate) fn line_comment_continues(
    rope: &Rope,
    line_start: usize,
    line_end: usize,
    tokens: &[&'static str],
) -> Option<(usize, &'static str)> {
    let first = first_nonwhitespace(rope, line_start, line_end)?;
    let token = longest_token_at(rope, first, line_end, tokens)?;
    Some((first, token))
}

/// The longest of `tokens` the rope spells at `offset`, within `limit`.
///
/// Callers reach the offset by different routes, which is why the match is its
/// own step. A comment's own line finds it past every whitespace character,
/// while a join walk stops at the first character that is neither a space nor a
/// tab.
fn longest_token_at(
    rope: &Rope,
    offset: usize,
    limit: usize,
    tokens: &[&'static str],
) -> Option<&'static str> {
    tokens
        .iter()
        .copied()
        .filter(|token| rope_matches_at(rope, offset, limit, token))
        .max_by_key(|token| token.len())
}

pub(super) fn goto_file_start(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    super::jump::push_jump(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    move_cursors(&mut editor.selections, buffer_snapshot, extend, |_| {
        Some((0, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

/// Shrink each selection to a block cursor on its head.
///
/// The vertical goal drops with the rest of the selection. A goal outlives the
/// motion that set it, so that `j` through a short line returns to the column
/// it started from. A collapse ends that run, because the user places a cursor
/// rather than travels down a column.
pub(super) fn collapse_selection(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let landings = block_cursor_landings(&editor.selections, buffer_snapshot, |read| {
        Some((
            cursor_offset(rope, read.tail, read.head),
            SelectionGoal::None,
        ))
    });
    editor
        .selections
        .land_block_cursors(&landings, buffer_snapshot);
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

pub(super) fn ensure_selections_forward(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    editor.selections.transform(buffer_snapshot, |sel| {
        let mut new = sel.clone();
        new.reversed = false;
        new
    });
    UpdateEffect::Redraw
}

pub(super) fn rotate_selection_contents_forward(stoat: &mut Stoat) -> UpdateEffect {
    rotate_selection_contents(stoat, true)
}

pub(super) fn rotate_selection_contents_backward(stoat: &mut Stoat) -> UpdateEffect {
    rotate_selection_contents(stoat, false)
}

/// Cyclically move the text of each selection into the next (`forward`) or
/// previous selection's range, following Helix's `reorder_selection_contents`.
///
/// The selections stay in place and re-cover their new text, and the primary
/// travels with the fragment it held. Fewer than two non-empty selections is a
/// no-op. Unlike `(`/`)`, which rotate only which selection is primary, this
/// rewrites the buffer.
fn rotate_selection_contents(stoat: &mut Stoat, forward: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
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
        let mut entries: Vec<(usize, usize, usize, String)> = editor
            .selections
            .all_anchors()
            .iter()
            .filter_map(|sel| {
                let s = buffer_snapshot.resolve_anchor(&sel.start);
                let e = buffer_snapshot.resolve_anchor(&sel.end);
                (s != e).then(|| (sel.id, s, e, rope.chunks_in_range(s..e).collect()))
            })
            .collect();
        entries.sort_by_key(|(_, start, _, _)| *start);
        (buffer_id, entries)
    };

    if entries.len() < 2 {
        return UpdateEffect::None;
    }

    let rotate_by = count.min(entries.len());
    let mut rotated: Vec<String> = entries.iter().map(|(_, _, _, text)| text.clone()).collect();
    if forward {
        rotated.rotate_right(rotate_by);
    } else {
        rotated.rotate_left(rotate_by);
    }

    // The primary travels with the text it held. `rotate_right(n)` carries the
    // fragment at index `i` to `i + n`, so the primary moves the same way.
    let new_primary = {
        let primary_id = ws
            .editors
            .get(editor_id)
            .expect("editor")
            .selections
            .newest_anchor()
            .id;
        entries
            .iter()
            .position(|(id, _, _, _)| *id == primary_id)
            .map(|index| {
                let len = entries.len();
                let shifted = if forward {
                    index + rotate_by
                } else {
                    index + len - rotate_by
                };
                entries[shifted % len].0
            })
    };

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = entries
            .iter()
            .enumerate()
            .rev()
            .map(|(i, (_, start, end, _))| (*start..*end, rotated[i].as_str()))
            .collect();
        guard.edit_batch(&batch);
    }

    // Each selection re-covers its new text. A running length delta shifts later
    // ranges when fragments differ in length.
    let mut new_ranges: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
    let mut shift = 0isize;
    for (i, (id, start, end, _)) in entries.iter().enumerate() {
        let new_start = (*start as isize + shift) as usize;
        let new_end = new_start + rotated[i].len();
        new_ranges.insert(*id, (new_start, new_end));
        shift += rotated[i].len() as isize - (*end - *start) as isize;
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some(&(start, end)) = new_ranges.get(&sel.id) {
            new.start = new_buf.anchor_at(start, Bias::Right);
            new.end = new_buf.anchor_at(end, Bias::Right);
            new.goal = SelectionGoal::None;
        }
        new
    });

    if let Some(id) = new_primary {
        editor.selections.make_primary(id);
    }

    UpdateEffect::Redraw
}

pub(super) fn join_selections(stoat: &mut Stoat) -> UpdateEffect {
    join_selections_impl(stoat, false)
}

pub(super) fn join_selections_space(stoat: &mut Stoat) -> UpdateEffect {
    join_selections_impl(stoat, true)
}

/// Join every line each selection touches onto one line, following Helix's
/// `join_selections_impl`.
///
/// Each newline plus the following indentation is replaced with a single
/// space, except that a blank joining line contributes no space. A single-line
/// selection joins with the line below.
///
/// A joined line loses its leading comment token only when that token matches
/// the one the join is already running under, so a comment block collapses to
/// one leader. A line opening with a different token keeps it and becomes the
/// running one, which is what stops a `///` doc comment from being shaved down
/// to `/` under a plain `//` line.
///
/// With `select_space`, the inserted spaces are left selected. Otherwise the
/// selection is remapped through the edit. No-op when the focused pane is not
/// an editor or nothing joins.
fn join_selections_impl(stoat: &mut Stoat, select_space: bool) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };
    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let comment_tokens = ws
        .buffers
        .language_for(buffer_id)
        .map_or(&[][..], |lang| lang.line_comments);

    let mut changes: Vec<(usize, usize, bool)> = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let max_row = rope.max_point().row;
        let mut changes = Vec::new();
        for sel in editor.selections.all_anchors() {
            let (start_row, mut end_row) = selection_line_range(sel, rope, buffer_snapshot);
            if start_row == end_row {
                end_row = (end_row + 1).min(max_row);
            }

            let first_start = rope.point_to_offset(Point::new(start_row, 0));
            let first_end = line_content_end(rope, start_row);
            let mut current_token =
                line_comment_continues(rope, first_start, first_end, comment_tokens)
                    .map(|(_, token)| token);

            for line in start_row..end_row {
                let join_start = line_content_end(rope, line);
                let next_end = line_content_end(rope, line + 1);
                let next_start = rope.point_to_offset(Point::new(line + 1, 0));
                let mut join_end = skip_spaces_tabs(rope, next_start, next_end);

                if let Some(token) = longest_token_at(rope, join_end, next_end, comment_tokens) {
                    if Some(token) == current_token {
                        join_end = skip_spaces_tabs(rope, join_end + token.len(), next_end);
                    } else {
                        current_token = Some(token);
                    }
                }

                let has_space = join_end != next_end;
                changes.push((join_start, join_end, has_space));
            }
        }
        changes
    };

    if changes.is_empty() {
        return UpdateEffect::None;
    }
    changes.sort_by_key(|(start, _, _)| *start);
    changes.dedup();

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = changes
            .iter()
            .rev()
            .map(|(start, end, has_space)| (*start..*end, if *has_space { " " } else { "" }))
            .collect();
        guard.edit_batch(&batch);
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    let spaces: Vec<Selection<Anchor>> = if select_space {
        let mut spaces = Vec::new();
        let mut delta = 0isize;
        for (start, end, has_space) in &changes {
            let new_start = (*start as isize + delta) as usize;
            let replaced = if *has_space { 1 } else { 0 };
            if *has_space {
                spaces.push(Selection {
                    id: 0,
                    start: new_buf.anchor_at(new_start, Bias::Right),
                    end: new_buf.anchor_at(new_start + 1, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                });
            }
            delta += replaced - (*end - *start) as isize;
        }
        spaces
    } else {
        Vec::new()
    };

    if spaces.is_empty() {
        editor.selections.transform(new_buf, |sel| sel.clone());
    } else {
        editor.selections.replace_with_fresh_ids(spaces, new_buf);
    }
    UpdateEffect::Redraw
}

/// Offset just past the content of buffer `row`, before its line ending (the
/// rope end for the last line).
fn line_content_end(rope: &Rope, row: u32) -> usize {
    rope.point_to_offset(Point::new(row, rope.line_len(row)))
}

/// Advance past spaces and tabs from `offset`, stopping at `limit` or the first
/// other character. A newline stops the scan, so it never crosses a line.
fn skip_spaces_tabs(rope: &Rope, offset: usize, limit: usize) -> usize {
    let mut cursor = offset;
    for ch in rope.chars_at(offset) {
        if cursor >= limit || (ch != ' ' && ch != '\t') {
            break;
        }
        cursor += ch.len_utf8();
    }
    cursor
}

/// Whether `token` occurs at `offset` within `[offset, limit)`.
fn rope_matches_at(rope: &Rope, offset: usize, limit: usize, token: &str) -> bool {
    if offset + token.len() > limit {
        return false;
    }
    rope.chars_at(offset)
        .zip(token.chars())
        .all(|(actual, expected)| actual == expected)
}

pub(super) fn align_selections(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    // A selection spanning two rows has no single column to align, so one of
    // them abandons the whole align. Reporting it has to wait for the block to
    // release the workspace, which the edit below borrows again.
    let mut multi_line = false;
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
                multi_line = true;
                break;
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

    if multi_line {
        stoat.set_status("align cannot work with multi line selections");
        return UpdateEffect::Redraw;
    }

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
        let batch: Vec<(Range<usize>, &str)> = edits
            .iter()
            .rev()
            .map(|(offset, text)| (*offset..*offset, text.as_str()))
            .collect();
        guard.edit_batch(&batch);
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
    editor
        .selections
        .split_each(buffer_snapshot, Bias::Right, |sel| {
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

            let mut pieces: Vec<(usize, usize)> = Vec::with_capacity(newline_positions.len() + 1);
            let mut prev = start_offset;
            for nl in &newline_positions {
                // An empty line still carries a cursor, so it contributes a
                // piece like any other line rather than being skipped over.
                pieces.push((prev, *nl));
                prev = nl + 1;
            }
            if prev < end_offset {
                pieces.push((prev, end_offset));
            }
            split_selection::widen_pieces(rope, pieces)
        });
    editor.selections.make_first_primary();
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
        let batch: Vec<(Range<usize>, &str)> = edits
            .iter()
            .rev()
            .map(|(_, s, e, new_text)| (*s..*e, new_text.as_str()))
            .collect();
        guard.edit_batch(&batch);
    }

    let edited_ranges = shifted_edit_ranges(&edits);

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

/// Where each edit's replacement text ended up, keyed by the selection that
/// produced it.
///
/// `edits` are `(selection id, start, end, replacement)` at the offsets they
/// were measured at, ascending by start. Every edit before a given one has
/// changed the text's length by then, so its own start moves by the running
/// total of those changes.
fn shifted_edit_ranges(
    edits: &[(usize, usize, usize, String)],
) -> std::collections::HashMap<usize, (usize, usize)> {
    let mut ranges = std::collections::HashMap::with_capacity(edits.len());
    let mut shift: i64 = 0;
    for (id, s, e, new_text) in edits {
        let start = (*s as i64 + shift) as usize;
        ranges.insert(*id, (start, start + new_text.len()));
        shift += new_text.len() as i64 - (*e - *s) as i64;
    }
    ranges
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
                let cursor = cursor_offset(
                    rope,
                    buffer_snapshot.resolve_anchor(&sel.tail()),
                    buffer_snapshot.resolve_anchor(&sel.head()),
                );
                let num_match = find_number_seeking(rope, cursor)?;
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
        let batch: Vec<(Range<usize>, &str)> = edits
            .iter()
            .rev()
            .map(|(_, s, e, new_text)| (*s..*e, new_text.as_str()))
            .collect();
        guard.edit_batch(&batch);
    }

    let edited_ranges = shifted_edit_ranges(&edits);

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
        && fragments.iter().any(|f| !f.is_empty())
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
            .map(|sel| {
                let s = buffer_snapshot.resolve_anchor(&sel.start);
                let e = buffer_snapshot.resolve_anchor(&sel.end);
                (sel.id, s, e)
            })
            .collect();
        (buffer_id, deletions)
    };

    if deletions.is_empty() {
        return UpdateEffect::None;
    }

    deletions.sort_by_key(|(_, s, _)| *s);

    // Selections are merged where they overlap, but these are offsets the
    // anchors resolve to now, and text deleted between two selections
    // collapses them onto each other without any selection change to notice
    // it. Editing an overlap twice takes as much text again beyond it, so the
    // spans are unioned before anything is removed.
    let spans = merge_overlapping_spans(
        deletions
            .iter()
            .filter(|(_, s, e)| s < e)
            .map(|&(_, s, e)| (s, e))
            .collect(),
    );

    // An empty range still costs an edit, which discards the redo history and
    // records an undo step. A delete that covers no text has to stop short of
    // that rather than rely on the buffer to ignore it.
    if spans.is_empty() {
        return UpdateEffect::None;
    }

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> =
            spans.iter().rev().map(|(s, e)| (*s..*e, "")).collect();
        guard.edit_batch(&batch);
    }

    let deleted_ids: std::collections::HashSet<usize> =
        deletions.iter().map(|(id, _, _)| *id).collect();

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    editor.selections.transform(new_buf, |sel| {
        if deleted_ids.contains(&sel.id) {
            let post_offset = new_buf.resolve_anchor(&sel.start);
            land_block_cursor(
                sel.id,
                post_offset,
                SelectionGoal::None,
                new_buf.rope(),
                new_buf,
            )
        } else {
            sel.clone()
        }
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

/// One selection's open site, resolved before indentation is computed.
///
/// `continued` holds the comment token the opened line carries on, per site
/// rather than per buffer, because separate cursors sit on lines opening with
/// different tokens.
struct OpenSite {
    id: usize,
    insert_offset: usize,
    row: u32,
    continued: Option<&'static str>,
}

/// One selection's opened lines. `text` is a single newline+indent block
/// repeated once per opened line, so a count of N opens N lines at
/// `insert_offset`, each cursor landing `cursor_within` bytes into its block.
struct OpenUnit {
    id: usize,
    insert_offset: usize,
    text: String,
    block_len: usize,
    cursor_within: usize,
}

pub(super) fn open_line(stoat: &mut Stoat, dir: OpenDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;

    let editor_id = {
        let ws = stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => return UpdateEffect::None,
        }
    };

    let (buffer_id, sites) = {
        let ws = stoat.active_workspace_mut();
        let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
        let comment_tokens = ws
            .buffers
            .language_for(buffer_id)
            .map_or(&[][..], |lang| lang.line_comments);
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let sites: Vec<OpenSite> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                // Each direction opens from the end it moves away from, so a
                // multi-line selection opens above its first line and below its
                // last whichever way round it faces. Reading the cursor instead
                // puts both openings at whichever end the head happens to sit.
                let from = buffer_snapshot.resolve_anchor(&sel.start);
                let to = buffer_snapshot.resolve_anchor(&sel.end);
                let anchor_offset = match dir {
                    OpenDir::Above => from,
                    // A 1-wide cursor at a line's end has its boundary on the
                    // next line, so the step back lands on the cell it draws.
                    // The rope end draws no cell at all and stays put.
                    OpenDir::Below => cursor_offset(rope, from, to),
                };
                let row = rope.offset_to_point(anchor_offset).row;
                let line_start = rope.point_to_offset(Point::new(row, 0));
                let line_end = rope.point_to_offset(Point::new(row, rope.line_len(row)));
                let insert_offset = match dir {
                    OpenDir::Above => line_start,
                    OpenDir::Below => line_end,
                };
                let continued = line_comment_continues(rope, line_start, line_end, comment_tokens)
                    .map(|(_, token)| token);
                OpenSite {
                    id: sel.id,
                    insert_offset,
                    row,
                    continued,
                }
            })
            .collect();
        (buffer_id, sites)
    };

    if sites.is_empty() {
        return UpdateEffect::None;
    }

    // A line opened below inherits the freshly opened block through the indents
    // query. One opened above copies the current line's indentation. A comment
    // continuation aligns to the line's own leading whitespace and carries the
    // token forward.
    let mut units: Vec<OpenUnit> = sites
        .iter()
        .map(|site| {
            let indent = if site.continued.is_some() {
                stoat.line_indent_string(buffer_id, site.row)
            } else {
                match dir {
                    OpenDir::Below => stoat.newline_indent_string(buffer_id, site.insert_offset),
                    OpenDir::Above => stoat.line_indent_string(buffer_id, site.row),
                }
            };
            let prefix = match site.continued {
                Some(token) => format!("{indent}{token} "),
                None => indent,
            };
            let (block, cursor_within) = match dir {
                OpenDir::Below => (format!("\n{prefix}"), 1 + prefix.len()),
                OpenDir::Above => (format!("{prefix}\n"), prefix.len()),
            };
            OpenUnit {
                id: site.id,
                insert_offset: site.insert_offset,
                block_len: block.len(),
                text: block.repeat(count),
                cursor_within,
            }
        })
        .collect();
    units.sort_by_key(|unit| (unit.insert_offset, unit.id));

    // Each cursor lands past the text inserted before it. Same-offset units stack
    // in (offset, id) order, so a lower-id unit's lines precede a higher-id one's.
    let mut cursors_by_id: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    let mut shift = 0usize;
    for unit in &units {
        let base = unit.insert_offset + shift;
        let offsets = (0..count)
            .map(|i| base + i * unit.block_len + unit.cursor_within)
            .collect();
        cursors_by_id.insert(unit.id, offsets);
        shift += unit.text.len();
    }

    {
        let ws = stoat.active_workspace_mut();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = units
            .iter()
            .rev()
            .map(|unit| (unit.insert_offset..unit.insert_offset, unit.text.as_str()))
            .collect();
        guard.edit_batch(&batch);
    }

    let bias = match dir {
        OpenDir::Above => Bias::Left,
        OpenDir::Below => Bias::Right,
    };

    {
        let ws = stoat.active_workspace_mut();
        let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
        let new_display = editor.display_map.snapshot();
        let new_buf = new_display.buffer_snapshot();
        editor.selections.split_each(new_buf, bias, |sel| {
            cursors_by_id
                .get(&sel.id)
                .map(|offsets| offsets.iter().map(|&offset| (offset, offset)).collect())
                .unwrap_or_default()
        });
    }

    stoat.auto_indent_cursors = {
        let ws = stoat.active_workspace();
        let editor = ws.editors.get(editor_id).expect("editor still exists");
        editor
            .selections
            .all_anchors()
            .iter()
            .map(|s| s.id)
            .collect()
    };
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
            .map(|sel| {
                let s = buffer_snapshot.resolve_anchor(&sel.start);
                let e = buffer_snapshot.resolve_anchor(&sel.end);
                // By character rather than by codepoint. A letter carrying a
                // combining mark is two codepoints and a joined emoji is
                // several, and each is one character to replace.
                let mut count = 0usize;
                let mut byte_pos = s;
                while byte_pos < e {
                    let next = rope.next_grapheme_boundary(byte_pos);
                    // The step stands still at the rope end, which a selection
                    // reaching past it would otherwise spin on.
                    if next <= byte_pos {
                        break;
                    }
                    byte_pos = next;
                    count += 1;
                }
                let mut replacement = String::with_capacity(count * ch.len_utf8());
                for _ in 0..count {
                    replacement.push(ch);
                }
                (sel.id, s, e, replacement)
            })
            .collect();
        (buffer_id, entries)
    };

    // A selection covering no character produces no replacement, and editing it
    // would still discard the redo history and record an undo step. Dropping it
    // here leaves its anchors alone, since the transform below only moves a
    // selection it finds an entry for.
    entries.retain(|(_, _, _, replacement)| !replacement.is_empty());

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    entries.sort_by_key(|(_, s, _, _)| *s);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = entries
            .iter()
            .rev()
            .map(|(_, s, e, text)| (*s..*e, text.as_str()))
            .collect();
        guard.edit_batch(&batch);
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
    let editor = ws.editors.get(editor_id).expect("editor");
    let buffer_id = editor.buffer_id;
    let selections = editor.selections.shared_anchors();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let mut guard = buffer.write().expect("poisoned");

    // Seal the open insert group and reopen a fresh one, so the edits before and
    // after the checkpoint undo as two separate steps like Helix's Ctrl-s.
    //
    // Reopening only when there was a group to seal is what keeps this a pure
    // checkpoint outside insert mode. This action runs unwrapped, so a group it
    // opened in normal mode is one nothing would ever close, and it would go on
    // collecting later edits into a step they do not belong to.
    let was_open = guard.group_open();
    guard.seal_group(Arc::clone(&selections));
    if was_open {
        guard.begin_group(selections);
    }
    guard.checkpoint(None);
    UpdateEffect::None
}

fn apply_buffer_history<F>(stoat: &mut Stoat, count: u32, op: F) -> UpdateEffect
where
    F: Fn(&mut crate::buffer::TextBuffer) -> Option<Arc<[Selection<Anchor>]>>,
{
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;

    // Each step returns the selections to restore. Keep the last so a multi-step
    // count lands on the final group's edit-time selections.
    let restored = {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let mut restored: Option<Arc<[Selection<Anchor>]>> = None;
        for _ in 0..count {
            match op(&mut guard) {
                Some(selections) => restored = Some(selections),
                None => break,
            }
        }
        restored
    };

    let Some(selections) = restored else {
        return UpdateEffect::None;
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    if !selections.is_empty() {
        editor.selections.restore(selections);
    }
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| sel.clone());
    UpdateEffect::Redraw
}

/// One row a comment toggle can act on, classified before the set decides
/// which direction to go.
///
/// Blank rows never become a `CommentRow`. They take no edit either way, and
/// letting one in would drag [`Self::indent_chars`] to zero and pull every
/// prefix out to the left margin.
struct CommentRow {
    line_start: usize,
    /// Offset of the row's first non-whitespace byte, where its prefix sits or
    /// would sit if the rows were commented one by one.
    content_start: usize,
    /// Leading whitespace counted in characters rather than display columns, so
    /// a tab counts once no matter how wide it renders.
    indent_chars: usize,
    commented: bool,
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
    let Some(prefix) = language.line_comments.first().copied() else {
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

    let mut commentable: Vec<CommentRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let line_start = rope.point_to_offset(Point::new(row, 0));
        let line_len = rope.line_len(row) as usize;
        let line_end = line_start + line_len;
        let mut content_start = line_start;
        let mut indent_chars = 0;
        for ch in rope.chars_at(line_start) {
            if content_start >= line_end {
                break;
            }
            if !ch.is_whitespace() {
                break;
            }
            content_start += ch.len_utf8();
            indent_chars += 1;
        }
        if content_start >= line_end {
            continue;
        }

        let after_prefix = content_start + prefix.len();
        let commented = after_prefix <= line_end
            && rope
                .chars_at(content_start)
                .take(prefix.chars().count())
                .collect::<String>()
                == prefix;

        commentable.push(CommentRow {
            line_start,
            content_start,
            indent_chars,
            commented,
        });
    }

    if commentable.is_empty() {
        return UpdateEffect::None;
    }

    // One uncommented row commits the whole set to being commented, like Helix.
    // Deciding per row instead inverts each one, so a mixed block stays mixed
    // with its two halves swapped and no number of toggles ever unifies it.
    let comment_all = commentable.iter().any(|r| !r.commented);

    let mut edits: Vec<(usize, usize, String)> = Vec::with_capacity(commentable.len());
    if comment_all {
        // Every prefix goes at the shallowest row's indent so the block keeps
        // its relative indentation. The column is counted in characters, as
        // Helix counts it, so a tab is one column rather than a tab stop.
        let shared_indent = commentable
            .iter()
            .map(|r| r.indent_chars)
            .min()
            .expect("at least one commentable row");
        for row in &commentable {
            let mut insert_at = row.line_start;
            for ch in rope.chars_at(row.line_start).take(shared_indent) {
                insert_at += ch.len_utf8();
            }
            edits.push((insert_at, insert_at, format!("{prefix} ")));
        }
    } else {
        // Removal stays at each row's own prefix rather than the shared column.
        // Helix removes at the shared column, which eats indentation when the
        // commented rows are not equally indented, and removing what each row
        // actually has is what makes the round trip exact.
        for row in &commentable {
            let after_prefix = row.content_start + prefix.len();
            let drop_trailing_space = matches!(rope.chars_at(after_prefix).next(), Some(' '));
            let remove_end = if drop_trailing_space {
                after_prefix + 1
            } else {
                after_prefix
            };
            edits.push((row.content_start, remove_end, String::new()));
        }
    }

    edits.sort_by_key(|(start, _, _)| *start);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = edits
            .iter()
            .rev()
            .map(|(start, end, replacement)| (*start..*end, replacement.as_str()))
            .collect();
        guard.edit_batch(&batch);
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

/// Visual columns a tab advances, for column-accurate unindent. Matches the
/// editor's default render tab size.
const TAB_WIDTH: usize = 4;

fn apply_line_indent(stoat: &mut Stoat, dir: IndentDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let style = ws
        .buffers
        .get(buffer_id)
        .map(|buffer| buffer.read().expect("poisoned").indent_style())
        .unwrap_or_default();

    let mut edits = {
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
            let line_end = rope.point_to_offset(Point::new(row, rope.line_len(row)));
            match dir {
                IndentDir::In => {
                    // Indenting leaves all-whitespace rows untouched, like Helix.
                    if first_nonwhitespace(rope, line_start, line_end).is_none() {
                        continue;
                    }
                    edits.push((line_start, line_start, style.as_str().repeat(count)));
                },
                IndentDir::Out => {
                    // Remove leading whitespace up to `count` indent widths of
                    // visual columns, counting a tab to its next stop.
                    let target = count.saturating_mul(style.indent_width(TAB_WIDTH));
                    let mut width = 0usize;
                    let mut consumed = 0usize;
                    for ch in rope.chars_at(line_start) {
                        match ch {
                            ' ' => width += 1,
                            '\t' => width = (width / TAB_WIDTH + 1) * TAB_WIDTH,
                            _ => break,
                        }
                        consumed += ch.len_utf8();
                        if width >= target {
                            break;
                        }
                    }
                    if consumed > 0 {
                        edits.push((line_start, line_start + consumed, String::new()));
                    }
                },
            }
        }
        edits
    };

    if edits.is_empty() {
        return UpdateEffect::None;
    }

    edits.sort_by_key(|(start, _, _)| *start);

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = edits
            .iter()
            .rev()
            .map(|(start, end, replacement)| (*start..*end, replacement.as_str()))
            .collect();
        guard.edit_batch(&batch);
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

pub(super) fn extend_to_line_bounds(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let max_row = rope.max_point().row;
    let rope_len = rope.len();
    editor.selections.transform(buffer_snapshot, |sel| {
        let (start_row, end_row) = selection_line_range(sel, rope, buffer_snapshot);
        let start = line_start_offset(rope, start_row, max_row, rope_len);
        let end = line_start_offset(rope, end_row + 1, max_row, rope_len);
        line_bound_selection(sel, start, end, rope, buffer_snapshot)
    });
    UpdateEffect::Redraw
}

pub(super) fn shrink_to_line_bounds(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let max_row = rope.max_point().row;
    let rope_len = rope.len();
    editor.selections.transform(buffer_snapshot, |sel| {
        let (start_row, end_row) = selection_line_range(sel, rope, buffer_snapshot);
        // A selection within one line stays put, sparing this command any
        // single-line special cases.
        if start_row == end_row {
            return sel.clone();
        }
        let start_offset = buffer_snapshot.resolve_anchor(&sel.start);
        let end_offset = buffer_snapshot.resolve_anchor(&sel.end);

        let mut start = line_start_offset(rope, start_row, max_row, rope_len);
        let mut end = line_start_offset(rope, end_row + 1, max_row, rope_len);
        // Trim a partial first or last line. A start already at the line
        // boundary stays, otherwise it moves down one line, and likewise the end
        // moves up when it is not at the line-after boundary.
        if start != start_offset {
            start = line_start_offset(rope, start_row + 1, max_row, rope_len);
        }
        if end != end_offset {
            end = line_start_offset(rope, end_row, max_row, rope_len);
        }
        line_bound_selection(sel, start, end, rope, buffer_snapshot)
    });
    UpdateEffect::Redraw
}

/// Offset of the start of display-independent buffer `row`, or the rope end for
/// a row past the last one (so a selection can extend past the final line).
fn line_start_offset(rope: &Rope, row: u32, max_row: u32, rope_len: usize) -> usize {
    if row > max_row {
        rope_len
    } else {
        rope.point_to_offset(Point::new(row, 0))
    }
}

/// The first and last buffer rows a selection covers, following Helix's
/// `line_range`: a forward selection ending exactly at a line start does not
/// count that line, since its block cursor rests on the previous line.
fn selection_line_range(
    sel: &Selection<Anchor>,
    rope: &Rope,
    buffer: &MultiBufferSnapshot,
) -> (u32, u32) {
    let start_offset = buffer.resolve_anchor(&sel.start);
    let end_offset = buffer.resolve_anchor(&sel.end);
    let start_row = rope.offset_to_point(start_offset).row;
    let end_point = rope.offset_to_point(end_offset);
    let end_row = if end_offset > start_offset && end_point.column == 0 {
        end_point.row.saturating_sub(1)
    } else {
        end_point.row
    };
    (start_row, end_row)
}

/// Build a selection spanning `[start, end)`, preserving the source direction
/// and widening a collapsed span to one cell.
fn line_bound_selection(
    sel: &Selection<Anchor>,
    start: usize,
    end: usize,
    rope: &Rope,
    buffer: &MultiBufferSnapshot,
) -> Selection<Anchor> {
    let resolved = Selection {
        id: sel.id,
        start,
        end,
        reversed: sel.reversed,
        goal: SelectionGoal::None,
    }
    .min_width_1(rope);
    anchor_selection(resolved, buffer)
}

pub(super) fn keep_primary_selection(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    editor.selections.keep_primary();
    UpdateEffect::Redraw
}

pub(super) fn merge_selections(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    editor.selections.merge_all();
    UpdateEffect::Redraw
}

pub(super) fn merge_consecutive_selections(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    editor
        .selections
        .merge_consecutive(display_snapshot.buffer_snapshot());
    UpdateEffect::Redraw
}

/// Drop the primary selection, reporting when it is the only one left.
pub(super) fn remove_primary_selection(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    if !editor.selections.remove_primary() {
        stoat.set_status("no selections remaining");
    }
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
            let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
            let tail_offset = buffer_snapshot.resolve_anchor(&sel.tail());
            let cursor = cursor_offset(rope, tail_offset, head_offset);
            land_block_cursor(sel.id, cursor, sel.goal, rope, buffer_snapshot)
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
    let mut target: Option<Range<usize>> = None;
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
        let new_rope = new_buf.rope();
        // The sibling's end is one past its last character, so going forward
        // steps back onto the cell the block cursor should cover. Going
        // backward the node's start is already that cell.
        let target_cursor = match dir {
            SiblingDir::Next => new_rope.prev_grapheme_boundary(target.end),
            SiblingDir::Prev => target.start,
        };
        // Crossing to a sibling moves horizontally, so it drops any column a
        // prior vertical move was holding. Keeping it would send the next
        // vertical move back to the column the sibling was reached from.
        move_cursors(&mut editor.selections, new_buf, true, |_| {
            Some((target_cursor, SelectionGoal::None))
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
    editor
        .selections
        .split_each(buffer_snapshot, Bias::Right, |sel| {
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
            let mut walker = parent_node.walk();
            parent_node
                .named_children(&mut walker)
                .map(|child| {
                    let range = child.byte_range();
                    (range.start, range.end)
                })
                .collect()
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
        // Landing on a node's bound moves horizontally, so it drops any column a
        // prior vertical move was holding, for the same reason as the sibling
        // motion above.
        move_cursors(&mut editor.selections, new_buf, true, |_| {
            Some((target_offset, SelectionGoal::None))
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
    let count = count.max(1);

    if extend {
        // A find is horizontal, so it drops any column a prior vertical move was
        // holding. Carrying it would send the next vertical move back to the
        // column the find started from.
        move_cursors(&mut editor.selections, buffer_snapshot, true, |read| {
            // Each selection scans from its own cursor. Scanning once and
            // stamping the result on all of them makes every span identical, and
            // identical spans merge, so the set collapses to one cursor. A
            // selection whose line holds no match holds its place.
            let cursor = cursor_offset(rope, read.tail, read.head);
            let target = find_target(rope, cursor, kind, ch, count)?;
            Some((target, SelectionGoal::None))
        });
        return UpdateEffect::Redraw;
    }

    editor
        .selections
        .transform_resolved(buffer_snapshot, |sel, head_offset, tail_offset| {
            let cursor = cursor_offset(rope, tail_offset, head_offset);
            // Each selection scans from its own cursor. Scanning once and
            // stamping the result on all of them makes every span identical,
            // and identical spans merge, so the set collapses to one cursor.
            let Some(target) = find_target(rope, cursor, kind, ch, count) else {
                // A selection whose line holds no match holds its place rather
                // than being dragged onto another selection's target.
                return sel.clone();
            };

            // Select from the block cursor to the target rather than collapsing
            // there, so `dfx`/`yfx` operate on the whole span like Helix.
            let landed = Selection {
                id: sel.id,
                start: cursor,
                end: cursor,
                reversed: false,
                goal: SelectionGoal::None,
            }
            .put_cursor(rope, target, true);
            Selection {
                id: sel.id,
                start: buffer_snapshot.anchor_at(landed.start, Bias::Right),
                end: buffer_snapshot.anchor_at(landed.end, Bias::Left),
                reversed: landed.reversed,
                goal: SelectionGoal::None,
            }
        });
    UpdateEffect::Redraw
}

/// Offset the block cursor lands on for a find or till motion starting at
/// `cursor`, or `None` when `ch` does not occur on the rest of that line.
///
/// The scan never leaves the cursor's own line, which is what keeps `f` and its
/// siblings within-line motions.
fn find_target(rope: &Rope, cursor: usize, kind: FindKind, ch: char, count: u32) -> Option<usize> {
    let head_point = rope.offset_to_point(cursor);
    let line_start = rope.point_to_offset(Point::new(head_point.row, 0));
    let max_row = rope.max_point().row;
    let line_end = if head_point.row >= max_row {
        rope.len()
    } else {
        rope.point_to_offset(Point::new(head_point.row + 1, 0))
            .saturating_sub(1)
    };

    // A till motion whose target sits immediately next to the cursor would land
    // where the cursor already is, so bump the count to skip that adjacent
    // grapheme like Helix. Find motions land on the target and are unaffected.
    let count = match kind {
        FindKind::TillNextChar => {
            let adjacent =
                cursor.saturating_add(rope.chars_at(cursor).next().map_or(0, |c| c.len_utf8()));
            if rope.chars_at(adjacent).next() == Some(ch) {
                count + 1
            } else {
                count
            }
        },
        FindKind::TillPrevChar => {
            if rope.reversed_chars_at(cursor).next() == Some(ch) {
                count + 1
            } else {
                count
            }
        },
        FindKind::NextChar | FindKind::PrevChar => count,
    };
    match kind {
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
            let target = found?;
            if matches!(kind, FindKind::TillNextChar) {
                Some(
                    rope.reversed_chars_at(target)
                        .next()
                        .map(|c| target - c.len_utf8())
                        .unwrap_or(target),
                )
            } else {
                Some(target)
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
            let target = found?;
            if matches!(kind, FindKind::TillPrevChar) {
                let len = rope.chars_at(target).next().map_or(0, |c| c.len_utf8());
                Some(target + len)
            } else {
                Some(target)
            }
        },
    }
}

fn apply_primary_range(editor: &mut EditorState, target: Range<usize>) {
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    let widened = Selection {
        id: 0,
        start: target.start,
        end: target.end,
        reversed: false,
        goal: SelectionGoal::None,
    }
    .min_width_1(new_buf.rope());
    let new_start = new_buf.anchor_at(widened.start, Bias::Right);
    let new_end = new_buf.anchor_at(widened.end, Bias::Left);
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
    // In the conflict view the standard change-navigation chords step between
    // conflict chunks. The center is a pathless scratch buffer with no diff map,
    // so the hunk walk below would find nothing. Redirecting to the chunk walk
    // gives the same git-hunk muscle memory. Gate on the per-editor marker, not
    // the session, so a plain diff pane open alongside a conflict still walks its
    // own hunks.
    if focused_editor_mut(stoat).is_some_and(|editor| editor.conflict_view.is_some()) {
        super::conflict_view::conflict_step_chunk(stoat, matches!(dir, ChangeDir::Next));
        return UpdateEffect::Redraw;
    }

    let count = stoat.take_pending_count().unwrap_or(1).max(1) as usize;
    let origin = super::jump::live_entry(stoat);
    let current_path = stoat.focused_editor_ids().and_then(|(_, buffer_id)| {
        stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)
    });
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let source_diff_view = editor.diff_view;
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let sel = editor.selections.newest_anchor().clone();
    let rope = buffer_snapshot.rope();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(rope, tail_off, head_off);
    let cursor_row = rope.offset_to_point(cursor).row;

    let target_row = display_snapshot.diff_map().and_then(|diff_map| match dir {
        ChangeDir::Next => {
            let next: Vec<_> = diff_map
                .hunks_in_range(cursor_row.saturating_add(1)..u32::MAX)
                .into_iter()
                .filter(|h| h.buffer_start_line > cursor_row)
                .collect();
            (!next.is_empty()).then(|| {
                let idx = (count.saturating_sub(1)).min(next.len() - 1);
                next[idx].buffer_start_line
            })
        },
        ChangeDir::Prev => {
            let prev: Vec<_> = diff_map
                .hunks_in_range(0..cursor_row)
                .into_iter()
                .filter(|h| h.buffer_start_line < cursor_row)
                .collect();
            (!prev.is_empty()).then(|| {
                let idx = prev.len().saturating_sub(count);
                prev[idx].buffer_start_line
            })
        },
    });
    let Some(target_row) = target_row else {
        return goto_change_across_files(stoat, dir, current_path, source_diff_view, origin);
    };

    let target_offset = buffer_snapshot
        .rope()
        .point_to_offset(Point::new(target_row, 0));
    editor.selections.transform(buffer_snapshot, |sel| {
        land_block_cursor(
            sel.id,
            target_offset,
            SelectionGoal::None,
            buffer_snapshot.rope(),
            buffer_snapshot,
        )
    });
    if let Some(entry) = origin {
        super::jump::push_entry(stoat, entry);
    }
    UpdateEffect::Redraw
}

/// A cross-file changed hop whose scan has not landed yet.
///
/// Held on [`Stoat`] between the keypress that armed it and the
/// [`pump_changed_file_jump`] that applies it. The two halves the keypress
/// knows and the scan does not travel here.
pub(crate) struct PendingChangedFileJump {
    rx: mpsc::Receiver<ChangedFileJump>,
    _task: Task<()>,
    /// Whether the buffer the hop left was in diff view, which the target
    /// inherits so the mode survives crossing a file boundary.
    source_diff_view: bool,
    /// The jumplist entry for where the hop started, pushed once it lands so a
    /// scan that finds nowhere to go records no jump.
    origin: Option<JumpEntry>,
}

/// What a changed-file scan decided.
enum ChangedFileJump {
    /// Open `path` and land on `line`, which is `None` when the file turned out
    /// to hold no hunk after all.
    To {
        path: PathBuf,
        line: Option<u32>,
        wrapped: bool,
    },
    /// Nowhere else to go.
    NoMoreChanges,
}

/// Jump to the adjacent changed file when the focused buffer has no further
/// hunk in `dir`.
///
/// Returns as soon as the scan is armed. Listing the repo's changed files and
/// diffing the target against HEAD are what the hop costs, and both would run
/// on the thread that handled the key, so they run on a blocking one and
/// [`pump_changed_file_jump`] opens the target when they land.
///
/// Only tracked changed files (modifications, deletions, staged additions) are
/// visited. Untracked working-tree files are excluded because they have no HEAD
/// content, and so no diff to render.
fn goto_change_across_files(
    stoat: &mut Stoat,
    dir: ChangeDir,
    current_path: Option<PathBuf>,
    source_diff_view: bool,
    origin: Option<JumpEntry>,
) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let git_host = stoat.git_host.clone();
    let fs_host = stoat.fs_host.clone();
    let redraw = stoat.redraw_notify.clone();
    let (tx, rx) = mpsc::channel();

    let task = stoat.executor.spawn_blocking(move || {
        let found = scan_changed_file_jump(&git_host, &fs_host, &git_root, current_path, dir);
        let _ = tx.send(found);
        redraw.notify_one();
    });

    stoat.pending_changed_file_jump = Some(PendingChangedFileJump {
        rx,
        _task: task,
        source_diff_view,
        origin,
    });
    UpdateEffect::Redraw
}

/// Pick the changed file `dir` leads to from `current_path`, and the row in it
/// to land on.
///
/// Runs off the UI thread, so it reads the target's working-tree side through
/// `fs_host` rather than through any open buffer. For the file a hop is
/// crossing into those hold the same text, that file being by definition one
/// the reader does not have in front of them.
fn scan_changed_file_jump(
    git_host: &Arc<dyn GitHost>,
    fs_host: &Arc<dyn FsHost>,
    git_root: &Path,
    current_path: Option<PathBuf>,
    dir: ChangeDir,
) -> ChangedFileJump {
    let Some(repo) = git_host.discover(git_root) else {
        return ChangedFileJump::NoMoreChanges;
    };
    let changed: Vec<PathBuf> = repo
        .changed_files()
        .into_iter()
        .filter(|f| !f.untracked)
        .map(|f| f.path)
        .collect();
    let current_index = current_path
        .as_deref()
        .and_then(|path| changed.iter().position(|c| c == path));
    // Cross into a lone changed file when the current buffer is not itself in
    // the list. Only bail when nothing changed, or the current buffer is the
    // sole changed file and there is nowhere else to go.
    if changed.is_empty() || (current_index.is_some() && changed.len() < 2) {
        return ChangedFileJump::NoMoreChanges;
    }

    let (target_index, wrapped) = match (current_index, dir) {
        (Some(i), ChangeDir::Next) if i + 1 < changed.len() => (i + 1, false),
        (Some(_), ChangeDir::Next) => (0, true),
        (Some(i), ChangeDir::Prev) if i > 0 => (i - 1, false),
        (Some(_), ChangeDir::Prev) => (changed.len() - 1, true),
        (None, ChangeDir::Next) => (0, false),
        (None, ChangeDir::Prev) => (changed.len() - 1, false),
    };
    let path = changed[target_index].clone();
    let line = first_hunk_row(&*repo, fs_host, &path, dir);

    ChangedFileJump::To {
        path,
        line,
        wrapped,
    }
}

/// The row of `path`'s first (Next) or last (Prev) hunk against HEAD.
///
/// `None` when the file has no HEAD blob, cannot be read, or diffs clean, in
/// which case the hop opens it and leaves the cursor where the open put it.
fn first_hunk_row(
    repo: &dyn GitRepo,
    fs_host: &Arc<dyn FsHost>,
    path: &Path,
    dir: ChangeDir,
) -> Option<u32> {
    let base = repo.head_content(path)?;
    let mut bytes = Vec::new();
    fs_host.read(path, &mut bytes).ok()?;
    let working = String::from_utf8(bytes).ok()?;

    let result = structural_diff::diff(&base, &working);
    let hunks = diff_map::changes_to_hunks(&result.changes, &base, &working);
    match dir {
        ChangeDir::Next => hunks.first().map(|h| h.buffer_start_line),
        ChangeDir::Prev => hunks.last().map(|h| h.buffer_start_line),
    }
}

/// Apply a landed changed-file hop, opening the target and landing on its hunk.
pub(crate) fn pump_changed_file_jump(stoat: &mut Stoat) -> bool {
    let Some(pending) = stoat.pending_changed_file_jump.take() else {
        return false;
    };
    let found = match pending.rx.try_recv() {
        Ok(found) => found,
        Err(mpsc::TryRecvError::Empty) => {
            stoat.pending_changed_file_jump = Some(pending);
            return false;
        },
        Err(mpsc::TryRecvError::Disconnected) => return false,
    };

    let (path, line, wrapped) = match found {
        ChangedFileJump::To {
            path,
            line,
            wrapped,
        } => (path, line, wrapped),
        ChangedFileJump::NoMoreChanges => {
            stoat.set_status("no more changes");
            return true;
        },
    };

    let focused_pane = stoat.active_workspace().panes.focus();
    if crate::buffer_lifecycle::open_file_in_pane(stoat, focused_pane, &path).is_none() {
        return true;
    }

    if pending.source_diff_view
        && let Some(editor) = focused_editor_mut(stoat)
    {
        editor.set_diff_view(true);
    }

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(target_row) = line
        && let Some(editor) = focused_editor_mut(stoat)
    {
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let target_offset = buffer_snapshot
            .rope()
            .point_to_offset(Point::new(target_row, 0));
        editor.selections.transform(buffer_snapshot, |sel| {
            land_block_cursor(
                sel.id,
                target_offset,
                SelectionGoal::None,
                buffer_snapshot.rope(),
                buffer_snapshot,
            )
        });
        // Pull the view onto the landed hunk here, so a startup or mouse
        // dispatch that skips the Key-event epilogue still lands scrolled.
        view::ensure_cursor_in_view(editor, scrolloff);
    }

    if let Some(entry) = pending.origin {
        super::jump::push_entry(stoat, entry);
    }
    if wrapped {
        stoat.set_status("wrapped");
    }
    true
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ParaDir {
    Next,
    Prev,
}

/// Rows read per refill of [`BlankRows`].
///
/// A scan that stops after a row or two still fills a whole window, which is
/// the price of the scans this is for, where the same walk crosses hundreds of
/// rows without finding a blank.
const BLANK_ROW_WINDOW: u32 = 128;

/// Blank-row lookups over a rope, reading rows in windows rather than seeking
/// the tree for each one.
///
/// Paragraph scans step outward from the cursor a row at a time, and a file
/// with no blank lines makes them cross all of it, so a descent per probe is
/// what this exists to avoid.
///
/// A probe outside the window refills it on the side the probe left from,
/// which is what lets a scan running backward be served as well as one running
/// forward.
pub(super) struct BlankRows<'a> {
    rope: &'a Rope,
    /// Byte length of each row in [`Self::window`].
    lens: Vec<u32>,
    window: Range<u32>,
}

impl<'a> BlankRows<'a> {
    pub(super) fn new(rope: &'a Rope) -> Self {
        Self {
            rope,
            lens: Vec::new(),
            window: 0..0,
        }
    }

    /// Whether `row` holds no text.
    ///
    /// A row past the end of the rope reads as blank, matching the length the
    /// rope reports for one, so a caller's bounds check stays its own business.
    pub(super) fn is_blank(&mut self, row: u32) -> bool {
        if !self.window.contains(&row) {
            let start = if row < self.window.start {
                row.saturating_sub(BLANK_ROW_WINDOW - 1)
            } else {
                row
            };
            self.window = start..start + BLANK_ROW_WINDOW;
            self.lens = self.rope.line_lens_in_range(self.window.clone());
        }
        self.lens[(row - self.window.start) as usize] == 0
    }
}

pub(super) fn goto_paragraph(stoat: &mut Stoat, dir: ParaDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let sel = editor.selections.newest_anchor().clone();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(rope, tail_off, head_off);
    let cursor_row = rope.offset_to_point(cursor).row;
    let mut blanks = BlankRows::new(rope);
    let mut last_content_row = rope.max_point().row;
    if last_content_row > 0 && blanks.is_blank(last_content_row) {
        last_content_row -= 1;
    }

    let step = |blanks: &mut BlankRows<'_>, current: u32| -> Option<u32> {
        match dir {
            ParaDir::Next => {
                if current >= last_content_row {
                    return None;
                }
                let mut row = current;
                while row <= last_content_row && !blanks.is_blank(row) {
                    row += 1;
                }
                if row > last_content_row {
                    return None;
                }
                while row <= last_content_row && blanks.is_blank(row) {
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
                while row > 0 && blanks.is_blank(row) {
                    row -= 1;
                }
                while row > 0 && !blanks.is_blank(row) {
                    row -= 1;
                }
                if blanks.is_blank(row) && row < last_content_row {
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
        match step(&mut blanks, target_row) {
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

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;

    let tree_opt: Option<stoat_language::Tree> = ws
        .buffers
        .syntax_map(buffer_id)
        .and_then(|sm| sm.snapshot().iter_layers().next().map(|l| l.tree.clone()));

    let language = ws.buffers.language_for(buffer_id);
    let bracket_query = language.as_ref().and_then(|lang| lang.bracket_query());

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    move_cursors(&mut editor.selections, buffer_snapshot, false, |read| {
        // Every cursor pairs its own bracket. Resolving one partner and landing
        // it on all of them makes every span identical, and identical spans
        // merge, so the set collapses to one cursor. A cursor sitting in no pair
        // holds its place.
        let cursor = cursor_offset(rope, read.tail, read.head);
        let target = bracket_partner(rope, cursor, bracket_query, tree_opt.as_ref())?;
        Some((target, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

/// Offset of the bracket partnering the one the cursor sits in, or `None` when
/// it sits in no pair.
///
/// A brackets query captures only structural delimiters, so a bracket inside a
/// string, char, or comment literal resolves to no pair instead of
/// false-matching. When the language ships one it is authoritative and matches
/// from within a pair, not only on a delimiter, so the char under the cursor
/// does not gate the query path.
///
/// The text scan below only runs for languages without a query (e.g. toml), and
/// matches only when the cursor is on a delimiter, which is the plaintext
/// behavior. From-within matching is a syntax-path feature.
fn bracket_partner(
    rope: &Rope,
    cursor: usize,
    query: Option<&stoat_language::Query>,
    tree: Option<&stoat_language::Tree>,
) -> Option<usize> {
    let ch = rope.chars_at(cursor).next()?;

    if let (Some(query), Some(tree)) = (query, tree) {
        return stoat_language::matching_bracket(query, tree.root_node(), rope, cursor);
    }

    let (open, close, forward) = bracket_pair(ch)?;
    let scan = PairScan::around(tree, cursor);
    if scan.skips(cursor) {
        return None;
    }
    scan_bracket_match(rope, cursor, ch, open, close, forward, &scan)
}

/// How far a pair scan walks before giving up, in characters each way.
///
/// Picking the closest surrounding pair tries every pair type in both
/// directions, so a press with nothing around it walks the document once per
/// direction per type. Without a bound that is the whole file, fourteen times,
/// to conclude there was nothing there.
///
/// A genuine pair further apart than this is no longer found, which is the
/// price. Ten thousand characters is far past any construct a reader is asking
/// about and far short of what makes the walk noticeable.
pub(crate) const MAX_PAIR_SCAN: usize = 10_000;

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

/// How far either side of a cursor a pair scan can reach, in bytes.
///
/// [`MAX_PAIR_SCAN`] counts characters and a character is at most four bytes,
/// so this is the furthest a scan can look however the text is encoded.
const PAIR_SCAN_WINDOW_BYTES: usize = MAX_PAIR_SCAN * 4;

/// The byte ranges a pair scan reads as text rather than as code.
///
/// A bracket inside a string or a comment is not a delimiter. Asking the syntax
/// tree that per character means a descendant lookup from the root plus an
/// ancestor walk each time, and a scan crosses thousands of characters per
/// press, per cursor. Collected once for the window a scan can reach and
/// answered by binary search instead.
///
/// Ranges are sorted by start and non-overlapping, a nested one having been
/// merged into whatever encloses it.
pub(crate) struct SkipZones {
    ranges: Vec<Range<usize>>,
}

impl SkipZones {
    /// Every string or comment node of `tree` intersecting `window`.
    ///
    /// A wider window is always safe. It only collects zones a scan never asks
    /// about, and asking outside the collected window is what would be wrong.
    pub(crate) fn collect(tree: Option<&stoat_language::Tree>, window: Range<usize>) -> Self {
        let mut ranges: Vec<Range<usize>> = Vec::new();
        let Some(tree) = tree else {
            return Self { ranges };
        };

        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            let range = node.byte_range();
            if range.start >= window.end || range.end <= window.start {
                continue;
            }
            let kind = node.kind();
            if kind.contains("string") || kind.contains("comment") {
                // Whatever is inside is inside this range too, so the subtree
                // adds nothing.
                ranges.push(range);
                continue;
            }
            for i in 0..node.child_count() as u32 {
                if let Some(child) = node.child(i) {
                    pending.push(child);
                }
            }
        }

        ranges.sort_unstable_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(prev) if range.start <= prev.end => prev.end = prev.end.max(range.end),
                _ => merged.push(range),
            }
        }
        Self { ranges: merged }
    }

    pub(crate) fn contains(&self, offset: usize) -> bool {
        self.ranges
            .binary_search_by(|range| {
                if range.end <= offset {
                    Ordering::Less
                } else if range.start > offset {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            })
            .is_ok()
    }
}

/// A syntax tree together with the skip zones a pair scan reads off it.
///
/// The scans need the tree itself for the one question zones cannot answer,
/// which is where the string node under a cursor begins and ends. Carrying both
/// replaces the tree the scans already thread rather than adding a second
/// parameter beside it.
pub(crate) struct PairScan<'a> {
    pub(crate) tree: Option<&'a stoat_language::Tree>,
    pub(crate) zones: SkipZones,
}

impl<'a> PairScan<'a> {
    /// Zones for everything a scan from `cursor` can reach.
    pub(crate) fn around(tree: Option<&'a stoat_language::Tree>, cursor: usize) -> Self {
        Self::over(tree, window_around(cursor))
    }

    pub(crate) fn over(tree: Option<&'a stoat_language::Tree>, window: Range<usize>) -> Self {
        Self {
            zones: SkipZones::collect(tree, window),
            tree,
        }
    }

    pub(crate) fn skips(&self, offset: usize) -> bool {
        self.zones.contains(offset)
    }
}

/// The byte window a scan from `cursor` can reach.
pub(crate) fn window_around(cursor: usize) -> Range<usize> {
    cursor.saturating_sub(PAIR_SCAN_WINDOW_BYTES)..cursor.saturating_add(PAIR_SCAN_WINDOW_BYTES)
}

/// Test-only since the scans read [`SkipZones`] instead. It stays as what that
/// list is checked against, being the rule the list has to reproduce.
#[cfg(test)]
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
    scan: &PairScan<'_>,
) -> Option<usize> {
    let mut depth: u32 = 1;
    let in_skip_zone = |offset: usize| scan.skips(offset);
    // Bounded like the surround walks. A language with no bracket query leaves
    // the tree absent, so an unmatched bracket would otherwise read to the end
    // of the file to report that there is no match, once per press.
    if forward {
        let mut cur = start + start_ch.len_utf8();
        for c in rope.chars_at(cur).take(MAX_PAIR_SCAN) {
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
        for c in rope.reversed_chars_at(start).take(MAX_PAIR_SCAN) {
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

    let steps = count.saturating_sub(1) as usize;

    // Naming a column is itself a horizontal move, so it replaces whatever
    // column a prior vertical move was holding.
    move_cursors(&mut editor.selections, buffer_snapshot, extend, |read| {
        // Each cursor names the column on its own row. Computing one target and
        // stamping it on all of them makes every span identical, and identical
        // spans merge, so the set collapses to one cursor.
        let cursor = cursor_offset(rope, read.tail, read.head);
        let row = rope.offset_to_point(cursor).row;
        let line_start = rope.point_to_offset(Point::new(row, 0));
        let line_end = rope.point_to_offset(Point::new(row, rope.line_len(row)));

        // A column is a grapheme cluster. Stepping codepoints instead walks into
        // the middle of a letter carrying a combining mark and stops a column
        // short of the one asked for.
        let mut target_offset = line_start;
        for _ in 0..steps {
            let next = rope.next_grapheme_boundary(target_offset);
            if next > line_end || next == target_offset {
                break;
            }
            target_offset = next;
        }

        Some((target_offset, SelectionGoal::None))
    });
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
    move_cursors(&mut editor.selections, buffer_snapshot, extend, |_| {
        Some((target_offset, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

/// Land every selection as a 1-wide block cursor on `offset`. Returns
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
    editor.selections.transform(buffer_snapshot, |sel| {
        land_block_cursor(sel.id, clamped, SelectionGoal::None, rope, buffer_snapshot)
    });
    UpdateEffect::Redraw
}

pub(super) fn goto_word(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor
        .viewport_rows
        .unwrap_or(view::DEFAULT_VIEWPORT_ROWS)
        .max(1);
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

#[cfg(test)]
mod tests;
