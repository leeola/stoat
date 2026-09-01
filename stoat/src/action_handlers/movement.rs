use super::{split_selection, surround, view, LastMotion};
use crate::{
    action_handlers::focused_editor_mut,
    app::{Stoat, UpdateEffect},
    diff_map,
    display_map::{DisplayPoint, DisplaySnapshot},
    editor_state::{EditorId, EditorState},
    host::{FsHost, GitHost, GitRepo},
    jumplist::JumpEntry,
    multi_buffer::MultiBufferSnapshot,
    pane::View,
    selection::{
        anchor_selection, forward_block_cursor, land_block_cursor, merge_overlapping_spans,
        ResolvedRead, SelectionsCollection, SpanLanding,
    },
    workspace::{diff::DiffBase, Workspace},
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
    cursor_offset, date_time_increment, integer_increment, next_char_boundary,
    next_long_word_end_range, next_long_word_start_range, next_word_end_range,
    next_word_start_range, prev_long_word_end_range, prev_long_word_start_range,
    prev_word_end_range, prev_word_start_range, Anchor, Bias, IndentStyle, Point, Rope, Selection,
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
    editor.scroll_frac = 0.0;
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
    let primary_source_id = editor.selections.newest_anchor().id;
    // Which copy the primary passes to. The last one made from the primary's
    // own source, so the user keeps working down the column they started, and
    // none when that source found nowhere to copy to.
    let mut primary_copy: Option<usize> = None;
    let mut copies: Vec<Selection<usize>> = Vec::new();
    for source in &sources {
        // Both ends read as the cells the source covers, never the boundary
        // past one. Whichever end faces forward is the exclusive one, so a
        // forward selection steps its head back and a reversed one its tail.
        // Reading an unstepped tail makes a reversed copy a cell too wide, and
        // where that tail sits on the next row it inflates the height too, so
        // the copy lands a whole selection further than its source.
        //
        // A zero-width range has no forward end and steps its tail like a
        // reversed one, which is why the test reads strictly-forward rather
        // than reversed. At the buffer end that step reaches the row above, and
        // the copy is two rows tall.
        let tail_off = buffer.resolve_anchor(&source.tail());
        let head_raw = buffer.resolve_anchor(&source.head());
        let (anchor_off, head_off) = match tail_off < head_raw {
            true => (tail_off, cursor_offset(rope, tail_off, head_raw)),
            false => (rope.prev_grapheme_boundary(tail_off), head_raw),
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
                // A step past the top of the buffer lands on row 0 rather than
                // ending the walk, so a source two rows down still copies onto
                // the first row. The row-0 break below is what stops it there.
                AddDirection::Above => (
                    anchor_pt.row.saturating_sub(offset),
                    head_pt.row.saturating_sub(offset),
                ),
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
                if source.id == primary_source_id {
                    primary_copy = Some(copies.len());
                }
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
    match primary_copy {
        Some(index) => editor
            .selections
            .extend_with_fresh_ids_primary(added, index, buffer),
        // The primary's own source copied nowhere, so the primary stays on it
        // rather than jumping to some other source's copy.
        None => {
            editor.selections.extend_with_fresh_ids(added, buffer);
            editor.selections.make_primary(primary_source_id);
        },
    }
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
        // A step that reaches the cell it started on still lands. That is what
        // collapses a wide selection to a cursor when the user presses `h` at
        // the buffer start, where holding the selection instead reads as the
        // key doing nothing at all.
        Some((target, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

/// The rows a vertical motion counts.
#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum VerticalStep {
    /// Rows on the screen, so a soft-wrapped line takes as many presses to
    /// cross as it takes rows to draw. Backs `j` and `k`.
    ScreenRow,
    /// Rows in the text, so a soft-wrapped line is one press however it draws.
    /// The unbound counterpart, for a user who binds it.
    TextLine,
}

pub(super) fn move_vertical(stoat: &mut Stoat, delta: i32, extend: bool) -> UpdateEffect {
    move_vertical_by(stoat, delta, extend, VerticalStep::ScreenRow)
}

pub(super) fn move_vertical_by_line(stoat: &mut Stoat, delta: i32, extend: bool) -> UpdateEffect {
    move_vertical_by(stoat, delta, extend, VerticalStep::TextLine)
}

fn move_vertical_by(
    stoat: &mut Stoat,
    delta: i32,
    extend: bool,
    step: VerticalStep,
) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    let delta = (delta as i64).saturating_mul(count as i64);
    let travel = if delta > 0 { Bias::Right } else { Bias::Left };
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

    let max_display_row = display_snapshot.max_point().row;

    // Where a cursor at `head`/`tail` carrying `goal` lands, with the goal
    // column it takes along. Both arms land on the same cell. Extending decides
    // whether the anchor is left behind, never where the cursor ends up.
    let landing_for = |head: usize, tail: usize, goal: SelectionGoal| {
        let cursor = cursor_offset(rope, tail, head);
        let cursor_pt = rope.offset_to_point(cursor);
        let cursor_display = display_snapshot.buffer_to_display(cursor_pt);
        // Cells from the display row's start, not bytes. The two agree only
        // while every character is one byte and one cell, so carrying bytes
        // between rows drifts across wide glyphs and tabs.
        let goal_col = match goal {
            SelectionGoal::Column(c) => c,
            SelectionGoal::None => cursor_display.column,
        };
        // A text-line step counts rows in the buffer, then aims at the target
        // line's first screen row. The goal column stays a screen column either
        // way, so the two steps differ only in how far a wrap carries them.
        let new_row = match step {
            VerticalStep::ScreenRow => (cursor_display.row as i64)
                .saturating_add(delta)
                .clamp(0, max_display_row as i64) as u32,
            VerticalStep::TextLine => {
                let line = (cursor_pt.row as i64)
                    .saturating_add(delta)
                    .clamp(0, max_row as i64) as u32;
                display_snapshot.buffer_to_display(Point::new(line, 0)).row
            },
        };
        // The clamp above is what keeps an overshooting count on the edge row
        // rather than past the buffer. A step that reaches the row it started
        // on still lands, collapsing a wide selection the way `h` at the buffer
        // start does.
        let clipped = clip_to_goal(&display_snapshot, new_row, goal_col, travel);
        let buffer_pt = display_snapshot.display_to_buffer(clipped)?;
        if extend && Some(buffer_pt.row) == empty_final_row {
            return None;
        }
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

/// Where a motion carrying the cell column `goal` lands on display `row`.
///
/// A tab or a wide glyph covers several cells, so a goal inside one names no
/// position of its own. Both directions answer with the glyph the goal sits in,
/// which is the cell under the column the user aimed at. Resolving forward
/// instead hands back the character after it, so the same column reads as two
/// different cells depending on which way the cursor arrived.
///
/// `travel` decides only which row to leave for. A row holding no text at all,
/// a fold placeholder or a diff block row, has no position on it to answer
/// with, and clipping left there walks back the way the motion came. The second
/// clip covers that case and only that case.
pub(super) fn clip_to_goal(
    display: &DisplaySnapshot,
    row: u32,
    goal: u32,
    travel: Bias,
) -> DisplayPoint {
    let point = DisplayPoint::new(row, goal);
    let clipped = display.clip_point(point, Bias::Left);
    if clipped.row == row {
        return clipped;
    }
    display.clip_point(point, travel)
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
///
/// Sorted by id, which is what the landing lookup binary-searches.
fn span_landings<F>(
    selections: &SelectionsCollection,
    buffer: &MultiBufferSnapshot,
    landing_for: F,
) -> Vec<SpanLanding>
where
    F: Fn(&ResolvedRead) -> Option<SpanLanding>,
{
    let mut landings: Vec<SpanLanding> = selections
        .resolved_reads(buffer)
        .iter()
        .filter_map(landing_for)
        .collect();
    landings.sort_unstable_by_key(|landing| landing.id);
    landings
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
    // edited buffer. Summed once over the whole list rather than per query,
    // which every cursor and every plan makes one of.
    let inserted_before: Vec<usize> = {
        let mut running = 0;
        std::iter::once(0)
            .chain(inserts.iter().map(|ins| {
                running += ins.indent.len();
                running
            }))
            .collect()
    };
    // The insertions are in offset order, so the count of those ahead of an
    // offset indexes the running total directly.
    let shift_before =
        |off: usize| -> usize { inserted_before[inserts.partition_point(|ins| ins.at < off)] };

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

/// Move each cursor to the insert point past its line's last character.
///
/// The insert-mode End key. [`goto_line_end`] lands on the last character
/// itself, which is where a block cursor belongs while reading. A caret about
/// to type belongs one cell further on, which is the same landing [`A`] gives.
///
/// [`A`]: insert_at_line_end
pub(crate) fn goto_line_end_newline(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    goto_line_boundary(stoat, LineBoundary::EndNewline, extend)
}

#[derive(Copy, Clone)]
enum LineBoundary {
    Start,
    End,
    /// The line's end itself, where the End arm lands on the character before
    /// it.
    EndNewline,
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
    // to step back to and stays at its start. The insert-mode end keeps the
    // boundary itself, since that is where a caret about to type belongs.
    let target_for = |head: usize, tail: usize| {
        // Use the block-cursor cell's row, not the raw head. A 1-wide cursor
        // sitting at a line's end has its head on the next line's first cell,
        // which would move the boundary to the wrong line.
        let cursor_row = rope.offset_to_point(cursor_offset(rope, tail, head)).row;
        let line_start = rope.point_to_offset(Point::new(cursor_row, 0));
        let line_end = || rope.point_to_offset(Point::new(cursor_row, rope.line_len(cursor_row)));
        match boundary {
            LineBoundary::Start => line_start,
            LineBoundary::End => {
                let end = line_end();
                match end > line_start {
                    true => rope.prev_grapheme_boundary(end),
                    false => end,
                }
            },
            LineBoundary::EndNewline => line_end(),
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

/// Land on the file's first character, or on the line a count names.
///
/// A count turns this into the same numbered-line jump `G` makes, which is
/// what a user who typed one asked for. Without one it is the top of the file.
pub(super) fn goto_file_start(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count();
    super::jump::push_jump(stoat);
    if let Some(count) = count {
        return goto_line_row(stoat, count, extend);
    }

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

            // Where each turn joins onto is where the turn before worked out
            // its target ended, and the rope is not edited until the whole
            // batch lands, so the answer is carried rather than asked for
            // again.
            let mut join_start = first_end;
            for line in start_row..end_row {
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
                join_start = next_end;
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
        // The spaces come out in document order, and the front of that is where
        // the user reads on from, so the first one takes the primary rather than
        // the last one the loop happened to mint.
        editor
            .selections
            .replace_with_fresh_ids_primary(spaces, 0, new_buf);
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
            // Cells along the buffer line, not along a display row. A head past
            // the wrap width draws on a continuation row, whose column restarts
            // from zero and whose row is not its line's first. A display measure
            // therefore reads a column the text does not have, and splits two
            // heads on one line onto rows that never rank against each other.
            out.push(AlignEntry {
                insert_offset: start_offset,
                head_col: display_snapshot.visual_column(head_pt),
                head_row: head_pt.row,
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

    // The entries arrive in position order, so equal head rows come as one run
    // and a row is new exactly when the rank resets. That is the whole of the
    // row numbering, with no list to search.
    let mut ranked: Vec<RankedEntry> = Vec::with_capacity(entries.len());
    let mut row_count = 0usize;
    let mut last_row: Option<u32> = None;
    let mut rank: usize = 0;
    for entry in entries {
        if Some(entry.head_row) == last_row {
            rank += 1;
        } else {
            rank = 0;
            last_row = Some(entry.head_row);
            row_count += 1;
        }
        ranked.push(RankedEntry {
            insert_offset: entry.insert_offset,
            head_col: entry.head_col,
            row_idx: row_count - 1,
            rank,
        });
    }

    // Grouped once rather than filtered per rank, which is two passes over
    // every entry for each rank a row reaches.
    let mut by_rank: Vec<Vec<usize>> = Vec::new();
    for (index, entry) in ranked.iter().enumerate() {
        if by_rank.len() <= entry.rank {
            by_rank.resize(entry.rank + 1, Vec::new());
        }
        by_rank[entry.rank].push(index);
    }

    let mut offs = vec![0u32; row_count];
    let mut edits: Vec<(usize, String)> = Vec::new();

    // In rank order, since a rank's target column is read after the ranks
    // before it have already pushed their rows right.
    for bucket in &by_rank {
        let max_col = bucket
            .iter()
            .map(|&index| ranked[index].head_col + offs[ranked[index].row_idx])
            .max();
        let Some(max_col) = max_col else { continue };

        for &index in bucket {
            let entry = &ranked[index];
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
    /// Buffer row, which the rank loop only ever compares for equality. A
    /// display row splits one soft-wrapped line into several, and two heads on
    /// one line then rank as one apiece.
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

            // A selection with nothing to split still comes out rebuilt rather
            // than passed through, so it picks up the forward direction every
            // other piece gets.
            if newline_positions.is_empty() {
                return split_selection::widen_pieces(rope, vec![(start_offset, end_offset)]);
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
    let in_select = stoat.in_select_mode();
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
        // The selection's own text is the number. A selection covering anything
        // else spells no integer and stays where it is, so the arithmetic
        // follows what the reader picked out rather than an invisible scan.
        let edits: Vec<(usize, usize, usize, String)> = editor
            .selections
            .all_anchors()
            .iter()
            .filter_map(|sel| {
                let start = buffer_snapshot.resolve_anchor(&sel.start);
                let end = buffer_snapshot.resolve_anchor(&sel.end);
                let text = rope.slice(start..end).to_string();
                let new_text = integer_increment(&text, delta)
                    .or_else(|| date_time_increment(&text, delta))?;
                if new_text == text {
                    return None;
                }
                Some((sel.id, start, end, new_text))
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

    // The span the reader picked out is what the arithmetic consumed, so once
    // an edit lands the selection has served its purpose and select mode ends.
    if in_select {
        stoat.set_focused_mode("normal".into());
    }
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
        let target = stoat.active_register();
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
    change_selection_impl(stoat, true)
}

/// Like [`change_selection`], but the deleted text does not reach any register.
pub(super) fn change_selection_no_yank(stoat: &mut Stoat) -> UpdateEffect {
    change_selection_impl(stoat, false)
}

fn change_selection_impl(stoat: &mut Stoat, yank: bool) -> UpdateEffect {
    let whole_lines = selections_are_whole_lines(stoat);
    let deleted = delete_selection_impl(stoat, yank);
    if whole_lines {
        open_line(stoat, OpenDir::Above, CommentContinuation::Disabled)
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

/// Whether an opened line carries the current line's comment token forward.
///
/// `o` and `O` continue the comment, which is what writing a second line of
/// one wants. A change has already deleted the commented line and opens a
/// replacement for it, so continuing there puts a token in front of text the
/// user never asked to comment.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CommentContinuation {
    Enabled,
    Disabled,
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
    /// Offset the indent query is asked about, being the end of the line the
    /// opened one follows.
    ///
    /// That is the current line's end going down and the previous line's end
    /// going up, so both directions ask about the line the new one continues
    /// from. It differs from `insert_offset`, which for an upward open is the
    /// current line's start.
    indent_at: usize,
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

pub(super) fn open_line(
    stoat: &mut Stoat,
    dir: OpenDir,
    continuation: CommentContinuation,
) -> UpdateEffect {
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
        // No tokens means no line continues one, which is the same shape a
        // language without line comments already takes.
        let comment_tokens = match continuation {
            CommentContinuation::Enabled => ws
                .buffers
                .language_for(buffer_id)
                .map_or(&[][..], |lang| lang.line_comments),
            CommentContinuation::Disabled => &[][..],
        };
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
                let indent_at = match dir {
                    // The previous line's end, which the newline before this
                    // line sits on. The first row has no previous line, so the
                    // query is asked about the buffer's start.
                    OpenDir::Above => line_start.saturating_sub(1),
                    OpenDir::Below => line_end,
                };
                let continued = line_comment_continues(rope, line_start, line_end, comment_tokens)
                    .map(|(_, token)| token);
                OpenSite {
                    id: sel.id,
                    insert_offset,
                    row,
                    indent_at,
                    continued,
                }
            })
            .collect();
        (buffer_id, sites)
    };

    if sites.is_empty() {
        return UpdateEffect::None;
    }

    // Both directions read the indents query, each asked about the line the
    // opened one follows, so opening above a block's closing line indents the
    // same as opening below its opening line. A comment continuation instead
    // aligns to the line's own leading whitespace and carries the token
    // forward, which is the one case the query does not answer.
    let mut units: Vec<OpenUnit> = sites
        .iter()
        .map(|site| {
            let indent = match site.continued.is_some() {
                true => stoat.line_indent_string(buffer_id, site.row),
                false => stoat.newline_indent_string(buffer_id, site.indent_at),
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

/// Replace every character of every non-empty selection with `text`.
///
/// `text` is one keypress worth of replacement, and it repeats once per
/// grapheme covered rather than once per selection. It is a string rather than
/// a character because Enter and Tab name replacements no character key
/// reaches.
pub(crate) fn execute_replace(stoat: &mut Stoat, text: &str) -> UpdateEffect {
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
                (sel.id, s, e, text.repeat(count))
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
    // Select mode ends here rather than at the binding. The binding fires when
    // the chord arms, a keypress before the replacement char arrives, and a
    // chord cancelled in between then leaves select mode having changed
    // nothing.
    stoat.set_focused_mode("normal".to_string());
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

pub(super) fn earlier(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    apply_buffer_history(stoat, count, |buf| buf.earlier())
}

pub(super) fn later(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    apply_buffer_history(stoat, count, |buf| buf.later())
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
    /// The longest comment token this row starts with, `None` when the row is
    /// not commented.
    ///
    /// Per row rather than per operation, for the same reason removal works off
    /// each row's own prefix rather than a shared column. A doc comment starts
    /// with the ordinary token too, so removing the operation's token leaves
    /// the rest of the row's own token behind.
    token: Option<&'static str>,
    /// End of the row's text, before its line ending. Paired with
    /// [`Self::content_start`] it is the span a block comment wraps.
    line_end: usize,
}

/// Which comment syntax a toggle uses when the language offers both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommentStyle {
    /// Line tokens where the language has them, block tokens where it does not,
    /// and block tokens either way when the rows are already block-commented.
    Auto,
    Line,
    Block,
}

pub(super) fn toggle_comments(stoat: &mut Stoat) -> UpdateEffect {
    toggle_comments_with(stoat, CommentStyle::Auto)
}

pub(super) fn toggle_line_comments(stoat: &mut Stoat) -> UpdateEffect {
    toggle_comments_with(stoat, CommentStyle::Line)
}

pub(super) fn toggle_block_comments(stoat: &mut Stoat) -> UpdateEffect {
    toggle_comments_with(stoat, CommentStyle::Block)
}

fn toggle_comments_with(stoat: &mut Stoat, style: CommentStyle) -> UpdateEffect {
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
    let line_prefix = language.line_comments.first().copied();
    let block_tokens = language.block_comments;

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

        commentable.push(CommentRow {
            line_start,
            content_start,
            indent_chars,
            token: longest_token_at(rope, content_start, line_end, language.line_comments),
            line_end,
        });
    }

    if commentable.is_empty() {
        return UpdateEffect::None;
    }

    // Helix's ladder, with one block pair rather than several. Rows already
    // block-commented come off whatever else the language offers, since the key
    // that made them is the key that has to undo them. Otherwise line tokens
    // win, and block tokens are the fallback for a language with none.
    let block_commented = block_tokens.is_some_and(|tokens| {
        commentable.iter().all(|row| {
            stoat_text::is_block_commented(rope, row.content_start..row.line_end, tokens)
        })
    });
    let use_block = match style {
        CommentStyle::Line => line_prefix.is_none(),
        CommentStyle::Block => block_tokens.is_some(),
        CommentStyle::Auto => block_commented || line_prefix.is_none(),
    };

    if use_block && let Some(tokens) = block_tokens {
        return apply_block_comments(ws, editor_id, buffer_id, &commentable, tokens);
    }
    let Some(prefix) = line_prefix else {
        return UpdateEffect::None;
    };

    // One uncommented row commits the whole set to being commented, like Helix.
    // Deciding per row instead inverts each one, so a mixed block stays mixed
    // with its two halves swapped and no number of toggles ever unifies it.
    let comment_all = commentable.iter().any(|r| r.token.is_none());

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
        // Removal stays at each row's own prefix rather than the shared column,
        // and takes the row's own token rather than the operation's. Helix
        // removes at the shared column, which eats indentation when the
        // commented rows are not equally indented, and removing what each row
        // actually has is what makes the round trip exact.
        let token_of = |row: &CommentRow| {
            row.token
                .expect("comment_all is false, so every row carries a token")
        };

        // The margin is one decision for the whole set, as Helix makes it. A row
        // whose token carries no space keeps every row's, so the block gives up
        // the same width throughout and commenting it again restores it. Per row
        // the spaced rows lose two characters and the rest lose one, which
        // flattens the column relationship between them permanently.
        let margin = usize::from(commentable.iter().all(|row| {
            matches!(
                rope.chars_at(row.content_start + token_of(row).len())
                    .next(),
                Some(' ')
            )
        }));

        for row in &commentable {
            let remove_end = row.content_start + token_of(row).len() + margin;
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

/// Wrap each row's text in `tokens`, or unwrap the rows already wrapped.
///
/// One comment per row rather than one around the whole set, so a block-only
/// language toggles the same shape a line-comment language does and a row is
/// still legible as its own commented line.
fn apply_block_comments(
    ws: &mut Workspace,
    editor_id: EditorId,
    buffer_id: crate::buffer::BufferId,
    commentable: &[CommentRow],
    tokens: (&'static str, &'static str),
) -> UpdateEffect {
    let edits: Vec<(Range<usize>, String)> = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let rope = snapshot.buffer_snapshot().rope();
        commentable
            .iter()
            .flat_map(|row| {
                stoat_text::toggle_block_comment(rope, row.content_start..row.line_end, tokens)
            })
            .collect()
    };
    if edits.is_empty() {
        return UpdateEffect::None;
    }

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = edits
            .iter()
            .rev()
            .map(|(range, replacement)| (range.clone(), replacement.as_str()))
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

        // Contiguous runs, so one walk reads each. A walk carries one cursor
        // across consecutive rows, so a single walk over two selections far
        // apart steps through every row between them.
        let mut runs: Vec<Range<u32>> = Vec::new();
        for row in rows {
            match runs.last_mut() {
                Some(run) if run.end == row => run.end = row + 1,
                _ => runs.push(row..row + 1),
            }
        }

        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        let mut line = String::new();
        for run in runs {
            // One descent for the run's head. Every row after it follows from
            // the length the walk reports plus the newline it stepped over.
            let mut line_start = rope.point_to_offset(Point::new(run.start, 0));
            let mut walk = rope.line_walk(run);
            loop {
                line.clear();
                let Some(len) = walk.next_into(&mut line) else {
                    break;
                };
                match dir {
                    IndentDir::In => {
                        // Indenting leaves all-whitespace rows untouched, like
                        // Helix.
                        if let Some(leading) = leading_whitespace_chars(&line) {
                            edits.push((
                                line_start,
                                line_start,
                                indent_text(style, count, leading),
                            ));
                        }
                    },
                    IndentDir::Out => {
                        // Remove leading whitespace up to `count` indent widths
                        // of visual columns, counting a tab to its next stop.
                        let target = count.saturating_mul(style.indent_width(TAB_WIDTH));
                        let mut width = 0usize;
                        let mut consumed = 0usize;
                        for ch in line.chars() {
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
                line_start += len as usize + 1;
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

/// Text one `>` inserts at a line start, given the leading whitespace already
/// there.
///
/// A space style tops the line up to the next indent stop rather than adding a
/// whole unit, so a line three spaces into a four-space style gains one space
/// and every later press gains four. Tab stops belong to the renderer and no
/// partial tab exists to insert, so a tab style always inserts whole units.
fn indent_text(style: IndentStyle, count: usize, leading: usize) -> String {
    let unit = style.as_str();
    match style {
        IndentStyle::Tabs => unit.repeat(count),
        // The width comes from the string actually inserted, which is clamped
        // to at least one character, so the remainder always has a divisor.
        IndentStyle::Spaces(_) => " ".repeat(unit.len() * count - leading % unit.len()),
    }
}

/// Count of whitespace characters a line opens with, or `None` when the line
/// holds nothing else.
///
/// The count is in characters rather than bytes so it lines up with an indent
/// width measured in space characters. A leading tab therefore counts as one,
/// whatever column it advances to.
fn leading_whitespace_chars(line: &str) -> Option<usize> {
    line.chars().position(|ch| !ch.is_whitespace())
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
        .set_single_range(start_anchor, end_anchor, false, SelectionGoal::None);
    UpdateEffect::Redraw
}

pub(super) fn extend_line_below(stoat: &mut Stoat) -> UpdateEffect {
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

    let primary_id = editor.selections.newest_anchor().id;
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
            // The trim moves the cursor horizontally, so the column a later
            // vertical motion aims for is the trimmed one rather than the one
            // held before it.
            new.goal = SelectionGoal::None;
            Some(new)
        })
        .collect();

    if trimmed.is_empty() {
        editor.selections.transform(buffer_snapshot, |sel| {
            let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
            let tail_offset = buffer_snapshot.resolve_anchor(&sel.tail());
            let cursor = cursor_offset(rope, tail_offset, head_offset);
            land_block_cursor(sel.id, cursor, SelectionGoal::None, rope, buffer_snapshot)
        });
        editor.selections.keep_primary();
    } else {
        // A survivor of the primary keeps the primary by keeping its id, which
        // is still the highest. Where the primary was all whitespace it has no
        // survivor, and the primary falls to the document-last one rather than
        // to whichever id happens to top the rest.
        let promote = match trimmed.iter().any(|sel| sel.id == primary_id) {
            true => None,
            false => trimmed.last().map(|sel| sel.id),
        };
        editor.selections.replace_with(trimmed, buffer_snapshot);
        if let Some(id) = promote {
            editor.selections.make_primary(id);
        }
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
pub(crate) enum ChangeDir {
    Next,
    Prev,
}

pub(super) fn expand_selection(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    expand_selection_impl(stoat, count)
}

/// [`expand_selection`] with its count supplied rather than read from the
/// pending keypress, so a replay repeats the count the motion was made with.
pub(crate) fn expand_selection_impl(stoat: &mut Stoat, count: u32) -> UpdateEffect {
    stoat.last_motion = Some(LastMotion::ExpandSelection { count });
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

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
        return UpdateEffect::None;
    };
    let snapshot = syntax_map.snapshot();

    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let before = editor.selections.shared_anchors();
    let current_ranges = resolved_ranges(&before, buffer_snapshot);
    if editor.expansion_tip.as_deref() != Some(current_ranges.as_slice()) {
        editor.expansion_history.clear();
    }

    let mut grew = false;
    editor
        .selections
        .transform_resolved(buffer_snapshot, |sel, head_offset, tail_offset| {
            let (from, to) = (head_offset.min(tail_offset), head_offset.max(tail_offset));
            let Some(target) = enclosing_node_range(snapshot, from, to) else {
                return sel.clone();
            };
            grew = true;
            // The direction the selection already had survives the expansion,
            // so a reversed one stays reversed however far out it grows.
            Selection {
                id: sel.id,
                start: buffer_snapshot.anchor_at(target.start, Bias::Right),
                end: buffer_snapshot.anchor_at(target.end, Bias::Left),
                reversed: sel.reversed,
                goal: SelectionGoal::None,
            }
        });

    if !grew {
        return UpdateEffect::None;
    }
    editor.expansion_history.push(before);
    let after = editor.selections.shared_anchors();
    editor.expansion_tip = Some(resolved_ranges(&after, buffer_snapshot));
    UpdateEffect::Redraw
}

/// The byte range a selection spanning `from..to` expands to, which is the
/// node covering it or that node's parent when the two already agree.
fn enclosing_node_range(
    snapshot: &stoat_language::SyntaxSnapshot,
    from: usize,
    to: usize,
) -> Option<Range<usize>> {
    let layer = deepest_containing_layer(snapshot, from, to)?;
    let node = layer.tree.root_node().descendant_for_byte_range(from, to)?;
    let node_range = node.byte_range();
    if node_range.start == from && node_range.end == to {
        Some(node.parent()?.byte_range())
    } else {
        Some(node_range)
    }
}

/// Each selection's byte range, in the collection's order.
fn resolved_ranges(
    selections: &[Selection<Anchor>],
    snapshot: &MultiBufferSnapshot,
) -> Vec<Range<usize>> {
    selections
        .iter()
        .map(|sel| {
            let start = snapshot.resolve_anchor(&sel.start);
            let end = snapshot.resolve_anchor(&sel.end);
            start..end
        })
        .collect()
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
    shrink_selection_impl(stoat, count)
}

/// [`shrink_selection`] with its count supplied rather than read from the
/// pending keypress, so a replay repeats the count the motion was made with.
pub(crate) fn shrink_selection_impl(stoat: &mut Stoat, count: u32) -> UpdateEffect {
    stoat.last_motion = Some(LastMotion::ShrinkSelection { count });
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let syntax = ws.buffers.syntax_map(buffer_id).map(|sm| sm.snapshot());
    let editor = ws.editors.get_mut(editor_id).expect("editor");

    let mut changed = false;
    for _ in 0..count {
        if !shrink_one_step(editor, syntax) {
            break;
        }
        changed = true;
    }
    if !changed {
        return UpdateEffect::None;
    }

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let after = editor.selections.shared_anchors();
    editor.expansion_tip = Some(resolved_ranges(&after, buffer_snapshot));
    UpdateEffect::Redraw
}

/// Take one step inward, returning whether the selection moved.
///
/// The history's top is restored only when the selection still contains it.
/// Anything else means the user moved away since the expansion, which makes
/// the whole stack stale rather than only its top, so it is dropped.
///
/// With nothing to restore, each range descends to the first named child of
/// the node it covers. That is what makes the key useful with no expansion
/// behind it at all.
fn shrink_one_step(
    editor: &mut EditorState,
    syntax: Option<&stoat_language::SyntaxSnapshot>,
) -> bool {
    if let Some(prev) = editor.expansion_history.pop() {
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let current = resolved_ranges(&editor.selections.shared_anchors(), buffer_snapshot);
        let prev_ranges = resolved_ranges(&prev, buffer_snapshot);

        if prev_ranges
            .iter()
            .all(|p| current.iter().any(|c| c.start <= p.start && c.end >= p.end))
        {
            editor.selections.restore(prev);
            return true;
        }
        editor.expansion_history.clear();
    }

    let Some(syntax) = syntax else {
        return false;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let mut dived = false;
    editor
        .selections
        .transform_resolved(buffer_snapshot, |sel, head_offset, tail_offset| {
            let (from, to) = (head_offset.min(tail_offset), head_offset.max(tail_offset));
            let Some(target) = first_named_child_range(syntax, from, to) else {
                return sel.clone();
            };
            dived = true;
            Selection {
                id: sel.id,
                start: buffer_snapshot.anchor_at(target.start, Bias::Right),
                end: buffer_snapshot.anchor_at(target.end, Bias::Left),
                reversed: sel.reversed,
                goal: SelectionGoal::None,
            }
        });
    dived
}

/// Byte range of the first named child of the node covering `from..to`.
fn first_named_child_range(
    snapshot: &stoat_language::SyntaxSnapshot,
    from: usize,
    to: usize,
) -> Option<Range<usize>> {
    let layer = deepest_containing_layer(snapshot, from, to)?;
    let node = layer.tree.root_node().descendant_for_byte_range(from, to)?;
    let child = node.named_child(0)?;
    let range = child.byte_range();
    (range.start != from || range.end != to).then_some(range)
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum SiblingDir {
    Next,
    Prev,
}

pub(super) fn select_sibling(stoat: &mut Stoat, dir: SiblingDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    select_sibling_impl(stoat, dir, count)
}

/// [`select_sibling`] with its count supplied rather than read from the pending
/// keypress, so a replay repeats the count the motion was made with.
pub(crate) fn select_sibling_impl(stoat: &mut Stoat, dir: SiblingDir, count: u32) -> UpdateEffect {
    stoat.last_motion = Some(LastMotion::TsSibling { dir, count });
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
        return UpdateEffect::None;
    };
    let snapshot = syntax_map.snapshot();

    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let mut moved = false;
    editor
        .selections
        .transform_resolved(buffer_snapshot, |sel, head_offset, tail_offset| {
            let (from, to) = (head_offset.min(tail_offset), head_offset.max(tail_offset));
            let Some(target) = sibling_range(snapshot, from, to, dir, count) else {
                return sel.clone();
            };
            moved = true;
            // The walk sets the direction rather than the source range. A step
            // forward leaves its cursor at the sibling's end and a step back
            // leaves it at the start, so a repeat carries on the way it went.
            Selection {
                id: sel.id,
                start: buffer_snapshot.anchor_at(target.start, Bias::Right),
                end: buffer_snapshot.anchor_at(target.end, Bias::Left),
                reversed: matches!(dir, SiblingDir::Prev),
                goal: SelectionGoal::None,
            }
        });

    if !moved {
        return UpdateEffect::None;
    }
    UpdateEffect::Redraw
}

/// Byte range of the sibling `count` steps from the node covering `from..to`,
/// or `None` when the walk takes no step at all.
fn sibling_range(
    snapshot: &stoat_language::SyntaxSnapshot,
    from: usize,
    to: usize,
    dir: SiblingDir,
    count: u32,
) -> Option<Range<usize>> {
    let layer = deepest_containing_layer(snapshot, from, to)?;
    let mut current = layer.tree.root_node().descendant_for_byte_range(from, to)?;

    let mut moved = false;
    for _ in 0..count {
        match sibling_of_self_or_ancestor(current, dir) {
            Some(s) => {
                current = s;
                moved = true;
            },
            None => break,
        }
    }
    moved.then(|| current.byte_range())
}

/// The next named sibling of `node`, or of the nearest ancestor that has one.
///
/// A node at the end of its parent's children has nowhere to go on its own,
/// but the construct enclosing it usually does. Climbing is what carries the
/// walk out of a nested block and on through the file rather than stopping at
/// the block's edge.
///
/// `None` once the climb reaches the tree's root, where there is no enclosing
/// construct left to ask.
fn sibling_of_self_or_ancestor<'t>(
    node: stoat_language::Node<'t>,
    dir: SiblingDir,
) -> Option<stoat_language::Node<'t>> {
    let mut current = node;
    loop {
        let sibling = match dir {
            SiblingDir::Next => current.next_named_sibling(),
            SiblingDir::Prev => current.prev_named_sibling(),
        };
        if let Some(sibling) = sibling {
            return Some(sibling);
        }
        current = current.parent()?;
    }
}

pub(crate) fn select_all_siblings(stoat: &mut Stoat) -> UpdateEffect {
    stoat.last_motion = Some(LastMotion::SelectAllSiblings);
    fan_selections_to_children(stoat, true)
}

pub(crate) fn select_all_children(stoat: &mut Stoat) -> UpdateEffect {
    stoat.last_motion = Some(LastMotion::SelectAllChildren);
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
    let count = stoat.take_pending_count().unwrap_or(1).max(1);
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;

    // The snapshot outlives the editor borrow, so the rope stays readable while
    // the syntax map answers for each selection in turn.
    let (display_snapshot, reads) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let display_snapshot = editor.display_map.snapshot();
        let reads = editor
            .selections
            .resolved_reads(display_snapshot.buffer_snapshot());
        (display_snapshot, reads)
    };
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    // Every cursor walks to the bound of the node it sits in. Resolving one
    // node and landing every cursor on it makes every span identical, and
    // identical spans merge, so the set collapses to one cursor.
    let mut targets: Vec<(usize, usize)> = {
        let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
            return UpdateEffect::None;
        };
        let snapshot = syntax_map.snapshot();
        reads
            .iter()
            .filter_map(|read| {
                let cursor = cursor_offset(rope, read.tail, read.head);
                let start = read.head.min(read.tail);
                let end = read.head.max(read.tail);
                let landing = walk_to_node_bound(snapshot, start, end, cursor, bound, count)?;
                Some((read.id, landing))
            })
            .collect()
    };
    targets.sort_unstable_by_key(|(id, _)| *id);

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    // Landing on a node's bound moves horizontally, so it drops any column a
    // prior vertical move was holding, for the same reason as the sibling
    // motion above.
    move_cursors(&mut editor.selections, buffer_snapshot, extend, |read| {
        let found = targets.binary_search_by_key(&read.id, |(id, _)| *id).ok()?;
        Some((targets[found].1, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

/// The offset `count` node-bound steps from the selection covering
/// `start..end`, whose block cursor covers the cell at `cursor`.
///
/// One step takes the innermost named node covering the selection and answers
/// with the bound `bound` names. Going forward that is the cell just past the
/// node, so a run of steps walks out through the enclosing constructs one at a
/// time. Going backward it is the node's own first cell, except where the
/// cursor already stands there and the step climbs instead, since a node offers
/// nowhere further back to go.
///
/// Each step feeds its landing back as the next collapsed cursor, so a count
/// repeats the motion rather than widening any single step's reach.
fn walk_to_node_bound(
    snapshot: &stoat_language::SyntaxSnapshot,
    start: usize,
    end: usize,
    cursor: usize,
    bound: NodeBound,
    count: u32,
) -> Option<usize> {
    let (mut start, mut end, mut cursor) = (start, end, cursor);
    let mut landing = None;

    for _ in 0..count {
        let layer = deepest_containing_layer(snapshot, start, end)?;
        let node = layer
            .tree
            .root_node()
            .named_descendant_for_byte_range(start, end)?;
        let next = match bound {
            NodeBound::End => node.end_byte(),
            NodeBound::Start if node.start_byte() == cursor => {
                find_parent_start(node).unwrap_or(node).start_byte()
            },
            NodeBound::Start => node.start_byte(),
        };

        landing = Some(next);
        (start, end, cursor) = (next, next, next);
    }

    landing
}

/// The nearest named ancestor of `node` that starts before `node` does.
///
/// An ancestor sharing the same start byte answers the same offset, which is no
/// move at all, so the walk passes those by along with the unnamed nodes a
/// grammar fills in between them.
fn find_parent_start(node: stoat_language::Node<'_>) -> Option<stoat_language::Node<'_>> {
    let start = node.start_byte();
    let mut node = node;
    while node.start_byte() >= start || !node.is_named() {
        node = node.parent()?;
    }
    Some(node)
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

pub(crate) fn execute_find(
    stoat: &mut Stoat,
    kind: FindKind,
    ch: char,
    extend: bool,
    count: u32,
) -> UpdateEffect {
    let count = count.max(1);
    stoat.last_motion = Some(LastMotion::Find {
        kind,
        ch,
        count,
        extend,
    });
    move_to_motion_range(stoat, extend, |rope, cursor| {
        let target = find_target(rope, cursor, kind, ch, count)?;
        Some(landing_range(rope, cursor, target))
    })
}

/// Run a find whose target is the end of a line rather than a character.
///
/// Backs `f`/`t`/`F`/`T` followed by Enter. A line ending is not a character
/// the caller names in advance, since one line ends in LF where the next ends
/// in CRLF, so the target comes from the cursor's row instead of a scan.
pub(crate) fn execute_find_line_ending(
    stoat: &mut Stoat,
    kind: FindKind,
    extend: bool,
    count: u32,
) -> UpdateEffect {
    let count = count.max(1);
    stoat.last_motion = Some(LastMotion::FindLineEnding {
        kind,
        count,
        extend,
    });
    move_to_motion_range(stoat, extend, |rope, cursor| {
        let target = line_ending_target(rope, cursor, kind, count)?;
        Some(landing_range(rope, cursor, target))
    })
}

/// The `(anchor, head)` pair that lands a block cursor on `target`.
///
/// A find names the character it means to land on, where a paragraph motion
/// names the gap it stops at. This converts the first into the second, reaching
/// one grapheme past whichever end the cursor is not on, so the target's own
/// cell stays covered whichever way the motion ran.
fn landing_range(rope: &Rope, cursor: usize, target: usize) -> (usize, usize) {
    if target >= cursor {
        (cursor, rope.next_grapheme_boundary(target))
    } else {
        (rope.next_grapheme_boundary(cursor), target)
    }
}

/// The `(start, end, reversed)` span an extend produces when its motion lands
/// on `target`.
///
/// For the motions that land on a whole span rather than a single cell, where
/// [`cursor_offset`] answers for the rest. The anchor stays put and the head
/// reaches whichever end of the target lies further from it, so a step back
/// grows the selection rather than turning it inside out.
///
/// The anchor is the selection's tail, never the lesser of its two ends. A
/// reversed selection anchors on its right end, so extending it forward
/// releases everything to the left of that.
pub(super) fn extend_span(
    rope: &Rope,
    anchor: usize,
    target: &Range<usize>,
) -> (usize, usize, bool) {
    if target.end < anchor {
        (target.start, anchor, true)
    } else if target.end == anchor {
        // An empty span belongs to the rope's end alone, so widen this one.
        (anchor, rope.next_grapheme_boundary(anchor), false)
    } else {
        (anchor, target.end, false)
    }
}

/// Move each selection to the `(anchor, head)` pair `range_of` picks for it.
///
/// The finds and the paragraph motions share this, so they differ only in where
/// they look. A selection `range_of` returns `None` for holds its place rather
/// than being dragged onto another selection's target.
///
/// Extending ignores the anchor and holds the one the selection already has,
/// which is what makes a run of these grow one selection rather than restart it.
fn move_to_motion_range(
    stoat: &mut Stoat,
    extend: bool,
    range_of: impl Fn(&Rope, usize) -> Option<(usize, usize)>,
) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    if extend {
        // These are horizontal, so they drop any column a prior vertical move
        // held. A carried column sends the next vertical move back to where this
        // one started from.
        move_cursors(&mut editor.selections, buffer_snapshot, true, |read| {
            // Each selection reads from its own cursor. Reading once and
            // stamping the result on all of them makes every span identical, and
            // identical spans merge, so the set collapses to one cursor.
            let cursor = cursor_offset(rope, read.tail, read.head);
            // The cell comes back out against the anchor the motion paired its
            // head with, never the live selection's. The two sit on opposite
            // sides of the head once the selection has grown past the target,
            // and reading from the wrong side gives up the target's own cell.
            let (anchor, head) = range_of(rope, cursor)?;
            Some((cursor_offset(rope, anchor, head), SelectionGoal::None))
        });
        return UpdateEffect::Redraw;
    }

    editor
        .selections
        .transform_resolved(buffer_snapshot, |sel, head_offset, tail_offset| {
            let cursor = cursor_offset(rope, tail_offset, head_offset);
            // Each selection reads from its own cursor. Reading once and
            // stamping the result on all of them makes every span identical,
            // and identical spans merge, so the set collapses to one cursor.
            let Some((anchor, head)) = range_of(rope, cursor) else {
                return sel.clone();
            };

            // Span the pair rather than collapsing onto the head, so `dfx`/`yfx`
            // operate on the whole reach like Helix.
            let landed = Selection {
                id: sel.id,
                start: anchor.min(head),
                end: anchor.max(head),
                reversed: head < anchor,
                goal: SelectionGoal::None,
            }
            .min_width_1(rope);
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

/// Offset the block cursor lands on for a find or till motion aimed at a line
/// ending, or `None` when the target line falls outside the buffer.
///
/// The target is a row away from the cursor's own, so a count crosses that many
/// lines. A cursor already sitting on the edge it is aimed at counts as one line
/// consumed, which is what lets the same keys run again and advance rather than
/// hold still.
///
/// The last row is unreachable going forward. It carries no ending, since a
/// buffer that ends in one has an empty row after it.
fn line_ending_target(rope: &Rope, cursor: usize, kind: FindKind, count: u32) -> Option<usize> {
    let row = rope.offset_to_point(cursor).row;
    let max_row = rope.max_point().row;
    let count = count as i64;

    match kind {
        FindKind::NextChar | FindKind::TillNextChar => {
            let this_end = line_end_offset(rope, row, max_row);
            let on_edge = match kind {
                FindKind::TillNextChar => {
                    this_end == cursor || this_end == rope.next_grapheme_boundary(cursor)
                },
                _ => this_end == cursor,
            };

            let target_row = row as i64 + count - 1 + i64::from(on_edge);
            if target_row >= i64::from(max_row) {
                return None;
            }

            let end = line_end_offset(rope, target_row as u32, max_row);
            match kind {
                FindKind::TillNextChar => Some(rope.prev_grapheme_boundary(end)),
                _ => Some(end),
            }
        },
        FindKind::PrevChar => {
            let target_row = row as i64 - count;
            if target_row < 0 {
                return None;
            }
            Some(line_end_offset(rope, target_row as u32, max_row))
        },
        FindKind::TillPrevChar => {
            let on_edge = rope.point_to_offset(Point::new(row, 0)) == cursor;
            let target_row = row as i64 - count + 1 - i64::from(on_edge);
            if target_row <= 0 {
                return None;
            }
            Some(rope.point_to_offset(Point::new(target_row as u32, 0)))
        },
    }
}

/// Offset of `row`'s last character, before the line ending that follows it.
///
/// The final row has no ending to sit before, so it reports the buffer's end.
fn line_end_offset(rope: &Rope, row: u32, max_row: u32) -> usize {
    if row >= max_row {
        rope.len()
    } else {
        rope.point_to_offset(Point::new(row + 1, 0))
            .saturating_sub(1)
    }
}

/// Offset the block cursor lands on for a find or till motion starting at
/// `cursor`, or `None` when `ch` does not occur in the rest of the buffer.
///
/// The scan runs to the buffer's edge rather than the line's, so `f` and its
/// siblings reach a target on any later line. A newline is an ordinary
/// character to the scan, and matches `ch` when `ch` is one.
fn find_target(rope: &Rope, cursor: usize, kind: FindKind, ch: char, count: u32) -> Option<usize> {
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

pub(super) fn goto_change(stoat: &mut Stoat, dir: ChangeDir) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1).max(1);
    goto_change_impl(stoat, dir, count)
}

/// [`goto_change`] with its count supplied rather than read from the pending
/// keypress, so a replay repeats the count the motion was made with.
pub(crate) fn goto_change_impl(stoat: &mut Stoat, dir: ChangeDir, count: u32) -> UpdateEffect {
    stoat.last_motion = Some(LastMotion::Change { dir, count });

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

    let count = count as usize;
    let extend = stoat.in_select_mode();
    let center_off = stoat.settings.jump_scrolloff.unwrap_or(0);
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
    let rope = buffer_snapshot.rope();

    // One walk over them feeds every selection, since each one picks its own
    // target out of the same sorted list.
    let hunk_rows = live_hunk_rows(&display_snapshot, buffer_snapshot);

    // Every selection steps from its own cursor, so a multi-cursor set walks to
    // one hunk each rather than sharing whichever cursor happened to be newest.
    let landings: Vec<(usize, Range<usize>, Range<u32>)> = editor
        .selections
        .all_anchors()
        .iter()
        .filter_map(|sel| {
            let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
            let head_off = buffer_snapshot.resolve_anchor(&sel.head());
            let cursor_row = rope
                .offset_to_point(cursor_offset(rope, tail_off, head_off))
                .row;
            let rows = nth_hunk_rows(&hunk_rows, cursor_row, dir, count)?;
            Some((sel.id, hunk_span(rope, rows.clone()), rows))
        })
        .collect();

    if landings.is_empty() {
        return goto_change_across_files(stoat, dir, current_path, source_diff_view, origin);
    }

    editor
        .selections
        .transform_resolved(buffer_snapshot, |sel, _head_offset, tail_offset| {
            let Some((_, target, _)) = landings.iter().find(|(id, ..)| *id == sel.id) else {
                return sel.clone();
            };

            let (start, end, reversed) = if extend {
                extend_span(rope, tail_offset, target)
            } else {
                // One landing point per hunk, whichever way the walk arrived:
                // the selection covers the stop and the cursor sits on its
                // first row. A hunk that presented two faces made a reversal
                // land the same hunk again rather than step to its neighbor.
                //
                // The repeat still steps out from that row. `Next` keeps stops
                // starting past the cursor, and a stop's own start never is,
                // while `Prev` keeps stops ending at or before it, which a
                // non-empty stop's own end never is.
                (target.start, target.end, true)
            };

            Selection {
                id: sel.id,
                start: buffer_snapshot.anchor_at(start, Bias::Right),
                end: buffer_snapshot.anchor_at(end, Bias::Left),
                reversed,
                goal: SelectionGoal::None,
            }
        });
    if let Some(entry) = origin {
        super::jump::push_entry(stoat, entry);
    }
    // The landing goes to the middle of the screen rather than the edge the
    // walk arrived at. The key epilogue's follow then finds the cursor deep
    // inside its margin and leaves both the view and this glide alone.
    //
    // The whole chunk takes the middle, not the row the cursor sits on, so a
    // tall hunk does not hang off the bottom edge with its landing centered.
    if let Some(editor) = focused_editor_mut(stoat) {
        let down = matches!(dir, ChangeDir::Next);
        let span = landed_display_span(editor, &landings);
        match span {
            Some(span) => view::center_jump_on_span(editor, span, down, center_off),
            None => view::center_jump_on_cursor(editor, down, center_off),
        };
    }
    UpdateEffect::Redraw
}

/// Display rows the newest selection's landed stop covers, exclusive end.
///
/// `None` where the newest selection took no landing, which leaves the caller
/// on the plain cursor centering.
fn landed_display_span(
    editor: &mut EditorState,
    landings: &[(usize, Range<usize>, Range<u32>)],
) -> Option<Range<u32>> {
    let newest_id = editor.selections.newest_anchor().id;
    let rows = landings
        .iter()
        .find(|(id, ..)| *id == newest_id)
        .map(|(_, _, rows)| rows.clone())?;

    Some(stop_display_span(&editor.display_map.snapshot(), &rows))
}

/// Display rows a stop's buffer `rows` cover, exclusive end.
///
/// A deletion stop holds no buffer rows of its own, so it brackets its seam.
/// The span runs from the row it landed on through the row after the seam. The
/// diff view's spliced block sits between those two buffer rows, so the mapped
/// span covers the removed lines without this reading the block structure.
fn stop_display_span(snapshot: &DisplaySnapshot, rows: &Range<u32>) -> Range<u32> {
    let display_row = |row: u32| snapshot.buffer_to_display(Point::new(row, 0)).row;
    match rows.is_empty() {
        true => display_row(rows.start.saturating_sub(1))..display_row(rows.start) + 1,
        false => display_row(rows.start)..display_row(rows.end),
    }
}

/// Buffer rows of the hunk `count` steps from `cursor_row`, or `None` when the
/// walk runs out of hunks before its first step.
///
/// The backward walk compares each hunk's end against the cursor rather than
/// its start, which is what steps out of the hunk the cursor is already inside
/// instead of landing on that one again.
///
/// A deletion hunk stores no rows, and [`hunk_span`] lands it on the row above
/// its seam, so both arms read that row as the one it occupies. A step forward
/// off a removal the cursor already sits on passes over it, and a step back
/// from the seam row lands on it.
fn nth_hunk_rows(
    hunk_rows: &[Range<u32>],
    cursor_row: u32,
    dir: ChangeDir,
    count: usize,
) -> Option<Range<u32>> {
    match dir {
        ChangeDir::Next => {
            let next: Vec<_> = hunk_rows
                .iter()
                .filter(|r| match r.is_empty() {
                    true => r.start.saturating_sub(1) > cursor_row,
                    false => r.start > cursor_row,
                })
                .collect();
            let idx = (count.saturating_sub(1)).min(next.len().checked_sub(1)?);
            Some(next[idx].clone())
        },
        ChangeDir::Prev => {
            let prev: Vec<_> = hunk_rows
                .iter()
                .filter(|r| match r.is_empty() {
                    true => r.start.saturating_sub(1) < cursor_row,
                    false => r.end <= cursor_row,
                })
                .collect();
            let idx = prev.len().checked_sub(count.max(1))?.min(prev.len() - 1);
            Some(prev[idx].clone())
        },
    }
}

/// Row ranges change navigation stops on, in document order.
///
/// Where the changes sit now, not where the last diff job left them, so a jump
/// reaches the row the gutter paints its mark on. A refined hunk offers one
/// stop per marked run, so a walk crosses the rows that changed rather than
/// the block that holds them. A buffer with no diff map at all answers empty.
fn live_hunk_rows(
    display_snapshot: &DisplaySnapshot,
    buffer_snapshot: &MultiBufferSnapshot,
) -> Vec<Range<u32>> {
    display_snapshot
        .diff_map()
        .map(|diff_map| diff_map.live_hunks(buffer_snapshot).change_stops())
        .unwrap_or_default()
}

pub(super) fn goto_first_change(stoat: &mut Stoat) -> UpdateEffect {
    goto_edge_change(stoat, false)
}

pub(super) fn goto_last_change(stoat: &mut Stoat) -> UpdateEffect {
    goto_edge_change(stoat, true)
}

/// Select the buffer's first or last change hunk.
///
/// Nothing at all happens where there is no hunk, the origin included, so a
/// press in an unchanged buffer leaves the jumplist as it was.
///
/// This shares only the hunk collection with [`goto_change_impl`], not its
/// walk. The per-selection stepping, the extend branch, the repeat record and
/// the cross-file hop all answer questions an end of the list does not ask.
fn goto_edge_change(stoat: &mut Stoat, last: bool) -> UpdateEffect {
    let target = {
        let Some(editor) = focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let rows = live_hunk_rows(&display_snapshot, buffer_snapshot);
        let rows = match last {
            true => rows.last().cloned(),
            false => rows.first().cloned(),
        };
        rows.map(|rows| hunk_span(buffer_snapshot.rope(), rows))
    };
    let Some(target) = target else {
        return UpdateEffect::None;
    };

    super::jump::push_jump(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    editor.selections.set_single_range(
        buffer_snapshot.anchor_at(target.start, Bias::Left),
        buffer_snapshot.anchor_at(target.end, Bias::Right),
        // The same one landing point the stepping walk lands: the cursor on the
        // stop's first row, so an edge jump and a step onto the same hunk leave
        // the reader in the same place.
        true,
        SelectionGoal::None,
    );
    UpdateEffect::Redraw
}

/// Byte span a stop's rows cover, from the first row through the start of the
/// row after the last.
///
/// A deletion stop holds no rows, so it gets one cell on the row above its
/// seam. The block cursor has no place to sit in an empty span, and the diff
/// view splices the deleted block above the seam, so the row above is the last
/// line the removed text followed. A removal at the top of the file saturates
/// to row 0.
fn hunk_span(rope: &Rope, rows: Range<u32>) -> Range<usize> {
    let first_row = match rows.is_empty() {
        true => rows.start.saturating_sub(1),
        false => rows.start,
    };
    let start = rope.point_to_offset(Point::new(first_row, 0));
    let end = if rows.is_empty() {
        rope.next_grapheme_boundary(start)
    } else {
        rope.point_to_offset(Point::new(rows.end, 0))
    };
    start..end
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
    /// Which way the hop went, which the landing needs to bias its centering
    /// toward the file the reader came from. The scan lands long after the
    /// keypress that knew this.
    dir: ChangeDir,
}

/// What a changed-file scan decided.
enum ChangedFileJump {
    /// Open `path` and land on `line`.
    ///
    /// A hop always lands on a change, so the row is unconditional: the scan
    /// walks past every candidate that offers no landing row rather than
    /// opening one blind.
    ///
    /// `rows` is the buffer rows that hunk occupies, which the far side centers
    /// on. It rides the message because the target is not open when the scan
    /// picks it, so nothing after the open knows which chunk was chosen.
    To {
        path: PathBuf,
        line: u32,
        rows: Range<u32>,
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
    let base = stoat.active_workspace().diff_base().cloned();
    // The hop reads the tally the workspace already holds rather than walking
    // the repository again, which is what every `n` across a file used to cost.
    let tally = stoat
        .active_workspace()
        .repo_hunk_totals()
        .map(<[_]>::to_vec);
    let redraw = stoat.redraw_notify.clone();
    let (tx, rx) = mpsc::channel();

    let task = stoat.executor.spawn_blocking(move || {
        let found = scan_changed_file_jump(
            &git_host,
            &fs_host,
            &git_root,
            current_path,
            dir,
            base.as_ref(),
            tally,
        );
        let _ = tx.send(found);
        redraw.notify_one();
    });

    stoat.pending_changed_file_jump = Some(PendingChangedFileJump {
        rx,
        _task: task,
        source_diff_view,
        origin,
        dir,
    });
    UpdateEffect::Redraw
}

/// Whether `a` and `b` name the same file, each resolved through the
/// filesystem before the comparison.
///
/// The changed list holds libgit2's `workdir.join(rel)` while a buffer holds
/// the path it was opened with, so a symlinked component or any other spelling
/// of the same file makes a raw comparison miss. A path that will not resolve
/// falls back to itself, which is the raw comparison again.
fn same_file(fs_host: &Arc<dyn FsHost>, a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| {
        fs_host
            .canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
    };
    resolve(a) == resolve(b)
}

/// Absolute paths of the working tree's changed files that own at least one
/// hunk, which is what narrows a hop list to files that hold a row to land on.
///
/// Tests the count rather than presence in the tally. The two backends spell a
/// changeless file differently. The local one leaves it out of `per_file`
/// entirely, while the fake registers it at zero.
///
/// Tally paths are repo-relative and hop-list paths are absolute, so they are
/// joined onto the same workdir `changed_files` used. `git_root` stands in for
/// a repo with no workdir, which lists no changed files anyway.
///
/// `stored` is the workspace's last tally, which is what this reads when it has
/// one. A fresh tally is a walk of every diff in the repository, and a hop
/// tolerates a beat of staleness exactly as the status bar does. `None` means
/// no tally has landed yet, and only then is one worth running here.
fn files_with_hunks(
    repo: &dyn GitRepo,
    git_root: &Path,
    stored: Option<Vec<(PathBuf, usize)>>,
) -> std::collections::HashSet<PathBuf> {
    let workdir = repo.workdir().unwrap_or_else(|| git_root.to_path_buf());
    let per_file = match stored {
        Some(per_file) => per_file,
        None => repo.hunk_tallies().per_file,
    };
    per_file
        .into_iter()
        .filter(|(_, hunks)| *hunks > 0)
        .map(|(rel, _)| workdir.join(rel))
        .collect()
}

/// Pick the changed file `dir` leads to from `current_path`, and the row in it
/// to land on.
///
/// The walk passes over any file that offers no row to land on -- a staged
/// addition with no base blob, a file that reads as non-UTF8 -- and keeps
/// going to the next one. An open of such a file puts the cursor at row 0
/// with nothing under it, and from there the in-file walk finds no hunk and
/// crosses out again on the very next press.
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
    base: Option<&DiffBase>,
    tally: Option<Vec<(PathBuf, usize)>>,
) -> ChangedFileJump {
    let Some(repo) = git_host.discover(git_root) else {
        return ChangedFileJump::NoMoreChanges;
    };
    // The hop walks the list the reader is looking at. Under a revision base
    // that is what has happened since that commit, not what is uncommitted --
    // otherwise a file whose hunks are on screen cannot be reached by pressing
    // `n`. A memory base supplies its text rather than a commit, so there is no
    // revision to list against and the working tree's own list is the closest
    // true answer.
    let listed = match base {
        Some(DiffBase::Rev { sha: Some(sha) }) => repo.changed_files_from(sha.as_str()),
        _ => {
            // A moved file lists as changed while owning no hunk, so a hop into
            // it lands on no row at all. The same holds for a mode-only change
            // and a binary delta.
            let hunky = files_with_hunks(&*repo, git_root, tally);
            repo.changed_files()
                .into_iter()
                .filter(|f| hunky.contains(&f.path))
                .collect()
        },
    };
    let mut changed: Vec<PathBuf> = listed
        .into_iter()
        .filter(|f| !f.untracked)
        .map(|f| f.path)
        .collect();
    // `changed_files` groups staged before unstaged, which is the shape a list
    // surface paints in two sections. The walk has no sections, so that order
    // leaves a mixed changeset visiting files with no reading of its own. Path
    // order is the one the status bar's per-file tally already counts through.
    changed.sort();

    let current_index = current_path
        .as_deref()
        .and_then(|path| changed.iter().position(|c| same_file(fs_host, c, path)));
    // Cross into a lone changed file when the current buffer is not itself in
    // the list, which the canonical comparison above decides. Only bail
    // when nothing changed, or the current buffer is the sole changed file and
    // there is nowhere else to go.
    if changed.is_empty() || (current_index.is_some() && changed.len() < 2) {
        return ChangedFileJump::NoMoreChanges;
    }

    // Every file the walk reaches before it comes back round, paired with
    // whether reaching it crossed the end of the list. A candidate that turns
    // out to hold no landing row is passed over, so the order has to run past
    // the adjacent file rather than stop at it.
    let candidates: Vec<(usize, bool)> = match (current_index, dir) {
        (Some(i), ChangeDir::Next) => (i + 1..changed.len())
            .map(|c| (c, false))
            .chain((0..i).map(|c| (c, true)))
            .collect(),
        (Some(i), ChangeDir::Prev) => (0..i)
            .rev()
            .map(|c| (c, false))
            .chain((i + 1..changed.len()).rev().map(|c| (c, true)))
            .collect(),
        (None, ChangeDir::Next) => (0..changed.len()).map(|c| (c, false)).collect(),
        (None, ChangeDir::Prev) => (0..changed.len()).rev().map(|c| (c, false)).collect(),
    };

    for (index, wrapped) in candidates {
        let path = &changed[index];
        // A hop exists to leave the file. Landing back on the one the reader
        // is already in restarts the walk at its first hunk, which reads as
        // the same hunks coming round again rather than as a wrap. Any
        // identity miss the resolve above did not close surfaces here as
        // the end of the walk.
        if current_path
            .as_deref()
            .is_some_and(|current| same_file(fs_host, path, current))
        {
            return ChangedFileJump::NoMoreChanges;
        }
        let Some((line, rows)) = first_hunk_stop(&*repo, fs_host, path, dir, base) else {
            continue;
        };
        return ChangedFileJump::To {
            path: path.clone(),
            line,
            rows,
            wrapped,
        };
    }

    ChangedFileJump::NoMoreChanges
}

/// The landing row of `path`'s first (Next) or last (Prev) hunk against the
/// workspace's diff base, paired with the rows that hunk occupies.
///
/// The base side mirrors the head side of `resolve_base`, so the row this
/// lands on is one of the hunks the target's own diff map will show. Reading
/// HEAD instead leaves a file committed on top of a review base looking
/// clean, and the hop drops the cursor at the file top with no change under
/// it.
///
/// The rows travel with the row because the hop's target is not open yet,
/// which leaves nothing on the far side to look the chunk up from. An empty
/// range is a deletion, which the display mapping brackets rather than spans.
///
/// `None` when the file has no base text, reads as non-UTF8, or diffs clean.
/// The scan passes such a file over rather than opening it with no row to
/// land on.
fn first_hunk_stop(
    repo: &dyn GitRepo,
    fs_host: &Arc<dyn FsHost>,
    path: &Path,
    dir: ChangeDir,
    base_override: Option<&DiffBase>,
) -> Option<(u32, Range<u32>)> {
    let base = match base_override {
        Some(DiffBase::Rev { sha: Some(sha) }) => repo.content_at(sha, path).unwrap_or_default(),
        Some(DiffBase::Rev { sha: None }) => String::new(),
        Some(DiffBase::Memory { files }) => match files.get(path) {
            Some(text) => text.to_string(),
            None => head_or_moved_content(repo, path)?,
        },
        None => head_or_moved_content(repo, path)?,
    };

    let mut bytes = Vec::new();
    let Ok(()) = fs_host.read(path, &mut bytes) else {
        // The file is gone from the working tree. The diff map answers a
        // whole-file removal as one hunk anchored at row 0, not as one
        // removal per item the differ finds inside it, so this answers the
        // same shape rather than diffing against empty text.
        return (!base.is_empty()).then_some((0, 0..0));
    };
    let working = String::from_utf8(bytes).ok()?;

    let result = structural_diff::diff(&base, &working);
    let hunks = diff_map::changes_to_hunks(&result.changes, &base, &working);
    // The file is not open, so there is no LiveHunks to read stops from. The
    // runs and buffer_start_line are both stored coordinates, which is the
    // space this answer travels in, so reading them here needs no shift.
    let stop_row = |hunk: &diff_map::DiffHunk, last: bool| {
        // A deletion covers no rows and lands on the one above its seam, which
        // is where the in-file walk puts one through hunk_span.
        if hunk.buffer_line_range.is_empty() {
            return hunk.buffer_start_line.saturating_sub(1);
        }
        match last {
            true => hunk
                .marked_rows
                .last()
                .map_or(hunk.buffer_start_line, |run| run.start),
            false => hunk
                .marked_rows
                .first()
                .map_or(hunk.buffer_start_line, |run| run.start),
        }
    };
    let (hunk, last) = match dir {
        ChangeDir::Next => (hunks.first()?, false),
        ChangeDir::Prev => (hunks.last()?, true),
    };
    Some((stop_row(hunk, last), hunk.buffer_line_range.clone()))
}

/// `path`'s base blob, read from the blob it was moved from when the move left
/// nothing under the new path.
///
/// The head side of the workspace's own base resolution, which is what the
/// target's diff map reads once the hop opens it. A read at the new path alone
/// answers nothing for a move, which drops an edited move out of the walk.
fn head_or_moved_content(repo: &dyn GitRepo, path: &Path) -> Option<String> {
    if let Some(head) = repo.head_content(path) {
        return Some(head);
    }
    let moved_from = repo.rename_source(path)?;
    repo.head_content(&moved_from)
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

    let (path, line, rows, wrapped) = match found {
        ChangedFileJump::To {
            path,
            line,
            rows,
            wrapped,
        } => (path, line, rows, wrapped),
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

    // The in-file walk reads its stops from the diff map, and the fresh editor
    // has none until the background job settles. Without one here the next
    // press finds no hunk in the file just landed and crosses out again, which
    // reads as the hop skipping files. A map that is already current costs
    // nothing. A stale one computes the single landed file, which is the price
    // the latched pane already pays per switch.
    if let Some((editor_id, buffer_id)) = stoat.focused_editor_ids() {
        super::review::ensure_diff_map(stoat, editor_id, buffer_id);
    }

    let center_off = stoat.settings.jump_scrolloff.unwrap_or(0);
    if let Some(editor) = focused_editor_mut(stoat) {
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let target_offset = buffer_snapshot.rope().point_to_offset(Point::new(line, 0));
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
        // dispatch that skips the Key-event epilogue still lands scrolled. The
        // hop centers its landing the way the in-file walk does, biased toward
        // the file the reader crossed from, and on the whole chunk where the
        // scan told it which rows the chunk covers. The map installed above is
        // what puts the removed-line blocks in the display, so the span maps
        // against real splices rather than against an unspliced buffer.
        let down = matches!(pending.dir, ChangeDir::Next);
        let span = stop_display_span(&editor.display_map.snapshot(), &rows);
        view::center_jump_on_span(editor, span, down, center_off);
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
pub(crate) enum ParaDir {
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

/// Move each selection to the start of the next or previous paragraph.
///
/// Backs `]p` and `[p`. Each selection runs its own scan from its own cursor,
/// and the result reaches from that cursor to the boundary rather than
/// collapsing onto it, so an operator after the motion has the paragraph to
/// work on.
pub(super) fn goto_paragraph(stoat: &mut Stoat, dir: ParaDir, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1).max(1);
    goto_paragraph_impl(stoat, dir, extend, count)
}

/// [`goto_paragraph`] with its count supplied rather than read from the
/// pending keypress, so a replay repeats the count the motion was made with.
pub(crate) fn goto_paragraph_impl(
    stoat: &mut Stoat,
    dir: ParaDir,
    extend: bool,
    count: u32,
) -> UpdateEffect {
    stoat.last_motion = Some(LastMotion::Paragraph { dir, count, extend });
    move_to_motion_range(stoat, extend, |rope, cursor| {
        let mut blanks = BlankRows::new(rope);
        Some(paragraph_range(rope, &mut blanks, cursor, dir, count))
    })
}

/// The `(anchor, head)` a paragraph motion leaves, as byte offsets.
///
/// The head is the start of the row the scan stops on, or the buffer's end when
/// the scan runs off it, so the motion always reaches the edge rather than
/// giving up short of it.
///
/// A cursor already on the character that divides two paragraphs starts the
/// scan a row further along, or the scan finds the boundary it is already on
/// and stands still. That same condition moves the anchor one grapheme, in
/// opposite directions for the two: going forward it drops the blank row's own
/// ending out of the result, and going backward it keeps the cursor's cell
/// covered.
fn paragraph_range(
    rope: &Rope,
    blanks: &mut BlankRows<'_>,
    cursor: usize,
    dir: ParaDir,
    count: u32,
) -> (usize, usize) {
    let max_row = rope.max_point().row;
    let rope_len = rope.len();
    let row = rope.offset_to_point(cursor).row;
    let line_start = |row: u32| line_start_offset(rope, row, max_row, rope_len);

    match dir {
        ParaDir::Next => {
            let on_last_char = rope.prev_grapheme_boundary(line_start(row + 1)) == cursor;
            let opens_a_paragraph =
                blanks.is_blank(row) && !blanks.is_blank((row + 1).min(max_row));
            let adjust = opens_a_paragraph && on_last_char;

            let mut row = row + u32::from(adjust);
            let mut last_row = row;
            for _ in 0..count {
                while row <= max_row && !blanks.is_blank(row) {
                    row += 1;
                }
                while row <= max_row && blanks.is_blank(row) {
                    row += 1;
                }
                if row == last_row {
                    break;
                }
                last_row = row;
            }

            let anchor = if adjust {
                rope.next_grapheme_boundary(cursor)
            } else {
                cursor
            };
            (anchor, line_start(row))
        },
        ParaDir::Prev => {
            let on_first_char = line_start(row) == cursor;
            let follows_a_paragraph =
                blanks.is_blank(row.saturating_sub(1)) && !blanks.is_blank(row);
            let adjust = follows_a_paragraph && !on_first_char;

            let mut row = row + u32::from(adjust);
            let mut last_row = row;
            for _ in 0..count {
                while row > 0 && blanks.is_blank(row - 1) {
                    row -= 1;
                }
                while row > 0 && !blanks.is_blank(row - 1) {
                    row -= 1;
                }
                if row == last_row {
                    break;
                }
                last_row = row;
            }

            let anchor = if follows_a_paragraph && on_first_char {
                cursor
            } else {
                rope.next_grapheme_boundary(cursor)
            };
            (anchor, line_start(row))
        },
    }
}
pub(super) fn match_brackets(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;

    // The snapshot outlives the editor borrow, so the rope stays readable while
    // the syntax map answers for each cursor in turn.
    let (display_snapshot, reads) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let display_snapshot = editor.display_map.snapshot();
        let reads = editor
            .selections
            .resolved_reads(display_snapshot.buffer_snapshot());
        (display_snapshot, reads)
    };
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    // Every cursor pairs its own bracket. Resolving one partner and landing it
    // on all of them makes every span identical, and identical spans merge, so
    // the set collapses to one cursor.
    //
    // A cursor reads the layer it falls in, not the file's own. A fenced block
    // in a markup file parses under its own grammar, so its brackets pair
    // through that grammar's tree and that language's query.
    let mut targets: Vec<(usize, usize)> = {
        let snapshot = ws.buffers.syntax_map(buffer_id).map(|sm| sm.snapshot());
        reads
            .iter()
            .filter_map(|read| {
                let cursor = cursor_offset(rope, read.tail, read.head);
                let layer = surround::deepest_layer_at(snapshot, cursor);
                let query = layer.and_then(|layer| layer.language.bracket_query());
                let target = bracket_partner(rope, cursor, query, layer.map(|l| &l.tree))?;
                Some((read.id, target))
            })
            .collect()
    };
    targets.sort_unstable_by_key(|(id, _)| *id);

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    // A cursor sitting in no pair holds its place.
    move_cursors(&mut editor.selections, buffer_snapshot, extend, |read| {
        let found = targets.binary_search_by_key(&read.id, |(id, _)| *id).ok()?;
        Some((targets[found].1, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

/// Offset of the bracket partnering the one the cursor sits in, or `None` when
/// it sits in no pair.
///
/// Three paths answer, narrowing as less is known about the buffer.
///
/// A brackets query captures only structural delimiters, so a bracket inside a
/// string, char, or comment literal resolves to no pair instead of
/// false-matching. Where the language ships one it is authoritative.
///
/// A language with a grammar but no query reads the tree directly, which still
/// names the construct the cursor is in. Both syntax paths therefore match from
/// within a pair and not only on a delimiter.
///
/// Text with no tree at all falls to the character scan, which matches only a
/// cursor already on a delimiter. Nothing there says which side of a quote
/// opens, or whether a bracket is inside a comment.
fn bracket_partner(
    rope: &Rope,
    cursor: usize,
    query: Option<&stoat_language::Query>,
    tree: Option<&stoat_language::Tree>,
) -> Option<usize> {
    if let (Some(query), Some(tree)) = (query, tree) {
        return stoat_language::matching_bracket(query, tree.root_node(), rope, cursor);
    }
    if let Some(tree) = tree
        && let Some(found) =
            stoat_language::matching_bracket_from_tree(tree.root_node(), rope, cursor)
    {
        return Some(found);
    }

    let ch = rope.chars_at(cursor).next()?;
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

/// Pairs a bracket match walks for in text with no syntax tree to consult.
///
/// Their delimiters are distinct characters, so a character under the cursor
/// names at most one pair and says which end of it the cursor is on. That is
/// what a plaintext walk needs. It is also what keeps quotes out, since `"`
/// opens and closes alike and only a tree tells the two apart.
pub(crate) const BRACKET_PAIRS: [(char, char); 9] = [
    ('(', ')'),
    ('{', '}'),
    ('[', ']'),
    ('<', '>'),
    ('\u{2018}', '\u{2019}'),
    ('\u{201c}', '\u{201d}'),
    ('\u{ab}', '\u{bb}'),
    ('\u{300c}', '\u{300d}'),
    ('\u{ff08}', '\u{ff09}'),
];

/// The pair `ch` belongs to and whether it opens one, or `None` when it is no
/// bracket at all.
fn bracket_pair(ch: char) -> Option<(char, char, bool)> {
    BRACKET_PAIRS
        .into_iter()
        .find(|&(open, close)| ch == open || ch == close)
        .map(|(open, close)| (open, close, ch == open))
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
    /// Zones a walk that reads the text alone skips over, which is none.
    pub(crate) fn none() -> Self {
        Self { ranges: Vec::new() }
    }

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

/// What a pair walk is allowed to read, and how far.
///
/// The walks need the tree itself for the one question zones leave open, which
/// is where the string node under a cursor starts and ends. Carrying it
/// alongside replaces the tree the walks already thread rather than adding a
/// second parameter beside it.
///
/// `reach` and `zones` are the policy the caller picks. A structural walk reads
/// both, so it stops at a bounded distance and steps over brackets that a
/// string or a comment owns. A plaintext walk reads neither and answers from
/// the characters alone.
pub(crate) struct PairScan<'a> {
    pub(crate) tree: Option<&'a stoat_language::Tree>,
    pub(crate) zones: SkipZones,
    pub(crate) reach: usize,
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
            reach: MAX_PAIR_SCAN,
        }
    }

    /// A walk over the characters alone, to whichever end of the rope it
    /// reaches first.
    ///
    /// `tree` still rides along, because a cursor sitting on a character that
    /// opens and closes alike has no answer in the text and only the tree says
    /// which side of it opens.
    ///
    /// The zones a structural walk collects are sized to the window
    /// [`window_around`] gives it, so they are only ever valid for a walk that
    /// stops inside that window. A walk with no such stop reads them and
    /// classifies everything past the window as code.
    pub(crate) fn plaintext(tree: Option<&'a stoat_language::Tree>) -> Self {
        Self {
            zones: SkipZones::none(),
            tree,
            reach: usize::MAX,
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

/// The last row a line-numbered jump lands on.
///
/// A trailing newline opens a row holding nothing, which is a cursor position
/// but not a line anyone names, so `G` on a file ending in a newline reaches
/// the last row with text on it.
fn last_addressable_row(rope: &Rope) -> u32 {
    let last = rope.max_point().row;
    match last > 0 && rope.line_len(last) == 0 {
        true => last - 1,
        false => last,
    }
}

/// Land every selection on the start of the line `count` names, one-indexed
/// and clamped to [`last_addressable_row`].
///
/// Pushes no jump. The keys that reach a numbered line differ over what they do
/// without a count, so each pushes its own before it gets here.
fn goto_line_row(stoat: &mut Stoat, count: u32, extend: bool) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let zero_indexed = count.saturating_sub(1);
    let target_row = (zero_indexed as u64).min(last_addressable_row(rope) as u64) as u32;
    let target_offset = rope.point_to_offset(Point::new(target_row, 0));

    move_cursors(&mut editor.selections, buffer_snapshot, extend, |_| {
        Some((target_offset, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

/// Land on the line a count names, or on the last line without one.
///
/// `extend` decides whether the selections grow to the target or collapse onto
/// it. Select mode binds `G` to the extending flavor, which is stoat's own
/// scheme rather than a key Helix rebinds there, and a count reaching the
/// target is what makes the two flavors agree on what `G` means.
pub(super) fn goto_line_number(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    let Some(count) = stoat.take_pending_count() else {
        return goto_last_line(stoat, extend);
    };
    super::jump::push_jump(stoat);
    goto_line_row(stoat, count, extend)
}

pub(super) fn goto_column(stoat: &mut Stoat, extend: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1);
    super::jump::push_jump(stoat);
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
    let target_offset = rope.point_to_offset(Point::new(last_addressable_row(rope), 0));
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

/// Land the one selection over `range`, running forward, and record the origin
/// on the jumplist.
///
/// The label jump crosses the viewport in one press, so the place it left is
/// worth getting back to. Replacing the whole set rather than moving each
/// cursor is what makes the labelled word the thing selected. Moving each
/// cursor instead stamps every one of them onto the same word.
pub(crate) fn jump_to_word_range(stoat: &mut Stoat, range: (usize, usize)) -> UpdateEffect {
    super::jump::push_jump(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let len = buffer_snapshot.rope().len();

    let start = buffer_snapshot.anchor_at(range.0.min(len), Bias::Left);
    let end = buffer_snapshot.anchor_at(range.1.min(len), Bias::Right);
    editor
        .selections
        .set_single_range(start, end, false, SelectionGoal::None);
    UpdateEffect::Redraw
}

pub(super) fn add_newline_below(stoat: &mut Stoat) -> UpdateEffect {
    add_newline(stoat, false)
}

pub(super) fn add_newline_above(stoat: &mut Stoat) -> UpdateEffect {
    add_newline(stoat, true)
}

/// Insert blank lines around the lines the selections touch.
///
/// A selection ending exactly at a line start does not reach the line below it,
/// so its span steps back a grapheme first. Two selections on one line ask for
/// the same insert point, and their two requests become one insert of both
/// runs, which lands the text each of them asked for.
fn add_newline(stoat: &mut Stoat, above: bool) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let buffer_id = editor.buffer_id;
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let max_row = rope.max_point().row;

    let mut offsets: Vec<usize> = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            let start = buffer_snapshot.resolve_anchor(&sel.start);
            let end = buffer_snapshot.resolve_anchor(&sel.end);
            let last = match end > start {
                true => rope.prev_grapheme_boundary(end).max(start),
                false => end,
            };
            match above {
                true => rope.point_to_offset(Point::new(rope.offset_to_point(start).row, 0)),
                false => match rope.offset_to_point(last).row {
                    row if row >= max_row => rope.len(),
                    row => rope.point_to_offset(Point::new(row + 1, 0)),
                },
            }
        })
        .collect();
    offsets.sort_unstable_by(|a, b| b.cmp(a));

    // Descending, so each insert lands before the offsets still to come move
    // under it. Runs of one offset collapse into a single insert.
    let mut batch: Vec<(usize, String)> = Vec::with_capacity(offsets.len());
    for offset in offsets {
        match batch.last_mut() {
            Some((at, text)) if *at == offset => text.push_str(&"\n".repeat(count)),
            _ => batch.push((offset, "\n".repeat(count))),
        }
    }
    if batch.is_empty() {
        return UpdateEffect::None;
    }

    let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
        return UpdateEffect::None;
    };
    let mut guard = buffer.write().expect("buffer poisoned");
    let edits: Vec<(Range<usize>, &str)> = batch
        .iter()
        .map(|(at, text)| (*at..*at, text.as_str()))
        .collect();
    guard.edit_batch(&edits);
    UpdateEffect::Redraw
}

/// Move every cursor to where the newest edit finished.
///
/// A buffer with no edits does nothing at all, rather than moving cursors to
/// some default, since there is no modification to go to.
pub(super) fn goto_last_modification(stoat: &mut Stoat) -> UpdateEffect {
    let extend = stoat.in_select_mode();
    let ws = stoat.active_workspace();
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };
    let Some(pos) = ws
        .buffers
        .get(buffer_id)
        .and_then(|buffer| buffer.read().expect("poisoned").last_edit_pos())
    else {
        return UpdateEffect::None;
    };

    super::jump::push_jump(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let target = pos.min(buffer_snapshot.rope().len());
    move_cursors(&mut editor.selections, buffer_snapshot, extend, |_| {
        Some((target, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

pub(super) fn goto_word(stoat: &mut Stoat) -> UpdateEffect {
    arm_word_labels(stoat, None)
}

/// Label the viewport's words, growing the selection to the one chosen rather
/// than replacing it.
///
/// The primary's span is captured now rather than read when the label arrives,
/// since the label keystrokes are what the extend measures from and none of
/// them moves the cursor.
pub(super) fn extend_to_word(stoat: &mut Stoat) -> UpdateEffect {
    let anchor = {
        let Some(editor) = focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let primary = editor.selections.newest_anchor();
        (
            buffer_snapshot.resolve_anchor(&primary.start),
            buffer_snapshot.resolve_anchor(&primary.end),
        )
    };
    arm_word_labels(stoat, Some(anchor))
}

fn arm_word_labels(stoat: &mut Stoat, extend_from: Option<(usize, usize)>) -> UpdateEffect {
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
    stoat.pending_goto_word_extend = extend_from;
    stoat.pending_goto_word_input.clear();
    UpdateEffect::Redraw
}

/// Grow the selection from `primary` to reach `label`, and record the origin.
///
/// A label ahead of the selection anchors at its start and a label behind
/// anchors at its end, so the edge the user came from is the one kept. The
/// pivot is the selection's own start, matching the cursor the labels were
/// walked out from.
pub(crate) fn extend_to_word_range(
    stoat: &mut Stoat,
    primary: (usize, usize),
    label: (usize, usize),
) -> UpdateEffect {
    let behind = label.0 < primary.0;
    let (from, to, reversed) = match behind {
        true => (label.0, label.1.max(primary.1), true),
        false => (label.0.min(primary.0), label.1, false),
    };

    super::jump::push_jump(stoat);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let len = buffer_snapshot.rope().len();

    let start = buffer_snapshot.anchor_at(from.min(len), Bias::Left);
    let end = buffer_snapshot.anchor_at(to.min(len), Bias::Right);
    editor
        .selections
        .set_single_range(start, end, reversed, SelectionGoal::None);
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests;
