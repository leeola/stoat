//! Viewport and scroll management for the focused editor: where the view sits
//! in the document, and how the cursor and the view stay coupled.
//!
//! The view is a window onto display rows, so everything here resolves through
//! the display map rather than buffer lines -- folds, soft wraps, and block
//! rows such as deleted-line blocks all take display rows the buffer does not
//! have. `scroll_row` is the display row at the top of the window and
//! `scroll_offset` its fractional counterpart, which a glide eases up to the
//! integer target so a page or wheel scroll moves smoothly rather than jumping.
//!
//! Cursor and view are pulled together from both ends. After a key moves the
//! cursor, [`ensure_cursor_in_view`] moves the view to it. After a wheel coast
//! moves the view past a stationary cursor, [`clamp_cursor_to_view`] moves the
//! cursor to it.

use super::{focused_editor_mut, movement};
use crate::{
    app::{Stoat, UpdateEffect},
    display_map::DisplayPoint,
    editor_state::{EditorState, ScrollGlide},
};
use stoat_text::{cursor_offset, Bias, SelectionGoal};

#[derive(Copy, Clone, Debug)]
pub(super) enum PageDir {
    Up,
    Down,
}

/// Fallback viewport height when the focused editor has not been
/// rendered yet (e.g. a unit test that dispatches a page action
/// without running a render pass).
pub(crate) const DEFAULT_VIEWPORT_ROWS: u32 = 20;

/// The display row a half page carries the cursor to, one row of travel per row
/// of scroll.
///
/// A downward jump past the last display row lands on it, since the empty
/// buffer row beyond has no display row of its own.
fn cursor_row_by_delta(current: u32, delta: u32, dir: PageDir, max_row: u32) -> u32 {
    match dir {
        PageDir::Up => current.saturating_sub(delta),
        PageDir::Down => current.saturating_add(delta).min(max_row),
    }
}

/// The display row a full page leaves the cursor on, which is where it already
/// was unless the scrolled view drags it past the scrolloff edge it recedes
/// toward.
///
/// The pull works in one direction only. Scrolling down lifts a cursor off the
/// top edge and scrolling up lifts one off the bottom. A cursor the view
/// scrolls toward keeps its row, so a page up at the top of the buffer moves
/// nothing.
fn cursor_row_at_edge(current: u32, scroll_row: u32, viewport: u32, dir: PageDir, gap: u32) -> u32 {
    let gap = gap.min(viewport.saturating_sub(1) / 2);
    match dir {
        PageDir::Up => current.min(scroll_row + viewport.saturating_sub(gap + 1)),
        PageDir::Down => current.max(scroll_row + gap),
    }
}

pub(super) fn page_motion(stoat: &mut Stoat, dir: PageDir, half: bool) -> UpdateEffect {
    let extend = stoat.in_select_mode();
    let count = stoat.take_pending_count().unwrap_or(1);
    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);
    let base_delta = if half { viewport / 2 } else { viewport };
    let delta = base_delta.saturating_mul(count);

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let max_point = display_snapshot.max_point();

    let sel = editor.selections.newest_anchor().clone();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(rope, tail_off, head_off);
    let current = display_snapshot.buffer_to_display(rope.offset_to_point(cursor));

    let prev = editor.scroll_row;
    let max_scroll = max_scroll_row(display_snapshot.line_count(), viewport);
    let new_scroll = match dir {
        PageDir::Up => editor.scroll_row.saturating_sub(delta),
        PageDir::Down => editor.scroll_row.saturating_add(delta).min(max_scroll),
    };

    // The half-page keys carry the cursor with the view and the full-page keys
    // leave it where it is, which is the split Helix draws between its two
    // families rather than between the two distances.
    let target_row = match half {
        true => cursor_row_by_delta(current.row, delta, dir, max_point.row),
        false => cursor_row_at_edge(current.row, new_scroll, viewport, dir, scrolloff),
    };

    // Move the cursor in display rows so it tracks the scroll leg below one for
    // one across block rows. A downward jump past the last display row lands on
    // the final cell, since the empty buffer row beyond it has no display row of
    // its own.
    let target_point = match target_row >= max_point.row {
        true => max_point,
        false => {
            let travel = match dir {
                PageDir::Up => Bias::Left,
                PageDir::Down => Bias::Right,
            };
            movement::clip_to_goal(&display_snapshot, target_row, current.column, travel)
        },
    };
    // A full page scrolls even where the cursor keeps its row, so the view
    // moving is its own reason to go on.
    if target_point == current && new_scroll == prev {
        return UpdateEffect::None;
    }
    let Some(target_buffer_pt) = display_snapshot.display_to_buffer(target_point) else {
        return UpdateEffect::None;
    };
    let target_offset = rope.point_to_offset(target_buffer_pt);

    editor.scroll_row = new_scroll;
    editor.scroll_frac = 0.0;
    // In select mode the page motion grows the selection by holding the anchor
    // and moving the head to the target row like Helix.
    movement::move_cursors(&mut editor.selections, buffer_snapshot, extend, |_| {
        Some((target_offset, SelectionGoal::None))
    });

    // Ease scroll_offset from the visible position up to the scroll_row target
    // the jump set, so a page motion glides instead of teleporting. The cursor
    // moved with scroll_row by the same delta, so it stays pinned to its screen
    // row and the post-key view-follow is a no-op.
    if editor.scroll_offset.floor() as u32 != prev {
        editor.scroll_offset = prev as f32;
    }
    editor.scroll_glide = ScrollGlide::Page;
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
/// clears any in-flight glide, so a keyboard line scroll cancels a page or wheel
/// glide and keeps the fractional position in step with the integer row.
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
    editor.scroll_frac = 0.0;
    editor.scroll_offset = new_scroll as f32;
    editor.scroll_glide = ScrollGlide::None;
    true
}

/// Advance `editor`'s wheel scroll one report, down when `down` and up
/// otherwise, arming a [`ScrollGlide::Wheel`] ease toward the new target.
///
/// One report is one line of travel, which [`wheel_scroll_by`] turns into the
/// three rows a notch moves.
pub(crate) fn wheel_scroll(editor: &mut EditorState, down: bool) {
    wheel_scroll_by(editor, if down { 1.0 } else { -1.0 });
}

/// Move `editor`'s wheel scroll target by `lines` of travel, positive going
/// down the document, arming a [`ScrollGlide::Wheel`] ease toward it.
///
/// Each line moves the target a fixed three rows -- matching the scrollback and
/// run-pane wheel steps -- toward the document bound, and the tick eases
/// `scroll_offset` up to it, so steady wheel input yields steady speed. The
/// selection stays anchored to its buffer line while the glide moves fast,
/// sliding out of view with the content. Once the glide slows below its re-home
/// velocity the cursor lands in the scrolloff band, ahead of the settle.
///
/// Fractional travel lands a fractional target: the whole part goes to
/// `scroll_row` and the remainder to `scroll_frac`, so the rest can sit between
/// two rows rather than snapping to one.
///
/// Reseeds `scroll_offset` from `scroll_row` only when no glide is in flight and
/// another path moved the integer row out from under the fraction. Mid-glide the
/// offset legitimately lags the target, so it must not be reseeded.
pub(crate) fn wheel_scroll_by(editor: &mut EditorState, lines: f32) {
    const STEP: f32 = 3.0;

    let max_scroll = max_scroll_offset(editor);
    let from = editor.scroll_row as f32 + editor.scroll_frac;
    let target = (from + lines * STEP).clamp(0.0, max_scroll);
    if target == from && editor.scroll_glide == ScrollGlide::None {
        return;
    }

    if editor.scroll_glide == ScrollGlide::None
        && editor.scroll_offset.floor() as u32 != editor.scroll_row
    {
        editor.scroll_offset = editor.scroll_row as f32;
    }
    editor.scroll_row = target.floor() as u32;
    editor.scroll_frac = target.fract();
    editor.scroll_glide = ScrollGlide::Wheel;
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

/// Ease `offset` toward `target` by `dt` seconds, closing `ease_per_nominal` of
/// the remaining gap per `NOMINAL_DT`. Returns the new offset and whether it
/// settled onto the target.
///
/// A glide jumps `scroll_row` to its target and lets `scroll_offset` ease up to
/// it. `ease_per_nominal` is raised to `dt / NOMINAL_DT` so the glide lasts the
/// same wall-clock time however often it ticks. A page motion closes the gap
/// fast. A wheel glide eases slower, so a stream of reports overlaps into
/// continuous motion. Within `EPSILON` of the target it snaps exactly onto it
/// and reports settled, so the caller can stop ticking.
///
/// The step never falls under `MIN_STEP`, which turns the exponential tail into
/// a constant-velocity lock-in that lands exactly on the target. Left
/// asymptotic, the last fraction of a row moves under a pixel per frame for many
/// frames, and the renderer quantizes that to whole pixels, so the stop reads as
/// an irregular train of one-pixel steps rather than an arrival.
pub(crate) fn step_scroll_ease(
    offset: f32,
    target: f32,
    dt: f32,
    ease_per_nominal: f32,
) -> (f32, bool) {
    const NOMINAL_DT: f32 = 0.008;
    const EPSILON: f32 = 0.01;
    // Rows per NOMINAL_DT, so 9.4 rows per second. Under the velocity at which a
    // wheel glide re-homes the cursor mid-flight, which keeps that re-home firing
    // through the lock-in instead of the floor carrying the glide past it.
    const MIN_STEP: f32 = 0.075;

    let remaining = target - offset;
    if remaining.abs() < EPSILON {
        return (target, true);
    }

    let frames = dt / NOMINAL_DT;
    let kept = (1.0 - ease_per_nominal).powf(frames);
    let step = (remaining.abs() * (1.0 - kept)).max(MIN_STEP * frames);

    // Landing on `target` itself rather than adding the capped step, which in
    // f32 arrives a hair to one side of it.
    if step >= remaining.abs() {
        return (target, true);
    }
    (offset + step.copysign(remaining), false)
}

pub(super) fn align_view(stoat: &mut Stoat, align: ViewAlign) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);

    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let sel = editor.selections.newest_anchor().clone();
    let rope = buffer_snapshot.rope();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(rope, tail_off, head_off);
    let cursor_row = display_snapshot
        .buffer_to_display(rope.offset_to_point(cursor))
        .row;

    // All three arms measure from the last visible row rather than the row
    // count, so an even viewport centers one row above its geometric middle
    // and `Bottom` reaches the last row instead of the one past it.
    let last_row = viewport.saturating_sub(1);
    let desired_scroll = match align {
        ViewAlign::Top => cursor_row,
        ViewAlign::Center => cursor_row.saturating_sub(last_row / 2),
        ViewAlign::Bottom => cursor_row.saturating_sub(last_row),
    };
    let max_scroll = max_scroll_row(display_snapshot.line_count(), viewport);
    editor.scroll_row = desired_scroll.min(max_scroll);
    editor.scroll_frac = 0.0;
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
    let sel = editor.selections.newest_anchor().clone();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(rope, tail_off, head_off);
    let cursor_row = snapshot.buffer_to_display(rope.offset_to_point(cursor)).row;

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
    if editor.scroll_row != before {
        // Following the cursor is a row-aligned move, so it lands on the grid
        // rather than inheriting whatever fraction the wheel rested on.
        editor.scroll_frac = 0.0;
    }
    editor.scroll_row != before
}

/// Follow the cursor after a jump, gliding the view from where it was instead of
/// teleporting, and report whether the view moved.
///
/// The cursor's content anchor is only sent to the terminal while a glide is
/// armed, so a jump that moved the view bare leaves the terminal easing the
/// cursor from wherever it sat on screen. Seeding `scroll_offset` with the
/// pre-jump row makes the cursor ride the eased content and arrive from the
/// direction it came from in the document.
///
/// A single-row follow stays a snap, so plain motion at the viewport margin
/// keeps its immediate feel.
pub(crate) fn follow_jump(editor: &mut EditorState, scrolloff: u32) -> bool {
    let prev = editor.scroll_row;
    let scrolled = ensure_cursor_in_view(editor, scrolloff);

    if scrolled && prev.abs_diff(editor.scroll_row) > 1 {
        editor.scroll_offset = prev as f32;
        editor.scroll_glide = ScrollGlide::Page;
    }

    scrolled
}

/// Center the view on the cursor after a jump, biased `center_off` rows toward
/// the side the jump arrived from, and report whether the view moved.
///
/// A jump takes the reader somewhere they were not looking, so the landing is
/// worth putting in the middle of the screen rather than at the edge the
/// ordinary follow leaves it on. `down` says which way the jump went, and the
/// bias keeps that many rows of where the reader came from on screen, so the
/// move reads as a direction rather than a teleport.
///
/// The band is one-sided for a landing already on screen. Such a jump arrives
/// from one direction, so the far side needs no guard, and a landing past the
/// target sits where it is rather than pulling the view back under a reader who
/// can already see it.
///
/// A landing off screen centers whichever way it came. A cross-file hop opens
/// its target at the top of the file, so the one-sided band would leave a
/// backward hop's landing below the last visible row.
///
/// `center_off` is capped at half the viewport, and the result is clamped to
/// `max_scroll`, so a landing near the document end pins to the bottom rather
/// than scrolling blank space into view. A move over one row seeds the glide
/// from the pre-jump row, exactly as [`follow_jump`] does.
pub(crate) fn center_jump_on_cursor(editor: &mut EditorState, down: bool, center_off: u32) -> bool {
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);

    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let sel = editor.selections.newest_anchor().clone();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(rope, tail_off, head_off);
    let cursor_row = snapshot.buffer_to_display(rope.offset_to_point(cursor)).row;

    let center = viewport / 2;
    let center_off = center_off.min(viewport.saturating_sub(1) / 2);
    let target = match down {
        true => center + center_off,
        false => center.saturating_sub(center_off),
    };
    let max_scroll = max_scroll_row(snapshot.line_count(), viewport);

    let prev = editor.scroll_row;
    let sits_past = match down {
        true => cursor_row > prev + target,
        false => cursor_row < prev + target,
    };
    let on_screen = (prev..prev + viewport).contains(&cursor_row);
    if !sits_past && on_screen {
        return false;
    }
    editor.scroll_row = cursor_row.saturating_sub(target).min(max_scroll);
    // A jump lands on the row grid, whatever fraction the wheel last rested on.
    editor.scroll_frac = 0.0;

    if prev.abs_diff(editor.scroll_row) > 1 {
        editor.scroll_offset = prev as f32;
        editor.scroll_glide = ScrollGlide::Page;
    }
    editor.scroll_row != prev
}

/// The display row the primary cursor's caret sits on.
///
/// Resolves the newest selection's caret to a buffer offset and maps it through
/// the display map, so folds and soft wraps are accounted for rather than the raw
/// buffer line.
pub(crate) fn cursor_display_row(editor: &mut EditorState) -> u32 {
    cursor_display_cell(editor).0
}

/// The display row and column the primary cursor's caret sits on.
///
/// Like [`cursor_display_row`] but also returns the column, for callers placing
/// a pool cursor in a detached pane's window where no live paint recorded the
/// screen cell.
pub(crate) fn cursor_display_cell(editor: &mut EditorState) -> (u32, u32) {
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let sel = editor.selections.newest_anchor().clone();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(rope, tail_off, head_off);
    let display = snapshot.buffer_to_display(rope.offset_to_point(cursor));
    (display.row, display.column)
}

/// The dual of [`ensure_cursor_in_view`]: move the cursor to the view rather
/// than the view to the cursor.
///
/// A wheel coast advances `scroll_row` past a stationary cursor. This drags the
/// selection back inside the scrolloff band so the two never decouple, and a
/// later cursor motion has no stranded view to snap back. Self-gates to a no-op
/// when the primary cursor already sits inside the band.
///
/// Every selection shifts by the primary cursor's band-correction delta, the way
/// a count `j`/`k` moves them together, preserving each goal column and clipping
/// block rows toward the direction of travel. Extends the selection in select
/// mode. Returns whether any selection moved.
pub(crate) fn clamp_cursor_to_view(editor: &mut EditorState, scrolloff: u32) -> bool {
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1);

    let cursor_row = cursor_display_row(editor);

    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    let top = scrolloff.min(viewport.saturating_sub(1) / 2);
    let bottom = scrolloff.min(viewport / 2);
    let max_row = snapshot.max_point().row;

    let band_top = editor.scroll_row + top;
    let band_bottom = (editor.scroll_row + viewport.saturating_sub(1))
        .saturating_sub(bottom)
        .min(max_row);

    let (target_row, travel) = if cursor_row < band_top {
        (band_top.min(max_row), Bias::Right)
    } else if cursor_row > band_bottom {
        (band_bottom, Bias::Left)
    } else {
        return false;
    };
    if target_row == cursor_row {
        return false;
    }
    let row_delta = target_row as i64 - cursor_row as i64;

    let extend = editor.mode == "select";
    movement::move_cursors(&mut editor.selections, buffer_snapshot, extend, |read| {
        let sel_cursor = cursor_offset(rope, read.tail, read.head);
        let cursor_pt = rope.offset_to_point(sel_cursor);
        let cursor_display = snapshot.buffer_to_display(cursor_pt);
        // Cells along the display row, which is what a vertical motion carries.
        // The band is one of display rows too, so the row and the column are
        // counted in the same space and the clamp needs no conversion.
        let goal_col = match read.goal {
            SelectionGoal::Column(c) => c,
            SelectionGoal::None => cursor_display.column,
        };
        let new_row_i = (cursor_display.row as i64).saturating_add(row_delta);
        let new_row = new_row_i.clamp(0, max_row as i64) as u32;
        if new_row == cursor_display.row {
            return None;
        }
        let clipped = movement::clip_to_goal(&snapshot, new_row, goal_col, travel);
        let buffer_pt = snapshot.display_to_buffer(clipped)?;
        Some((
            rope.point_to_offset(buffer_pt),
            SelectionGoal::Column(goal_col),
        ))
    });
    true
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
    let max_display_row = display_snapshot.max_point().row;

    let offset = match align {
        WindowAlign::Top => 0,
        WindowAlign::Center => viewport / 2,
        WindowAlign::Bottom => viewport.saturating_sub(1),
    };
    let target_row = scroll_row.saturating_add(offset).min(max_display_row);

    let target_point = display_snapshot.clip_point(DisplayPoint::new(target_row, 0), Bias::Left);
    let Some(target_buffer_pt) = display_snapshot.display_to_buffer(target_point) else {
        return UpdateEffect::None;
    };
    let target_offset = rope.point_to_offset(target_buffer_pt);
    movement::move_cursors(&mut editor.selections, buffer_snapshot, extend, |_| {
        Some((target_offset, SelectionGoal::None))
    });
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers::dispatch,
        diff_map::{DiffHunk, DiffHunkStatus, DiffMap},
        pane::View,
        test_harness::{
            editor::{self, focused_cursor_point, focused_head_row, place_cursor},
            stoat, TestHarness,
        },
    };
    use std::sync::Arc;
    use stoat_action::{AddSelectionBelow, HalfPageDown, PageDown, PageUp};
    use stoat_text::Point;

    #[test]
    fn ensure_cursor_in_view_follows_cursor_and_noops_when_visible() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        movement::set_cursor_row(editor, 50);
        editor.scroll_row = 0;
        assert!(
            ensure_cursor_in_view(editor, 0),
            "a below-viewport cursor scrolls the view",
        );
        assert_eq!(
            editor.scroll_row, 41,
            "a below-viewport cursor pulls the view down to it",
        );

        movement::set_cursor_row(editor, 45);
        editor.scroll_row = 41;
        assert!(
            !ensure_cursor_in_view(editor, 0),
            "an already-visible cursor does not scroll",
        );
        assert_eq!(
            editor.scroll_row, 41,
            "an already-visible cursor leaves the view put",
        );

        movement::set_cursor_row(editor, 8);
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
        movement::set_cursor_row(editor, 50);
        editor.scroll_row = 0;
        assert!(ensure_cursor_in_view(editor, 3));
        assert_eq!(
            editor.scroll_row, 44,
            "downward jump keeps a 3-row margin below the cursor",
        );

        // An upward jump keeps 3 rows above the cursor (scroll_row = 8 - 3).
        movement::set_cursor_row(editor, 8);
        editor.scroll_row = 44;
        assert!(ensure_cursor_in_view(editor, 3));
        assert_eq!(
            editor.scroll_row, 5,
            "upward jump keeps a 3-row margin above the cursor",
        );
    }

    #[test]
    fn follow_jump_glides_a_far_move_from_the_pre_jump_row() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        movement::set_cursor_row(editor, 50);
        editor.scroll_row = 0;
        editor.scroll_offset = 0.0;
        editor.scroll_glide = ScrollGlide::None;

        assert!(follow_jump(editor, 3), "a far jump scrolls the view");
        assert_eq!(editor.scroll_row, 44, "the view follows the cursor");
        assert_eq!(
            editor.scroll_offset, 0.0,
            "the glide starts from the pre-jump row so the cursor rides the content"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::Page,
            "a far jump arms the glide that ships the cursor anchor"
        );
    }

    /// A jump lands in the middle rather than at the edge the walk arrived
    /// from, and the bias keeps rows of where the reader came from on screen.
    #[test]
    fn center_jump_lands_the_cursor_at_center_plus_the_bias() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        // Viewport 10, so the center row is 5 and a bias of 2 lands 7 or 3.
        editor.viewport_rows = Some(10);

        movement::set_cursor_row(editor, 50);
        editor.scroll_row = 0;
        editor.scroll_offset = 0.0;
        editor.scroll_glide = ScrollGlide::None;

        assert!(center_jump_on_cursor(editor, true, 2), "a far jump scrolls");
        assert_eq!(
            editor.scroll_row, 43,
            "a downward landing sits two rows past center, so what stays on \
             screen above it is where the reader came from",
        );
        assert_eq!(
            (editor.scroll_offset, editor.scroll_glide),
            (0.0, ScrollGlide::Page),
            "and the glide starts from the pre-jump row, as every jump follow does",
        );

        movement::set_cursor_row(editor, 8);
        editor.scroll_row = 43;
        assert!(
            center_jump_on_cursor(editor, false, 2),
            "the reversal scrolls too"
        );
        assert_eq!(
            editor.scroll_row, 5,
            "an upward landing sits two rows short of center",
        );
    }

    /// A cross-file hop opens its target at the top, so a backward landing
    /// sits below every visible row. The band is one-sided only for a landing
    /// the reader can already see.
    #[test]
    fn center_jump_centers_an_off_screen_landing_either_way() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        movement::set_cursor_row(editor, 50);
        editor.scroll_row = 0;

        assert!(
            center_jump_on_cursor(editor, false, 2),
            "a backward hop onto a row past the last visible one still scrolls",
        );
        assert_eq!(
            editor.scroll_row, 47,
            "and it lands two rows short of center, the way a backward jump does",
        );
    }

    /// A hop the reader can already see stays put, so the view does not pull
    /// itself out from under them for a row or two.
    #[test]
    fn center_jump_leaves_a_landing_short_of_the_band() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        movement::set_cursor_row(editor, 3);
        editor.scroll_row = 0;

        assert!(
            !center_jump_on_cursor(editor, true, 2),
            "a landing above the band leaves the view alone",
        );
        assert_eq!(editor.scroll_row, 0, "and the view stays where it was");
    }

    /// The last rows of a file cannot be centered without scrolling blank
    /// space into view, so the landing pins to the bottom instead.
    #[test]
    fn center_jump_clamps_at_the_document_end() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        movement::set_cursor_row(editor, 99);
        editor.scroll_row = 0;
        let max = max_scroll_row(editor.display_map.snapshot().line_count(), 10);

        assert!(center_jump_on_cursor(editor, true, 2));
        assert_eq!(
            editor.scroll_row, max,
            "the view stops at the last full screen",
        );
        assert!(
            max < 99 - 7,
            "which is short of the row centering alone would ask for",
        );
    }

    /// A bias past half the viewport would put the landing off screen, so it
    /// caps at the half.
    #[test]
    fn center_jump_caps_the_bias_at_half_the_viewport() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        movement::set_cursor_row(editor, 50);
        editor.scroll_row = 0;

        assert!(center_jump_on_cursor(editor, true, 99));
        assert_eq!(
            editor.scroll_row, 41,
            "the bias caps at four, so the landing sits on the last row of the \
             ten the viewport holds",
        );
    }

    #[test]
    fn follow_jump_snaps_a_single_row_move() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        // One row past the bottom margin, the follow every plain j at the edge does.
        movement::set_cursor_row(editor, 7);
        editor.scroll_row = 0;
        editor.scroll_offset = 0.0;
        editor.scroll_glide = ScrollGlide::None;

        assert!(follow_jump(editor, 3), "the margin follow still scrolls");
        assert_eq!(editor.scroll_row, 1, "the view moves exactly one row");
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::None,
            "a one-row follow stays a snap, keeping plain motion immediate"
        );
    }

    #[test]
    fn follow_jump_leaves_a_visible_cursor_alone() {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        movement::set_cursor_row(editor, 45);
        editor.scroll_row = 41;
        editor.scroll_glide = ScrollGlide::None;

        assert!(!follow_jump(editor, 0), "an in-view cursor does not scroll");
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::None,
            "no view move arms no glide"
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

    /// Open a 20-line buffer with a deleted-line block anchored above buffer row
    /// 1, so display rows sit below their buffer rows for every row past the
    /// block. The harness comes back with a 10-row viewport and deleted blocks
    /// shown. A deletion diff is the cache-coherent way to add block rows in a
    /// test (a `diff_version` bump forces the snapshot rebuild).
    fn open_with_deleted_block(h: &mut TestHarness) {
        let body: String = (0..20).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("diff.rs", &body);
        h.open_file(&path);

        let dm = {
            let mut dm = DiffMap::default();
            dm.set_base_text(Arc::new("a\nb\nc\n".to_string()));
            dm.push_hunk(DiffHunk {
                status: DiffHunkStatus::Deleted,
                unstaged_lines: std::iter::once(1..1).collect(),
                marked_rows: Vec::new(),
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
        editor.display_map.set_show_deleted_blocks(true);
    }

    /// The focused editor's block cursor as a display row, mapped through the
    /// display map so inserted block rows are accounted for.
    fn focused_cursor_display_row(h: &mut TestHarness) -> u32 {
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail = buffer_snapshot.resolve_anchor(&sel.tail());
        let head = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor = cursor_offset(buffer_snapshot.rope(), tail, head);
        let point = buffer_snapshot.rope().offset_to_point(cursor);
        snapshot.buffer_to_display(point).row
    }

    /// Block rows -- review chunk headers or deleted-line blocks -- add display
    /// rows the buffer does not have. The scroll bound must count them, and the
    /// cursor-follow must reach the last display row, or the last content sits
    /// below a false bottom.
    #[test]
    fn scroll_bound_reaches_last_row_past_block_rows() {
        let mut h = TestHarness::with_size(40, 12);
        open_with_deleted_block(&mut h);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
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

        movement::set_cursor_row(editor, buffer_rows - 1);
        editor.scroll_row = 0;
        ensure_cursor_in_view(editor, 0);
        // The cursor is zero-width on the empty final row, so it reaches it and
        // the view scrolls all the way to the bound.
        assert_eq!(
            editor.scroll_row,
            line_count - 10,
            "following the cursor to the final row reaches the display bound",
        );
    }

    #[test]
    fn set_cursor_row_anchors_display_row_past_a_block() {
        let mut h = TestHarness::with_size(40, 12);
        open_with_deleted_block(&mut h);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        movement::set_cursor_row(editor, 12);
        let scroll = editor.scroll_row;

        let cursor_row = focused_cursor_display_row(&mut h);
        assert!(
            cursor_row > 12,
            "the block shifts buffer row 12 to a lower display row",
        );
        assert_eq!(
            cursor_row - scroll,
            2,
            "the cursor's display row keeps the intended 2-row top margin",
        );
    }

    /// A downward step onto a block row carries on downward, landing on the
    /// buffer row below the block rather than the one above it.
    ///
    /// A block row holds no text, so a position on it resolves to the nearest
    /// row that does. Searching left from a row a `j` just reached walks back to
    /// the row the motion started from, which leaves the cursor where it was.
    /// The direction of travel is what picks the far side of the block.
    #[test]
    fn a_downward_step_onto_a_block_row_lands_below_it() {
        let mut h = TestHarness::with_size(40, 12);
        open_with_deleted_block(&mut h);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            place_cursor(editor, 0, 0);
        }

        movement::move_vertical(&mut h.stoat, 1, false);

        assert_eq!(
            focused_cursor_point(&mut h.stoat),
            Point::new(1, 0),
            "j crosses the block to the buffer row under it",
        );
    }

    #[test]
    fn page_motion_pins_cursor_to_its_screen_row_past_a_block() {
        let mut h = TestHarness::with_size(40, 12);
        open_with_deleted_block(&mut h);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            place_cursor(editor, 0, 0);
            editor.scroll_row = 0;
        }

        page_motion(&mut h.stoat, PageDir::Down, true);

        let cursor_row = focused_cursor_display_row(&mut h);
        let scroll = focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_row;
        assert!(cursor_row > 0, "the half-page jump moves the cursor down");
        assert_eq!(
            cursor_row, scroll,
            "the cursor stays pinned to the screen row it started on",
        );
    }

    #[test]
    fn align_view_top_uses_cursor_display_row_past_a_block() {
        let mut h = TestHarness::with_size(40, 12);
        open_with_deleted_block(&mut h);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            place_cursor(editor, 8, 0);
        }

        align_view(&mut h.stoat, ViewAlign::Top);

        let cursor_row = focused_cursor_display_row(&mut h);
        let scroll = focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .scroll_row;
        assert!(
            cursor_row > 8,
            "the block shifts buffer row 8 below display row 8",
        );
        assert_eq!(
            scroll, cursor_row,
            "align Top puts the cursor's display row at the viewport top",
        );
    }

    #[test]
    fn goto_window_top_lands_on_display_top_line_past_a_block() {
        let mut h = TestHarness::with_size(40, 12);
        open_with_deleted_block(&mut h);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_row = 11;
        }

        let expected_row = {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            let snapshot = editor.display_map.snapshot();
            snapshot
                .display_to_buffer(DisplayPoint::new(11, 0))
                .expect("a buffer line renders at the viewport top")
                .row
        };

        goto_window(&mut h.stoat, WindowAlign::Top, false);

        assert!(
            expected_row < 11,
            "display row 11 maps to an earlier buffer row past the block",
        );
        assert_eq!(
            focused_cursor_point(&mut h.stoat).row,
            expected_row,
            "goto Top lands on the buffer line rendered at the viewport top",
        );
    }

    fn open_hundred_lines(h: &mut TestHarness) {
        let body: String = (0..100).map(|i| format!("line {i:02}\n")).collect();
        let path = h.write_file("long.rs", &body);
        h.open_file(&path);
    }

    #[test]
    fn clamp_cursor_to_view_pulls_above_band_cursor_to_the_top() {
        let mut h = TestHarness::with_size(40, 12);
        open_hundred_lines(&mut h);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            place_cursor(editor, 0, 5);
            editor.scroll_row = 40;
            // band = [40+3, 40+9-3] = [43, 46]; the row-0 cursor sits above it.
            assert!(
                clamp_cursor_to_view(editor, 3),
                "an above-band cursor moves"
            );
            assert_eq!(editor.scroll_row, 40, "the view is left untouched");
        }
        assert_eq!(
            focused_cursor_point(&mut h.stoat),
            Point::new(43, 5),
            "the cursor lands on the band top with its column preserved",
        );
    }

    #[test]
    fn clamp_cursor_to_view_is_a_noop_inside_the_band() {
        let mut h = TestHarness::with_size(40, 12);
        open_hundred_lines(&mut h);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            place_cursor(editor, 45, 5);
            editor.scroll_row = 40;
            assert!(
                !clamp_cursor_to_view(editor, 3),
                "an in-band cursor does not move and returns false"
            );
            assert_eq!(editor.scroll_row, 40);
        }
        assert_eq!(
            focused_cursor_point(&mut h.stoat),
            Point::new(45, 5),
            "the cursor is left where it was"
        );
    }

    #[test]
    fn clamp_cursor_to_view_extends_in_select_mode() {
        let mut h = TestHarness::with_size(40, 12);
        open_hundred_lines(&mut h);
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        editor.mode = "select".to_string();
        place_cursor(editor, 0, 0);
        editor.scroll_row = 40;
        assert!(clamp_cursor_to_view(editor, 3));

        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_row = buffer_snapshot
            .rope()
            .offset_to_point(buffer_snapshot.resolve_anchor(&sel.tail()))
            .row;
        let head_row = buffer_snapshot
            .rope()
            .offset_to_point(buffer_snapshot.resolve_anchor(&sel.head()))
            .row;
        assert_eq!(tail_row, 0, "select mode keeps the anchor at its origin");
        assert_eq!(head_row, 43, "the head extends down to the band top");
    }

    /// A scroll clamp leaves behind a goal column a later vertical motion can
    /// use, in the space that motion reads it in.
    ///
    /// The goal is cells counted along the whole buffer line, so a motion lands
    /// in the same place whatever wrap does to the rows in between. A clamp
    /// measuring it along one display row instead both moves the cursor to the
    /// wrong column itself and leaves that column behind as the goal, which the
    /// next motion then reads as a whole-line one.
    #[test]
    fn a_scroll_clamp_keeps_the_cursor_column_under_wrap() {
        // Lines long enough to wrap at this width, so a display row covers only
        // part of a buffer line and the two column spaces come apart.
        //
        // No spaces in them, since a wrap breaking at a word boundary leaves a
        // display row too narrow to hold the goal column. The rows this gives
        // are all wide enough, so nothing clips and the column is preserved
        // exactly.
        let body: String = (0..60).map(|_| format!("{}\n", "x".repeat(63))).collect();

        let mut h = TestHarness::with_size(40, 12);
        let path = h.write_file("wrapped.rs", &body);
        h.open_file(&path);

        // Cells along the display row, which is the unit a vertical motion's
        // goal is counted in. A buffer-line column is a different number under
        // wrap, and is not what the clamp undertakes to hold.
        let visual_column = |h: &mut TestHarness| {
            let point = focused_cursor_point(&mut h.stoat);
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor
                .display_map
                .snapshot()
                .buffer_to_display(point)
                .column
        };

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            place_cursor(editor, 0, 47);
        }
        assert_eq!(
            visual_column(&mut h),
            11,
            "the cursor starts part way along a wrapped row",
        );

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_row = 30;
            assert!(clamp_cursor_to_view(editor, 3), "the clamp fires");
            editor.scroll_row = 0;
        }
        assert_eq!(
            visual_column(&mut h),
            11,
            "the clamp moves the cursor down its rows, not across its columns",
        );

        dispatch(&mut h.stoat, &stoat_action::MoveDown);
        assert_eq!(
            visual_column(&mut h),
            11,
            "and the goal it left behind is the one a motion reads",
        );
    }

    #[test]
    fn clamp_cursor_to_view_clamps_column_to_a_short_edge_line() {
        let mut h = TestHarness::with_size(40, 12);
        let mut body = String::from("0123456789\n");
        body.push_str(&"ab\n".repeat(99));
        let path = h.write_file("mixed.rs", &body);
        h.open_file(&path);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            place_cursor(editor, 0, 6);
            editor.scroll_row = 40;
            assert!(clamp_cursor_to_view(editor, 3));
        }
        // The band-top row 43 is "ab" (length 2), so the goal column 6 clamps
        // onto the line's end rather than overrunning it.
        assert_eq!(
            focused_cursor_point(&mut h.stoat),
            Point::new(43, 2),
            "the goal column clamps to the short line's length",
        );
    }

    /// A scroll clamp lands its cursor on the same cell whether or not it is
    /// dragging an anchor behind it.
    ///
    /// The clamp places a cursor the way a vertical motion does, so select mode
    /// changes what the selection covers, never where the clamp puts its far
    /// end.
    #[test]
    fn clamp_cursor_to_view_extends_onto_the_cell_a_move_lands_on() {
        let cell_after = |select: bool| {
            let mut h = TestHarness::with_size(40, 12);
            let mut body = String::from("0123456789\n");
            body.push_str(&"ab\n".repeat(99));
            let path = h.write_file("mixed.rs", &body);
            h.open_file(&path);
            {
                let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
                editor.viewport_rows = Some(10);
                if select {
                    editor.mode = "select".to_string();
                }
                place_cursor(editor, 0, 6);
                editor.scroll_row = 40;
                assert!(clamp_cursor_to_view(editor, 3), "the clamp fires");
            }
            focused_cursor_point(&mut h.stoat)
        };

        assert_eq!(
            cell_after(true),
            cell_after(false),
            "extending to the band top covers the cell moving there lands on",
        );
    }

    #[test]
    fn ease_advances_toward_target_and_settles() {
        let mut offset = 0.0_f32;
        let target = 10.0_f32;
        let mut last_gap = f32::INFINITY;
        let mut settled = false;
        for _ in 0..1000 {
            let (next, done) = step_scroll_ease(offset, target, 0.016, 0.35);
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
        let (one_step, _) = step_scroll_ease(0.0, 10.0, 0.016, 0.35);
        let (half, _) = step_scroll_ease(0.0, 10.0, 0.008, 0.35);
        let (two_steps, _) = step_scroll_ease(half, 10.0, 0.008, 0.35);
        assert!(
            (one_step - two_steps).abs() < 0.01,
            "one 16ms ease {one_step} should equal two 8ms eases {two_steps}"
        );
    }

    /// An unfloored exponential tail moves under a pixel per frame for many
    /// frames, and the renderer quantizes that to whole pixels, so the stop
    /// paints as an irregular train of one-pixel steps.
    #[test]
    fn ease_floors_its_tail_step_and_lands_exactly() {
        // Mirrors step_scroll_ease's own MIN_STEP, in rows per NOMINAL_DT.
        const MIN_STEP: f32 = 0.075;
        const NOMINAL_DT: f32 = 0.008;
        const WHEEL_EASE: f32 = 0.13;

        // From twice the floor out the geometric step is the smaller of the two,
        // so the floor is what advances the offset.
        let (next, done) = step_scroll_ease(0.0, MIN_STEP * 2.0, NOMINAL_DT, WHEEL_EASE);
        assert!(
            (next - MIN_STEP).abs() < 1e-5,
            "the tail advances by the floor, not by {next}",
        );
        assert!(!done, "and is still easing");

        // The floor is a velocity, so a frame twice as long carries twice as far.
        let (next, _) = step_scroll_ease(0.0, MIN_STEP * 4.0, NOMINAL_DT * 2.0, WHEEL_EASE);
        assert!(
            (next - MIN_STEP * 2.0).abs() < 1e-5,
            "the floor scales with the frame time, giving {next}",
        );

        // Walking a whole tail keeps every step at the floor and lands on the
        // target rather than approaching it asymptotically.
        let target = 0.6_f32;
        let mut offset = 0.0_f32;
        let mut settled = false;
        for _ in 0..100 {
            let (next, done) = step_scroll_ease(offset, target, NOMINAL_DT, WHEEL_EASE);
            assert!(
                done || next - offset >= MIN_STEP - 1e-5,
                "step of {} fell under the floor",
                next - offset,
            );
            offset = next;
            if done {
                settled = true;
                break;
            }
        }
        assert!(settled, "the tail settles");
        assert_eq!(offset, target, "exactly on the target");
    }

    fn harness_with_long_buffer() -> TestHarness {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("glide.rs", &body);
        h.open_file(&path);
        h
    }

    /// Trackpad travel worth less than a line still moves the target, and the
    /// remainder rests between two rows rather than rounding to one.
    #[test]
    fn fractional_wheel_travel_rests_between_two_rows() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        // Three rows to a line, so a fifth of a line is 0.6 of a row.
        wheel_scroll_by(editor, 0.2);
        assert_eq!(
            (editor.scroll_row, editor.scroll_glide),
            (0, ScrollGlide::Wheel),
            "the first travel stays inside row zero and arms the glide",
        );
        assert!(
            (editor.scroll_frac - 0.6).abs() < 1e-5,
            "resting six tenths of a row down, got {}",
            editor.scroll_frac,
        );

        wheel_scroll_by(editor, 0.2);
        assert_eq!(editor.scroll_row, 1, "the second travel crosses a row");
        assert!(
            (editor.scroll_frac - 0.2).abs() < 1e-5,
            "and carries the remainder, got {}",
            editor.scroll_frac,
        );
    }

    /// The document bounds hold for fractional travel the way they do for a
    /// notch, so a trackpad cannot drift the target past either end.
    #[test]
    fn fractional_wheel_travel_clamps_at_both_ends() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        wheel_scroll_by(editor, -0.5);
        assert_eq!(
            (editor.scroll_row, editor.scroll_frac),
            (0, 0.0),
            "travel up from the top rests on the first row",
        );

        let max = max_scroll_offset(editor);
        wheel_scroll_by(editor, 1000.0);
        assert_eq!(
            editor.scroll_row as f32 + editor.scroll_frac,
            max,
            "and travel past the tail rests exactly on the bound",
        );
    }

    /// A notch is one line of travel, so the whole-row step every wheel report
    /// has always taken is unchanged.
    #[test]
    fn a_notch_still_steps_three_whole_rows() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        wheel_scroll(editor, true);
        assert_eq!((editor.scroll_row, editor.scroll_frac), (3, 0.0));
    }

    /// Only wheel travel rests unaligned. A keyboard motion after one lands on
    /// the row grid rather than inheriting the fraction.
    #[test]
    fn a_page_motion_clears_a_resting_fraction() {
        let mut h = harness_with_long_buffer();
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            wheel_scroll_by(editor, 0.2);
            assert_ne!(editor.scroll_frac, 0.0, "the wheel rests off the grid");
        }

        page_motion(&mut h.stoat, PageDir::Down, false);

        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert_eq!(
            editor.scroll_frac, 0.0,
            "the page motion lands on a whole row",
        );
    }

    #[test]
    fn wheel_scroll_advances_the_target_and_clamps_at_the_tail() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        wheel_scroll(editor, true);
        assert_eq!(
            editor.scroll_row, 3,
            "a down report advances the target three rows"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::Wheel,
            "and arms a wheel glide"
        );
        assert_eq!(
            editor.scroll_offset, 0.0,
            "the offset lags at the pre-report row for the tick to ease up"
        );

        wheel_scroll(editor, true);
        assert_eq!(
            editor.scroll_row, 6,
            "repeated reports accumulate the target"
        );

        let max = max_scroll_offset(editor) as u32;
        editor.scroll_row = max;
        wheel_scroll(editor, true);
        assert_eq!(
            editor.scroll_row, max,
            "a report cannot advance past the document tail"
        );

        wheel_scroll(editor, false);
        assert_eq!(
            editor.scroll_row,
            max - 3,
            "an up report retreats the target"
        );
    }

    #[test]
    fn wheel_scroll_leaves_the_cursor_anchored() {
        let mut h = harness_with_long_buffer();
        let before = focused_head_row(&mut h.stoat);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            wheel_scroll(editor, true);
            assert_eq!(
                editor.scroll_row, 3,
                "the report advances the view three rows"
            );
        }
        assert_eq!(
            focused_head_row(&mut h.stoat),
            before,
            "but the selection stays anchored to its line until the glide settles"
        );
    }

    #[test]
    fn wheel_scroll_reseeds_a_drifted_offset_only_off_glide() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);

        editor.scroll_row = 10;
        editor.scroll_offset = 0.0;
        wheel_scroll(editor, true);
        assert_eq!(
            editor.scroll_offset as u32, 10,
            "off-glide, a drifted offset reseeds from scroll_row before gliding"
        );

        editor.scroll_offset = 0.0;
        wheel_scroll(editor, true);
        assert_eq!(
            editor.scroll_offset, 0.0,
            "mid-glide the lagging offset is left alone"
        );
    }

    #[test]
    fn keyboard_scroll_syncs_offset_and_clears_glide() {
        let mut h = harness_with_long_buffer();
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");

        editor.scroll_glide = ScrollGlide::Wheel;
        editor.scroll_offset = 4.2;
        assert!(
            scroll_editor(editor, true, 3),
            "scrolling down moves scroll_row"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::None,
            "keyboard scroll cancels the glide"
        );
        assert_eq!(
            editor.scroll_offset as u32, editor.scroll_row,
            "offset syncs to the integer row"
        );
    }

    fn set_focused_viewport_rows(stoat: &mut Stoat, rows: Option<u32>) {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        ws.editors[editor_id].viewport_rows = rows;
    }

    /// A cursor the scrolled view still clears is left where it is, so a page
    /// reads the file without dragging the edit point along.
    ///
    /// The last page of the buffer is where this shows, since only a clamped
    /// scroll travels less than the distance to the cursor.
    #[test]
    fn page_down_leaves_a_cursor_the_clamped_scroll_clears() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        for _ in 0..25 {
            dispatch(&mut stoat, &stoat_action::MoveDown);
        }
        // The view-follow runs in the key loop rather than the dispatch, so
        // drive it here to reach the resting view of a real press.
        ensure_cursor_in_view(focused_editor_mut(&mut stoat).expect("focused editor"), 3);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(25, 0)]);
        assert_eq!(editor::editor_scroll_rows(&stoat), vec![19]);

        // The scroll clamps at 21, two rows on, which leaves row 25 well below
        // the top edge of 24.
        dispatch(&mut stoat, &PageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(25, 0)],
            "the view moved and the cursor did not"
        );
        assert_eq!(editor::editor_scroll_rows(&stoat), vec![21]);
    }

    /// A page up at the top of the buffer moves nothing, since the pull only
    /// works against the direction the view travels.
    #[test]
    fn page_up_at_the_top_leaves_the_cursor_alone() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        for _ in 0..4 {
            dispatch(&mut stoat, &stoat_action::MoveDown);
        }
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(4, 0)]);

        dispatch(&mut stoat, &PageUp);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(4, 0)]);
        assert_eq!(editor::editor_scroll_rows(&stoat), vec![0]);
    }

    /// Half a page carries the cursor with it, which is the other family and
    /// the reason both still exist.
    #[test]
    fn half_page_down_carries_the_cursor() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));

        dispatch(&mut stoat, &HalfPageDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(5, 0)]);
        assert_eq!(editor::editor_scroll_rows(&stoat), vec![5]);
    }

    #[test]
    fn page_down_with_unrendered_editor_uses_default_viewport() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, None);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(14, 0)],
            "a page of viewport 20, scroll clamped to 11, cursor to the edge at 14"
        );
    }

    /// Half of a one-row viewport is no rows, so the key does nothing rather
    /// than rounding itself up to a whole row of travel.
    #[test]
    fn half_page_down_does_nothing_for_one_row_viewport() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "a\nb\nc\n");
        set_focused_viewport_rows(&mut stoat, Some(1));
        assert_eq!(
            dispatch(&mut stoat, &HalfPageDown),
            UpdateEffect::None,
            "a zero-row delta moves neither the view nor the cursor"
        );
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 0)]);
        assert_eq!(editor::editor_scroll_rows(&stoat), vec![0]);
    }

    /// An odd viewport splits into a short half and a long one, and the half
    /// page takes the short one.
    #[test]
    fn half_page_down_floors_an_odd_viewport() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(9));
        dispatch(&mut stoat, &HalfPageDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(4, 0)]);
        assert_eq!(editor::editor_scroll_rows(&stoat), vec![4]);
    }

    #[test]
    fn half_page_down_extends_the_selection_in_select_mode() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "a\nb\nc\nd\ne\nf\n");
        set_focused_viewport_rows(&mut stoat, Some(4));
        stoat.set_focused_mode("select".into());
        dispatch(&mut stoat, &HalfPageDown);
        // Half of viewport 4 is two rows, so the anchor holds at the top and
        // the head extends down to row 2 rather than collapsing there.
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 5, false)]);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(2, 0)],
            "the block cursor covers row 2's cell, not the newline before it",
        );
    }

    #[test]
    fn page_down_collapses_multi_cursors_to_one() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        dispatch(&mut stoat, &AddSelectionBelow);
        assert_eq!(editor::head_offsets(&mut stoat).len(), 2);
        dispatch(&mut stoat, &PageDown);
        // The view scrolls a page to row 10 and leaves both cursors above it,
        // so both are pulled down to the top edge and collapse to the same
        // target via the transform dedupe.
        assert_eq!(editor::head_offsets(&mut stoat).len(), 1);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(13, 0)]);
    }

    #[test]
    fn count_prefix_page_down_moves_n_pages() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        stoat.pending_count = Some(3);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(33, 0)],
            "three pages of scroll to row 30, and the cursor pulled to the edge at 33"
        );
    }

    #[test]
    fn page_down_arms_a_scroll_glide() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));

        dispatch(&mut stoat, &PageDown);

        let editor = focused_editor_mut(&mut stoat).expect("focused editor");
        assert_eq!(
            editor.scroll_row, 10,
            "PageDown jumps scroll_row a full viewport"
        );
        assert_eq!(
            editor.scroll_offset, 0.0,
            "scroll_offset lags at the pre-jump row so the pool eases up to it"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::Page,
            "a page glide is armed"
        );
    }

    #[test]
    fn count_prefix_half_page_down_moves_n_half_pages() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        stoat.pending_count = Some(3);
        dispatch(&mut stoat, &HalfPageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(15, 0)],
            "3 Ctrl-d with viewport=10 (half-page=5) should land at row 15"
        );
    }

    #[test]
    fn count_prefix_page_up_moves_n_pages() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        dispatch(&mut stoat, &PageDown);
        dispatch(&mut stoat, &PageDown);
        dispatch(&mut stoat, &PageDown);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(43, 0)],
            "test setup: four pages of scroll to row 40, cursor at the top edge"
        );
        stoat.pending_count = Some(3);
        dispatch(&mut stoat, &PageUp);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(16, 0)],
            "three pages back to row 10, and the cursor pulled to the bottom edge"
        );
    }

    #[test]
    fn count_prefix_page_down_clamps_at_buffer_end() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        stoat.pending_count = Some(99);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(24, 0)],
            "a huge count clamps the scroll to the last page, the cursor to its edge"
        );
    }

    fn page_scratch_content() -> String {
        (0..30).map(|i| format!("line{i:02}\n")).collect()
    }

    #[test]
    fn snapshot_page_down_scrolls_and_moves_cursor() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        h.assert_snapshot("snapshot_page_down_scrolls_and_moves_cursor");
    }

    #[test]
    fn snapshot_page_up_after_page_down_returns_to_top() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f ctrl-b");
        h.assert_snapshot("snapshot_page_up_after_page_down_returns_to_top");
    }

    #[test]
    fn snapshot_half_page_down() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-d");
        h.assert_snapshot("snapshot_half_page_down");
    }

    #[test]
    fn snapshot_half_page_up_from_bottom() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f ctrl-f ctrl-u");
        h.assert_snapshot("snapshot_half_page_up_from_bottom");
    }

    #[test]
    fn snapshot_page_down_clamps_at_last_line() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        h.type_keys("ctrl-f");
        h.assert_snapshot("snapshot_page_down_clamps_at_last_line");
    }

    #[test]
    fn snapshot_page_up_at_top_is_noop() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-b");
        h.assert_snapshot("snapshot_page_up_at_top_is_noop");
    }

    #[test]
    fn goto_window_top_after_scroll_lands_at_scroll_row() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows();
        let scroll_row = scroll_before[0];
        dispatch(&mut h.stoat, &stoat_action::GotoWindowTop);
        let positions = h.cursor_display_positions();
        assert_eq!(positions, vec![(scroll_row, 0)]);
        assert_eq!(h.editor_scroll_rows(), scroll_before);
    }

    #[test]
    fn goto_window_center_lands_at_viewport_midpoint() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows();
        let scroll_row = scroll_before[0];
        dispatch(&mut h.stoat, &stoat_action::GotoWindowCenter);
        let positions = h.cursor_display_positions();
        assert!(positions[0].0 > scroll_row);
        assert_eq!(h.editor_scroll_rows(), scroll_before);
    }

    #[test]
    fn goto_window_bottom_lands_at_last_visible_row() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows();
        let scroll_row = scroll_before[0];
        dispatch(&mut h.stoat, &stoat_action::GotoWindowBottom);
        let positions = h.cursor_display_positions();
        assert!(
            positions[0].0 > scroll_row,
            "bottom row {} must be below scroll_row {}",
            positions[0].0,
            scroll_row
        );
        assert_eq!(h.editor_scroll_rows(), scroll_before);
    }

    #[test]
    fn goto_window_clamps_to_buffer_end() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        dispatch(&mut h.stoat, &stoat_action::GotoWindowBottom);
        let positions = h.cursor_display_positions();
        assert!(
            positions[0].0 <= 3,
            "cursor must clamp to last buffer row, got {}",
            positions[0].0
        );
    }

    #[test]
    fn align_view_top_scrolls_so_cursor_at_top() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let head_before = h.cursor_display_positions();
        dispatch(&mut h.stoat, &stoat_action::AlignViewTop);
        let scroll = h.editor_scroll_rows()[0];
        let head_after = h.cursor_display_positions();
        assert_eq!(
            scroll, head_before[0].0,
            "scroll_row should equal cursor row"
        );
        assert_eq!(head_after, head_before, "cursor row must not move");
    }

    #[test]
    fn align_view_center_puts_cursor_at_midpoint() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let head_before = h.cursor_display_positions();

        dispatch(&mut h.stoat, &stoat_action::AlignViewCenter);
        assert_eq!(
            h.editor_scroll_rows(),
            vec![8],
            "the cursor sits on row 12, four rows down a nine-row view"
        );
        assert_eq!(
            h.cursor_display_positions(),
            head_before,
            "cursor row must not move"
        );
    }

    /// Centering measures from the last visible row, so an even viewport
    /// leaves one more row below the cursor than above it.
    #[test]
    fn align_view_center_measures_from_the_last_row() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        for _ in 0..20 {
            dispatch(&mut stoat, &stoat_action::MoveDown);
        }

        dispatch(&mut stoat, &stoat_action::AlignViewCenter);
        assert_eq!(
            editor::editor_scroll_rows(&stoat),
            vec![16],
            "rows 16 through 25, with the cursor fourth of ten"
        );
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(20, 0)]);
    }

    /// An odd viewport has a true middle row, and the cursor lands on it.
    #[test]
    fn align_view_center_lands_on_the_middle_of_an_odd_viewport() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(9));
        for _ in 0..20 {
            dispatch(&mut stoat, &stoat_action::MoveDown);
        }

        dispatch(&mut stoat, &stoat_action::AlignViewCenter);
        assert_eq!(
            editor::editor_scroll_rows(&stoat),
            vec![16],
            "rows 16 through 24, with the cursor fifth of nine"
        );
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(20, 0)]);
    }

    #[test]
    fn align_view_bottom_puts_cursor_at_last_visible_row() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let head_before = h.cursor_display_positions();
        let cursor_row = head_before[0].0;
        dispatch(&mut h.stoat, &stoat_action::AlignViewBottom);
        let scroll = h.editor_scroll_rows()[0];
        let head_after = h.cursor_display_positions();
        assert!(
            scroll <= cursor_row,
            "scroll {scroll} should be at or above cursor {cursor_row}"
        );
        assert_eq!(head_after, head_before, "cursor row must not move");
    }

    #[test]
    fn align_view_clamps_to_max_scroll() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        dispatch(&mut h.stoat, &stoat_action::AlignViewBottom);
        let scroll = h.editor_scroll_rows()[0];
        assert_eq!(
            scroll, 0,
            "buffer shorter than viewport must clamp scroll_row to 0"
        );
    }

    #[test]
    fn scroll_down_increments_scroll_row() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        let head_before = h.cursor_display_positions();
        let scroll_before = h.editor_scroll_rows()[0];
        dispatch(&mut h.stoat, &stoat_action::ScrollDown);
        assert_eq!(h.editor_scroll_rows()[0], scroll_before + 1);
        assert_eq!(
            h.cursor_display_positions(),
            head_before,
            "cursor must not move"
        );
    }

    #[test]
    fn scroll_up_decrements_scroll_row() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows()[0];
        assert!(scroll_before > 0);
        let head_before = h.cursor_display_positions();
        dispatch(&mut h.stoat, &stoat_action::ScrollUp);
        assert_eq!(h.editor_scroll_rows()[0], scroll_before - 1);
        assert_eq!(
            h.cursor_display_positions(),
            head_before,
            "cursor must not move"
        );
    }

    #[test]
    fn scroll_up_at_top_is_noop() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        dispatch(&mut h.stoat, &stoat_action::ScrollUp);
        assert_eq!(h.editor_scroll_rows()[0], 0);
    }

    #[test]
    fn scroll_down_clamps_at_max_scroll() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        for _ in 0..5 {
            dispatch(&mut h.stoat, &stoat_action::ScrollDown);
        }
        assert_eq!(
            h.editor_scroll_rows()[0],
            0,
            "buffer shorter than viewport keeps scroll_row at 0"
        );
    }

    #[test]
    fn count_prefix_scroll_down_advances_n_rows() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        let scroll_before = h.editor_scroll_rows()[0];
        h.type_keys("3 z j");
        assert_eq!(h.editor_scroll_rows()[0], scroll_before + 3);
    }

    #[test]
    fn count_prefix_scroll_up_walks_back_n_rows() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("3 z j");
        let scroll_before = h.editor_scroll_rows()[0];
        assert!(scroll_before >= 3);
        h.type_keys("3 z k");
        assert_eq!(h.editor_scroll_rows()[0], scroll_before - 3);
    }

    #[test]
    fn count_prefix_scroll_down_clamps_at_max_scroll() {
        let mut h = TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("9 9 z j");
        let scroll = h.editor_scroll_rows()[0];
        let saturating = h.editor_scroll_rows()[0];
        h.type_keys("z j");
        assert_eq!(
            h.editor_scroll_rows()[0],
            saturating,
            "scroll_row should be at max_scroll after huge count; further scroll-down is a no-op (got {scroll} -> {})",
            h.editor_scroll_rows()[0]
        );
    }
}
