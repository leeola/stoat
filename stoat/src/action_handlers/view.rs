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
use stoat_text::{cursor_offset, Bias, Point, SelectionGoal};

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
    let extend = stoat.focused_mode() == "select";
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
    let max_point = display_snapshot.max_point();

    let sel = editor.selections.newest_anchor().clone();
    let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
    let head_off = buffer_snapshot.resolve_anchor(&sel.head());
    let cursor = cursor_offset(rope, tail_off, head_off);
    let current = display_snapshot.buffer_to_display(rope.offset_to_point(cursor));

    // Move the cursor in display rows so it tracks the scroll leg below one for
    // one across block rows. A downward jump past the last display row lands on
    // the final cell, since the empty buffer row beyond it has no display row of
    // its own. Otherwise clip toward the direction of travel so a block row
    // snaps to the buffer row past it rather than back to the one just left.
    let target_point = match dir {
        PageDir::Up => {
            let raw = DisplayPoint::new(current.row.saturating_sub(delta), current.column);
            display_snapshot.clip_point(raw, Bias::Left)
        },
        PageDir::Down => {
            let row = current.row.saturating_add(delta);
            if row >= max_point.row {
                max_point
            } else {
                display_snapshot.clip_point(DisplayPoint::new(row, current.column), Bias::Right)
            }
        },
    };
    if target_point == current {
        return UpdateEffect::None;
    }
    let Some(target_buffer_pt) = display_snapshot.display_to_buffer(target_point) else {
        return UpdateEffect::None;
    };
    let target_offset = rope.point_to_offset(target_buffer_pt);

    let prev = editor.scroll_row;
    let max_scroll = max_scroll_row(display_snapshot.line_count(), viewport);
    editor.scroll_row = match dir {
        PageDir::Up => editor.scroll_row.saturating_sub(delta),
        PageDir::Down => editor.scroll_row.saturating_add(delta).min(max_scroll),
    };
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
    editor.scroll_offset = new_scroll as f32;
    editor.scroll_glide = ScrollGlide::None;
    true
}

/// Advance `editor`'s wheel scroll one report, down when `down` and up
/// otherwise, arming a [`ScrollGlide::Wheel`] ease toward the new target.
///
/// Each report moves `scroll_row` a fixed three rows -- matching the scrollback
/// and run-pane wheel steps -- toward the document bound, and the tick eases
/// `scroll_offset` up to it, so steady wheel input yields steady speed. The
/// selection stays anchored to its buffer line while the glide moves fast,
/// sliding out of view with the content. Once the glide slows below its re-home
/// velocity the cursor lands in the scrolloff band, ahead of the settle.
///
/// Reseeds `scroll_offset` from `scroll_row` only when no glide is in flight and
/// another path moved the integer row out from under the fraction. Mid-glide the
/// offset legitimately lags the target, so it must not be reseeded.
pub(crate) fn wheel_scroll(editor: &mut EditorState, down: bool) {
    const STEP: u32 = 3;

    let max_scroll = max_scroll_offset(editor) as u32;
    let target = if down {
        editor.scroll_row.saturating_add(STEP).min(max_scroll)
    } else {
        editor.scroll_row.saturating_sub(STEP)
    };
    if target == editor.scroll_row && editor.scroll_glide == ScrollGlide::None {
        return;
    }

    if editor.scroll_glide == ScrollGlide::None
        && editor.scroll_offset.floor() as u32 != editor.scroll_row
    {
        editor.scroll_offset = editor.scroll_row as f32;
    }
    editor.scroll_row = target;
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
pub(crate) fn step_scroll_ease(
    offset: f32,
    target: f32,
    dt: f32,
    ease_per_nominal: f32,
) -> (f32, bool) {
    const NOMINAL_DT: f32 = 0.008;
    const EPSILON: f32 = 0.01;

    let kept = (1.0 - ease_per_nominal).powf(dt / NOMINAL_DT);
    let next = target - (target - offset) * kept;
    if (target - next).abs() < EPSILON {
        (target, true)
    } else {
        (next, false)
    }
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

    let (target_row, clip_bias) = if cursor_row < band_top {
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
        // Cells along the whole buffer line, which is what a vertical motion
        // carries. Measuring along a display row would land the cursor elsewhere
        // under wrap and leave the wrong kind of goal for the next motion to
        // read.
        let goal_col = match read.goal {
            SelectionGoal::Column(c) => c,
            SelectionGoal::None => snapshot.visual_column(cursor_pt),
        };
        // The rows stay in display space, the band being one of display rows, so
        // the target row converts back to the buffer line whose cells the goal is
        // counted in.
        let new_row_i = (cursor_display.row as i64).saturating_add(row_delta);
        let new_row = new_row_i.clamp(0, max_row as i64) as u32;
        if new_row == cursor_display.row {
            return None;
        }
        let target_line = snapshot.display_to_buffer(DisplayPoint::new(new_row, 0))?;
        let col = snapshot.buffer_column_at_visual(target_line.row, goal_col, Bias::Left);
        let clipped = snapshot.clip_point(
            snapshot.buffer_to_display(Point::new(target_line.row, col)),
            clip_bias,
        );
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
        // The 1-wide cursor cannot sit on the empty final row, so it lands on
        // the last content line one row up, scrolling one short of the bound.
        assert_eq!(
            editor.scroll_row,
            line_count - 11,
            "following the cursor to the last content line nears the display bound",
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
        // part of a buffer line and the two column spaces come apart. Every line
        // is the same length, so a vertical move has no short line to clamp to
        // and the column is preserved exactly.
        let body: String = (0..60)
            .map(|i| format!("{i:02} {}\n", "x".repeat(60)))
            .collect();

        let mut h = TestHarness::with_size(40, 12);
        let path = h.write_file("wrapped.rs", &body);
        h.open_file(&path);

        let visual_column = |h: &mut TestHarness| {
            let point = focused_cursor_point(&mut h.stoat);
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.display_map.snapshot().visual_column(point)
        };

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.viewport_rows = Some(10);
            place_cursor(editor, 0, 40);
        }
        assert_eq!(visual_column(&mut h), 40, "the cursor starts mid-line");

        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_row = 30;
            assert!(clamp_cursor_to_view(editor, 3), "the clamp fires");
            editor.scroll_row = 0;
        }
        assert_eq!(
            visual_column(&mut h),
            40,
            "the clamp moves the cursor down its rows, not across its columns",
        );

        dispatch(&mut h.stoat, &stoat_action::MoveDown);
        assert_eq!(
            visual_column(&mut h),
            40,
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

    fn harness_with_long_buffer() -> TestHarness {
        let mut h = TestHarness::with_size(40, 12);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let path = h.write_file("glide.rs", &body);
        h.open_file(&path);
        h
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

    #[test]
    fn page_down_with_unrendered_editor_uses_default_viewport() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, None);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(20, 0)]);
    }

    #[test]
    fn half_page_down_rounds_up_for_one_row_viewport() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "a\nb\nc\n");
        set_focused_viewport_rows(&mut stoat, Some(1));
        dispatch(&mut stoat, &HalfPageDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 0)]);
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
        // AddSelectionBelow makes row 1 the primary cursor; PageDown from
        // row 1 with viewport=10 lands on row 11. Both cursors collapse to
        // the same target via the transform dedupe.
        assert_eq!(editor::head_offsets(&mut stoat).len(), 1);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(11, 0)]);
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
            vec![(30, 0)],
            "3 Ctrl-f with viewport=10 should land at row 30"
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
            vec![(40, 0)],
            "test setup: cursor at row 40 after four page-downs"
        );
        stoat.pending_count = Some(3);
        dispatch(&mut stoat, &PageUp);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(10, 0)],
            "3 Ctrl-b from row 40 with viewport=10 should land at row 10"
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
            vec![(29, 6)],
            "huge count clamps the 1-wide cursor onto the last real cell, not the empty final row"
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
        let cursor_row = head_before[0].0;
        dispatch(&mut h.stoat, &stoat_action::AlignViewCenter);
        let scroll = h.editor_scroll_rows()[0];
        let head_after = h.cursor_display_positions();
        assert!(
            scroll < cursor_row,
            "scroll {scroll} should be above cursor {cursor_row}"
        );
        assert!(
            cursor_row - scroll <= 5,
            "cursor at row {cursor_row}, scroll {scroll}: viewport midpoint should be roughly half a viewport up"
        );
        assert_eq!(head_after, head_before, "cursor row must not move");
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
