//! The handlers of a [`View::DiffChanges`] pane.
//!
//! They open and close the changes view, flip between it and the per-file diff,
//! move its cursor through the stack, carry the change walk across its
//! excerpts, and respawn a pane whose state did not survive a restore.

use super::{
    conflict_view, jump,
    movement::{self, ChangeDir, ChangeUnit},
    pane, review,
    view::{self, PageDir, ViewAlign},
};
use crate::{
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    buffer_registry::{BufferRegistry, OpenOrigin},
    diff_changes::{self, DiffChangesState, ExpandDir, Landing, ScopeParse},
    display_map::{DisplayPoint, DisplaySnapshot},
    editor_state::{EditorId, EditorState},
    pane::{PaneId, View},
    workspace::{diff::DiffLanding, Workspace},
};
use slotmap::SlotMap;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_scheduler::Executor;
use stoat_text::{cursor_offset, Anchor, Bias, BufferId, Point, Selection, SelectionGoal};

/// What a press on a changes view's stack did.
///
/// A middle or right press acts from the cursor, so it acts only after a press
/// that placed the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StackPress {
    /// The press landed on no row.
    Missed,
    /// The press opened a fold through one of its Fold Buttons and left the
    /// cursor where it was.
    Opened,
    /// The press folded a file to its header or opened it.
    Folded,
    /// The press put the cursor on the pressed cell.
    Placed,
}

/// Open a changes view over the focused view, or close the focused one.
///
/// On a focused changes view this closes it and lands the pane on the cursor's
/// file as a plain editor, also when the view opened from the per-file diff.
/// The action turns the changes view off and does not go back a layer.
/// [`close_diff_changes`], which `q` runs, is the close that returns to a
/// covered diff.
///
/// Anywhere else it shows a changes view over the focused view. An open
/// conflict view closes first, so the changes view covers the plain file that
/// the conflict view covered. `DiffFiles` opens the per-file diff from a
/// changes view.
pub(super) fn toggle_diff_changes(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().conflict.is_some() {
        conflict_view::close_conflict(stoat);
    }
    if stoat.active_workspace().focused_diff_changes_id().is_some() {
        return close_changes_view(stoat, false);
    }
    open_diff_changes(stoat)
}

/// Close the changes view the focused pane shows and land the pane on the
/// focused excerpt's file, at the excerpt's cursor with its selections.
///
/// The cursor keeps the screen row it held in the stack, as a flip to the
/// per-file diff does. A changes view opened from a per-file diff returns to
/// that diff on the landed file, so a close peels back one layer. A backward
/// jump and `g a` return to the file the changes view covered.
///
/// The excerpts close with it, and so does each buffer only the stack opened,
/// except the one the pane lands on. With no excerpt in focus that shows rows
/// the close puts back the view the changes view covered. A covered view that
/// is gone closes the pane instead, and a last pane gets a fresh scratch
/// editor.
pub(crate) fn close_diff_changes(stoat: &mut Stoat) -> UpdateEffect {
    close_changes_view(stoat, true)
}

/// Close the focused changes view as [`close_diff_changes`] does.
///
/// With `to_covered_diff` false, a changes view opened from a per-file diff
/// lands the pane in a plain editor and does not return to the diff.
pub(super) fn close_changes_view(stoat: &mut Stoat, to_covered_diff: bool) -> UpdateEffect {
    if stoat.active_workspace().focused_diff_changes_id().is_none() {
        return UpdateEffect::None;
    }
    let landing = focused_target_selections(stoat);
    let screen_row = landing.is_some().then(|| stack_screen_row(stoat)).flatten();

    let ws = stoat.active_workspace_mut();
    let focus = ws.panes.focus();
    let Some(mut state) = take_down(ws, focus) else {
        return UpdateEffect::None;
    };
    let excerpts = state.drain_excerpts();
    let (covered, covered_diff) = (state.covered, state.covered_diff && to_covered_diff);
    match landing {
        Some(landing) => {
            if put_back_covered(stoat, focus, covered, false) {
                land_on_excerpt(stoat, focus, landing, covered_diff, screen_row);
            }
            // After the landing, so the pane's editor holds the landed buffer
            // and the release leaves it open.
            diff_changes::release_excerpts(stoat, excerpts);
        },
        None => {
            diff_changes::release_excerpts(stoat, excerpts);
            put_back_covered(stoat, focus, covered, covered_diff);
        },
    }
    tracing::info!(target: "stoat::review", "changes view closed");
    UpdateEffect::Redraw
}

/// Show `landing`'s buffer in pane `focus` with its selections, at
/// `screen_row` when the cursor had one in the stack.
///
/// A pane that shows the buffer keeps its editor. Another buffer replaces the
/// pane's editor with a jump recorded. A pane that shows no editor keeps its
/// view. With `covered_diff` the pane re-enters the per-file diff on the landed
/// buffer.
fn land_on_excerpt(
    stoat: &mut Stoat,
    focus: PaneId,
    (buffer_id, selections): (BufferId, Arc<[Selection<Anchor>]>),
    covered_diff: bool,
    screen_row: Option<u32>,
) {
    let workspace = stoat.active_workspace;
    let ws = stoat.active_workspace();
    let View::Editor(editor_id) = ws.panes.pane(focus).view else {
        return;
    };
    let shown = ws.editors.get(editor_id).map(|editor| editor.buffer_id);
    if shown != Some(buffer_id) {
        let Some(buffer) = ws.buffers.get(buffer_id) else {
            return;
        };
        jump::record_pane_switch(stoat, workspace, focus, buffer_id);
        let executor = stoat.executor.clone();
        buffer_lifecycle::show_buffer_in_pane(stoat, workspace, focus, buffer_id, buffer, executor);
    }

    if let Some(editor) = super::focused_editor_mut(stoat) {
        editor.selections.restore(selections);
    }
    if covered_diff {
        review::enter_diff_view(stoat);
    }
    if let Some(row) = screen_row {
        hold_screen_row(stoat, row);
    } else {
        let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
        if let Some(editor) = super::focused_editor_mut(stoat) {
            editor.pending_center = Some(scrolloff);
        }
    }
}

/// Take down the changes view the focused pane shows and park its stack, so
/// the next changes view opens on the same excerpts.
///
/// A flip to the per-file diff takes this path, and the flip back finds every
/// excerpt with its editor and buffer, so no file reopens. One stack parks at
/// a time, so a stack already parked releases as a close releases it. The
/// pane gets back the view the changes view covered, in normal mode.
fn park_diff_changes(stoat: &mut Stoat) {
    let ws = stoat.active_workspace_mut();
    let focus = ws.panes.focus();
    let Some(mut state) = take_down(ws, focus) else {
        return;
    };
    let covered = state.covered.take();
    state.covered_diff = false;
    state.pending_landing = None;
    state.held_screen_row = None;
    if let Some(mut displaced) = ws.parked_diff_changes.replace(state) {
        diff_changes::release_excerpts(stoat, displaced.drain_excerpts());
    }
    put_back_covered(stoat, focus, covered, false);
    tracing::info!(
        target: "stoat::review",
        "changes view parked for the per-file diff"
    );
}

/// Show `covered` in pane `focus` once its changes view is gone, in normal
/// mode, and answer whether the pane still stands.
///
/// A covered view that is gone closes the pane instead, and a last pane gets a
/// fresh scratch editor. With `covered_diff`, a covered editor returns to the
/// per-file diff it showed.
fn put_back_covered(
    stoat: &mut Stoat,
    focus: PaneId,
    covered: Option<View>,
    covered_diff: bool,
) -> bool {
    let ws = stoat.active_workspace();
    let live = pane::view_is_live(ws, covered.as_ref());
    let rediff = live && covered_diff && matches!(covered, Some(View::Editor(_)));
    let stands = if live {
        pane::show_view_or_scratch(stoat, focus, covered);
        true
    } else if !pane::close_pane_by_id(stoat, focus) {
        pane::show_view_or_scratch(stoat, focus, None);
        true
    } else {
        false
    };
    stoat.set_focused_mode("normal".to_string());
    if rediff {
        review::enter_diff_view(stoat);
    }
    stands
}

/// Take down the changes view that pane `pane_id` of `ws` shows, before a
/// buffer opens in the pane, and put back the view it covered.
///
/// The buffer then opens as a file over the covered view, which comes back in
/// normal mode. A covered view that is gone leaves a scratch editor for the
/// buffer's editor to replace, so the pane never closes under the open.
///
/// The excerpt editors go at once, and each buffer only the stack opened waits
/// for [`diff_changes::pump_diff_changes_stack`] to close it. A jump to an
/// excerpt's own file therefore finds that buffer still open, and the pump
/// keeps the buffer once the jump's editor holds it.
pub(crate) fn close_diff_changes_for_open(
    ws: &mut Workspace,
    executor: &Executor,
    pane_id: PaneId,
) {
    let Some(mut state) = take_down(ws, pane_id) else {
        return;
    };
    diff_changes::dispose_excerpts(ws, state.drain_excerpts());
    let view = state
        .covered
        .filter(|view| pane::view_is_live(ws, Some(view)))
        .unwrap_or_else(|| pane::scratch_editor_view(ws, executor.clone()));
    if let View::Editor(editor_id) = view
        && let Some(editor) = ws.editors.get_mut(editor_id)
    {
        editor.mode = "normal".to_string();
        editor.pinned = false;
    }
    ws.panes.pane_mut(pane_id).view = view;
    tracing::info!(target: "stoat::review", "changes view closed for a file open");
}

/// Park the changes view and open its focused excerpt's file in the per-file
/// diff, at the excerpt's cursor.
///
/// The file opens in the pane after the changes view comes down and puts back
/// the view it covered, over the excerpt's own buffer, so an edit made in the
/// excerpt and its undo history carry over. The stack parks rather than
/// closing, so the flip back reuses its excerpts.
///
/// A stack with no excerpt in focus that shows rows leaves the changes view
/// open, and [`no_target_status`] says why in the status row.
///
/// The cursor's line keeps the screen row it held in the stack, where the file
/// has rows enough above and below it. A cursor off the stack's painted rows
/// lands as a jump does, centered against the rows the diff paints.
pub(super) fn diff_changes_open(stoat: &mut Stoat) -> UpdateEffect {
    let Some(((rel_path, point), workdir)) = focused_target(stoat).zip(repo_workdir(stoat)) else {
        let status = no_target_status(stoat.active_workspace());
        stoat.set_status(status);
        return UpdateEffect::Redraw;
    };
    tracing::info!(
        target: "stoat::review",
        path = %rel_path,
        "changes view flips to the per-file diff"
    );
    let screen_row = stack_screen_row(stoat);

    if let Some(state) = stoat.active_workspace_mut().focused_diff_changes_mut() {
        let focused = state.focused;
        if let Some(excerpt) = state.excerpts.get_mut(focused) {
            excerpt.owns_buffer = false;
        }
    }
    park_diff_changes(stoat);

    let focus = stoat.active_workspace().panes.focus();
    let path = workdir.join(rel_path);
    if buffer_lifecycle::open_file_in_pane_now(stoat, focus, &path, OpenOrigin::Visited).is_none() {
        return UpdateEffect::Redraw;
    }

    let offset = super::focused_editor_mut(stoat).map(|editor| {
        let buffer = editor.display_map.buffer_snapshot();
        let rope = buffer.rope();
        let row = point.row.min(rope.max_point().row);
        rope.point_to_offset(Point::new(row, point.column.min(rope.line_len(row))))
    });
    if let Some(offset) = offset {
        movement::jump_to_offset(stoat, offset);
    }
    review::enter_diff_view(stoat);
    if let Some(row) = screen_row {
        hold_screen_row(stoat, row);
    } else {
        let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
        if let Some(editor) = super::focused_editor_mut(stoat) {
            editor.pending_center = Some(scrolloff);
        }
    }
    UpdateEffect::Redraw
}

/// Close the focused changes view for the per-file diff of the view it
/// covered, and answer whether it closed.
///
/// A folded excerpt in focus keeps the changes view open and answers false,
/// so the caller flips to that excerpt's diff through [`diff_changes_open`].
/// With none, the excerpts release and do not park, because the stack holds
/// no folded excerpt worth keeping. The covered view then comes back plain,
/// in normal mode.
pub(super) fn close_for_covered_diff(stoat: &mut Stoat) -> bool {
    if focused_target(stoat).is_some() {
        return false;
    }
    close_changes_view(stoat, false);
    true
}

/// Hold the focused editor's cursor on `screen_row` of the diff it opened.
///
/// The next paint applies the hold. A file whose hunks are on the pool gets the
/// hold again when they land, since the deleted-line blocks they bring move the
/// cursor's display row.
fn hold_screen_row(stoat: &mut Stoat, screen_row: u32) {
    let Some((editor, buffer_id)) = stoat.focused_editor_ids() else {
        return;
    };
    let ws = stoat.active_workspace_mut();
    if let Some(state) = ws.editors.get_mut(editor) {
        state.held_screen_row = Some(screen_row);
    }
    ws.add_diff_landing(buffer_id, DiffLanding::HeldRow { editor, screen_row });
}

/// Move the focused changes view's cursor by `delta` display rows.
///
/// A step past an excerpt's last row goes on to the first row of the next
/// excerpt, and a step before its first row to the last row of the previous
/// one, so the steps walk the rows the stack paints. An excerpt that waits on
/// its diff map paints no row, so a step from it holds still and a crossing
/// passes over it. At either end of the stack the cursor stays.
///
/// A crossing in insert mode carries the insert session into the entered file.
pub fn diff_changes_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let down = delta > 0;
    let mut effect = UpdateEffect::None;
    for _ in 0..delta.unsigned_abs() {
        let Some(before) = step_place(stoat) else {
            break;
        };
        if movement::move_vertical(stoat, delta.signum(), false) == UpdateEffect::Redraw {
            effect = UpdateEffect::Redraw;
        }
        if step_place(stoat) != Some(before) {
            continue;
        }

        if !cross_excerpt(stoat, down) {
            break;
        }
        effect = UpdateEffect::Redraw;
    }
    effect
}

/// Move the focused changes view's cursor by `delta` rows in one placement,
/// to where [`diff_changes_step`] lands.
///
/// A row counts when a cursor lands on it, so a removed base row takes no
/// step, and a step past an excerpt's edge goes on into the next painted
/// excerpt. Inside the focused excerpt every cursor moves and keeps its goal
/// column. A move into another excerpt leaves one cursor there, as the row
/// walk does. A count typed before the key takes no part.
pub fn diff_changes_jump(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return UpdateEffect::None;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return UpdateEffect::None;
    };
    let editor_id = match state.focused_excerpt() {
        Some(excerpt) if excerpt.shows_rows() => excerpt.editor_id,
        _ => return UpdateEffect::None,
    };
    let Some(editor) = editors.get_mut(editor_id) else {
        return UpdateEffect::None;
    };
    let (down, steps) = (delta > 0, delta.unsigned_abs());
    if steps == 0 {
        return UpdateEffect::None;
    }

    let start = view::cursor_display_row(editor);
    let taken = walk_rows(&editor.display_map.snapshot(), start, steps, down).0;
    if taken == steps {
        walk_cursors(editor, steps, down);
        return UpdateEffect::Redraw;
    }
    // The step that finds the excerpt's edge moves the cursors once more and
    // then crosses, as the row walk's step that finds no row does.
    walk_cursors(editor, taken + 1, down);
    cross_rows(state, editors, steps - taken - 1, down);
    UpdateEffect::Redraw
}

/// Carry the change walk past the focused excerpt's last hunk to the first
/// hunk of the next excerpt, or before its first hunk to the last hunk of the
/// previous one.
///
/// The hop lands as the per-file diff's hop into the next changed file lands,
/// with the origin on the jumplist and the landing recorded, so `DiffBack` and
/// `DiffForward` go on walking changes. An excerpt whose diff map holds no hunk
/// is passed over. At either end of the stack the cursor stays, and the status
/// row says that no more changes lie that way.
pub(super) fn hop_excerpt(stoat: &mut Stoat, dir: ChangeDir, unit: ChangeUnit) -> UpdateEffect {
    let origin = super::focused_editor_mut(stoat).map(jump::live_entry);
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return UpdateEffect::None;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return UpdateEffect::None;
    };

    let down = matches!(dir, ChangeDir::Next);
    let target = excerpts_past(state, down).into_iter().find_map(|index| {
        let editor = editors.get_mut(state.excerpts[index].editor_id)?;
        let starts = diff_changes::hunk_starts(editor);
        let start = match down {
            true => starts.first(),
            false => starts.last(),
        };
        start.map(|&start| (index, start))
    });
    let Some((index, start)) = target else {
        tracing::debug!(
            target: "stoat::change_walk",
            ?dir,
            from = ?state.focused_excerpt().map(|excerpt| &excerpt.rel_path),
            route = "no more changes",
            "excerpt hop"
        );
        stoat.set_status("no more changes");
        return UpdateEffect::Redraw;
    };
    tracing::debug!(
        target: "stoat::change_walk",
        ?dir,
        from = ?state.focused_excerpt().map(|excerpt| &excerpt.rel_path),
        to = %state.excerpts[index].rel_path,
        row = start,
        "excerpt hop"
    );
    state.focused = index;

    movement::land_file_hop(stoat, dir, unit, origin, false, (start, start..start + 1));
    UpdateEffect::Redraw
}

/// Move the focused excerpt's cursor by the rows of the stack's last paint.
pub(super) fn diff_changes_page(stoat: &mut Stoat, dir: PageDir) -> UpdateEffect {
    let rows = viewport_rows(stoat);
    diff_changes_jump(stoat, page_delta(rows, dir))
}

/// Move the focused excerpt's cursor by half the rows of the stack's last
/// paint, and by one row at least.
pub(super) fn diff_changes_half_page(stoat: &mut Stoat, dir: PageDir) -> UpdateEffect {
    let rows = (viewport_rows(stoat) / 2).max(1);
    diff_changes_jump(stoat, page_delta(rows, dir))
}

/// Put the focused changes view's cursor on the first excerpt's first row, or
/// on the last excerpt's last row when `last` is set.
pub(super) fn diff_changes_edge(stoat: &mut Stoat, last: bool) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return UpdateEffect::None;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return UpdateEffect::None;
    };
    let index = match last {
        true => state.excerpts.len().checked_sub(1),
        false => (!state.excerpts.is_empty()).then_some(0),
    };
    let Some(index) = index else {
        return UpdateEffect::None;
    };
    let Some(editor) = editors.get_mut(state.excerpts[index].editor_id) else {
        return UpdateEffect::None;
    };

    let kept = diff_changes::kept_rows(editor, &state.excerpts[index].revealed);
    let row = match last {
        true => kept.last().map_or(0, |run| run.end - 1),
        false => kept.first().map_or(0, |run| run.start),
    };
    diff_changes::place_cursor(editor, Point::new(row, 0));
    state.focused = index;
    UpdateEffect::Redraw
}

/// Act on a press at stack row `row` and screen column `column` of the
/// focused changes view.
///
/// A press on a File Header folds its file to the header, or opens a file that
/// shows its header alone. A press on a pinned header acts on the header it
/// repeats, and the stack scrolls back to that header. A fold of the focused
/// file moves the focus to the nearest file that shows rows.
///
/// A press on a fold row's Fold Button opens the fold from the button's edge,
/// on any excerpt of the stack. The cursor, the focus, and the stack's scroll
/// stay where they were, so the rows above the pressed row keep their screen
/// rows and the opened rows push the rest down.
///
/// Any other press puts the cursor on the pressed cell, and the row's excerpt
/// takes the focus, as a press in an editor does.
pub(crate) fn diff_changes_press(stoat: &mut Stoat, row: usize, column: u16) -> StackPress {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return StackPress::Missed;
    };
    let cache = ws.diff_tree_cache();
    let Workspace {
        diff_changes_views,
        editors,
        buffers,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return StackPress::Missed;
    };
    diff_changes::layout_stack(state, editors);
    let (row, pinned) = diff_changes::pinned_row(state, editors, row);
    if let Some(index) = state.excerpts.iter().position(|excerpt| excerpt.top == row) {
        diff_changes::toggle_file(state, editors, index);
        return StackPress::Folded;
    }

    // A pinned row draws no Fold Button, so a press there opens no fold.
    if !pinned
        && let Some((index, gap, edge)) = diff_changes::fold_button_at(state, editors, row, column)
    {
        let parse = ScopeParse::of(buffers, state.excerpts[index].buffer_id, &cache);
        diff_changes::expand(state, editors, parse.as_ref(), index, gap, edge);
        return StackPress::Opened;
    }
    if !diff_changes::press_cell(state, editors, row, column) {
        return StackPress::Missed;
    }
    // A press on a pinned Scope puts the cursor above the view. The paint
    // follows only a cursor that moved, and the cursor sometimes stood on that
    // row already, so the press scrolls the row into view itself.
    if let Some(cursor) = diff_changes::cursor_stack_row(state, editors) {
        state.ensure_row_visible(editors, cursor, state.viewport_rows);
    }
    StackPress::Placed
}

/// Scroll the focused changes view's stack by `delta` rows and leave its
/// cursor, as a wheel scrolls an editor.
///
/// The scroll stays between the stack's top and its last page. The cursor
/// stays off screen until it moves, since only a moved cursor pulls the stack
/// back.
pub fn diff_changes_scroll(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return UpdateEffect::None;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return UpdateEffect::None;
    };
    let total = diff_changes::layout_stack(state, editors);
    let scroll = state
        .scroll
        .saturating_add_signed(delta as isize)
        .min(total.saturating_sub(state.viewport_rows));
    if scroll == state.scroll {
        return UpdateEffect::None;
    }
    state.scroll = scroll;
    UpdateEffect::Redraw
}

/// Scroll the focused changes view's stack so its cursor's row sits at the
/// top, the middle, or the bottom of the view, as the view keys align a plain
/// editor's view.
///
/// The scroll stops at the stack's last page, and the cursor stays. The top
/// of the view is the first row under the pinned rows, so the cursor's row
/// never aligns under them.
pub(super) fn align_stack(stoat: &mut Stoat, align: ViewAlign) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return UpdateEffect::None;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return UpdateEffect::None;
    };
    let total = diff_changes::layout_stack(state, editors);
    let Some(row) = diff_changes::cursor_stack_row(state, editors) else {
        return UpdateEffect::None;
    };

    // Measured from the last row, as a plain editor's alignment measures, so
    // an even view centers one row above its middle.
    let last_row = state.viewport_rows.saturating_sub(1);
    let scroll = match align {
        ViewAlign::Top => row,
        ViewAlign::Center => row.saturating_sub(last_row / 2),
        ViewAlign::Bottom => row.saturating_sub(last_row),
    }
    .min(total.saturating_sub(state.viewport_rows));
    let before = state.scroll;
    state.scroll = scroll;
    state.uncover_row(editors, row);
    match state.scroll == before {
        true => UpdateEffect::None,
        false => UpdateEffect::Redraw,
    }
}

/// Open unchanged lines of a fold of the focused changes view as `dir` says,
/// or say in the status row that no fold lies that way.
pub(super) fn diff_changes_expand(stoat: &mut Stoat, dir: ExpandDir) -> UpdateEffect {
    if let Err(text) = diff_changes::expand_from_cursor(stoat, dir) {
        stoat.set_status(text);
    }
    UpdateEffect::Redraw
}

/// Fold again the rows that expands opened around the focused changes view's
/// cursor.
pub(super) fn diff_changes_collapse(stoat: &mut Stoat) -> UpdateEffect {
    match diff_changes::collapse_at_cursor(stoat) {
        true => UpdateEffect::Redraw,
        false => UpdateEffect::None,
    }
}

/// Give a fresh state to every changes view pane of the active workspace whose
/// state did not survive, then list each stack's files.
///
/// The panes of every tab count, a parked one included, so a tab shows a live
/// changes view when the reader next switches to it.
///
/// A changes view pane rides `PaneTree` serde as [`View::DiffChanges`], but its
/// state is live state the session file does not hold, so the id is dead after
/// a restore or a workspace copy. A fresh state covers no view, so its close
/// leaves a scratch editor.
pub fn respawn_diff_changes_panes(stoat: &mut Stoat) {
    let dead_panes = {
        let ws = stoat.active_workspace();
        ws.pane_trees()
            .enumerate()
            .flat_map(|(tree, panes)| {
                panes
                    .split_pane_ids()
                    .into_iter()
                    .filter_map(move |id| match panes.pane(id).view {
                        View::DiffChanges(dead) if !ws.diff_changes_views.contains_key(dead) => {
                            Some((tree, id))
                        },
                        _ => None,
                    })
            })
            .collect::<Vec<(usize, PaneId)>>()
    };
    if dead_panes.is_empty() {
        return;
    }

    // `pane_trees_mut` walks the trees in the order `pane_trees` does, so the
    // index collected above names the same tree here.
    let ws = stoat.active_workspace_mut();
    for (tree, pane_id) in dead_panes {
        let id = ws.diff_changes_views.insert(DiffChangesState::default());
        if let Some(panes) = ws.pane_trees_mut().nth(tree) {
            panes.pane_mut(pane_id).view = View::DiffChanges(id);
        }
    }
    diff_changes::pump_diff_changes_stack(stoat);
}

/// Show a changes view in the focused pane, over the view the pane showed.
///
/// A focused editor's cursor waits as the landing, so the changes view opens
/// on that position once its file's excerpt is folded. An editor in the diff
/// view leaves it first, so the editor comes back plain beneath the changes
/// view, and the close returns to the diff.
///
/// A parked stack opens again with its excerpts, and the first pump lists the
/// files anew against it, keeping the excerpts still listed.
///
/// An open re-reads the git state. It stales every diff map and the repository
/// tally, since a commit or a write that no watcher event reported otherwise
/// leaves the stack on the tree before it.
pub(super) fn open_diff_changes(stoat: &mut Stoat) -> UpdateEffect {
    // Before the landing's map is wanted below, so that want reads fresh blobs
    // rather than landing superseded and diffing a second time.
    let ws = stoat.active_workspace_mut();
    ws.invalidate_all_diffs();
    ws.diff_warmed = false;

    let landing = editor_landing(stoat);
    let (covered, covered_diff) = {
        let ws = stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let in_diff = match pane.view {
            View::Editor(id) => {
                pane.diff_mode || ws.editors.get(id).is_some_and(|editor| editor.diff_view)
            },
            _ => false,
        };
        (pane.view.clone(), in_diff)
    };
    review::exit_diff_view(stoat);
    // No pane shows the covered editor's buffer beneath the changes view, so
    // its diff map is wanted here. An unsaved edit then reaches the map, and
    // its file joins the list the landing waits on.
    if let Some(landing) = &landing {
        review::ensure_diff_map(stoat, landing.buffer_id, None);
    }

    let ws = stoat.active_workspace_mut();
    let focus = ws.panes.focus();
    let mut state = ws.parked_diff_changes.take().unwrap_or_default();
    state.covered = Some(covered);
    state.covered_diff = covered_diff;
    state.pending_landing = landing;
    let id = ws.diff_changes_views.insert(state);
    ws.panes.pane_mut(focus).view = View::DiffChanges(id);
    ws.panes.widen(focus);

    diff_changes::pump_diff_changes_stack(stoat);
    tracing::info!(target: "stoat::review", covered_diff, "changes view opened");
    UpdateEffect::Redraw
}

/// The focused editor's cursor as a landing, with the file relative to the
/// repository and the row of the view the cursor sits on.
///
/// `None` when no editor has focus, or when its buffer has no file inside the
/// repository.
fn editor_landing(stoat: &mut Stoat) -> Option<Landing> {
    let workdir = repo_workdir(stoat)?;
    let (_, buffer_id) = stoat.focused_editor_ids()?;
    let rel_path = {
        let path = stoat.active_workspace().buffers.path_for(buffer_id)?;
        path.strip_prefix(&workdir).ok()?.display().to_string()
    };
    let editor = super::focused_editor_mut(stoat)?;
    let point = diff_changes::cursor_point(editor);
    let screen_row = view::cursor_display_row(editor)
        .checked_sub(editor.scroll_row)
        .and_then(|row| u16::try_from(row).ok());
    Some(Landing {
        buffer_id,
        rel_path,
        row: point.row,
        column: point.column,
        screen_row,
    })
}

/// Move the focused changes view's cursor to the first hunk of `path`, and
/// report whether the focused changes view shows `path`.
///
/// A folded excerpt takes the cursor at once. An excerpt that waits on its
/// diff map takes it once it folds. The move pushes no jumplist entry, since
/// the changes view stays and its own hops push none.
pub(super) fn land_pick(stoat: &mut Stoat, path: &Path) -> bool {
    let Some(id) = stoat.active_workspace().focused_diff_changes_id() else {
        return false;
    };
    let Some(workdir) = repo_workdir(stoat) else {
        return false;
    };
    let Ok(relative) = path.strip_prefix(&workdir) else {
        return false;
    };
    let rel_path = relative.display().to_string();

    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = stoat.active_workspace_mut();
    let Some(state) = diff_changes_views.get_mut(id) else {
        return false;
    };
    let Some(excerpt) = state
        .excerpts
        .iter()
        .find(|excerpt| excerpt.rel_path == rel_path)
    else {
        return false;
    };
    let row = editors
        .get_mut(excerpt.editor_id)
        .and_then(|editor| diff_changes::hunk_starts(editor).first().copied())
        .unwrap_or(0);
    state.pending_landing = Some(Landing {
        buffer_id: excerpt.buffer_id,
        rel_path,
        row,
        column: 0,
        screen_row: None,
    });
    state.held_screen_row = None;

    diff_changes::pump_diff_changes_stack(stoat);
    tracing::info!(
        target: "stoat::picker",
        path = %path.display(),
        "file finder pick landed in the changes view"
    );
    true
}

/// Move the focus of the focused changes view to the excerpt that shows
/// `buffer_id`, and answer whether the view shows that buffer in a folded
/// excerpt.
///
/// The move places no cursor and pushes no jump. The caller places the cursor
/// through [`super::focused_editor_mut`], which then answers that excerpt's
/// editor. A file folded to its header opens, since the move lands on it.
pub(crate) fn focus_stack_buffer(stoat: &mut Stoat, buffer_id: BufferId) -> bool {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return false;
    };
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return false;
    };
    let Some(index) = state.excerpts.iter().position(|excerpt| {
        excerpt.buffer_id == buffer_id && excerpt.folded_version.is_some() && !excerpt.empty
    }) else {
        return false;
    };
    if state.excerpts[index].collapsed {
        state.excerpts[index].collapsed = false;
        diff_changes::layout_stack(state, editors);
    }
    if index != state.focused {
        state.focused = index;
        state.held_screen_row = None;
    }
    true
}

/// The focused excerpt's file, relative to the repository, and the buffer
/// position of its cursor.
///
/// `None` with no excerpt in focus that shows rows.
fn focused_target(stoat: &mut Stoat) -> Option<(String, Point)> {
    let ws = stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id()?;
    let excerpt = ws.diff_changes_views.get(id)?.focused_excerpt()?;
    if !excerpt.shows_rows() {
        return None;
    }
    let rel_path = excerpt.rel_path.clone();
    let editor = ws.editors.get_mut(excerpt.editor_id)?;
    Some((rel_path, diff_changes::cursor_point(editor)))
}

/// The focused excerpt's buffer and the selections of its editor, which a
/// close carries to the pane.
///
/// `None` with no excerpt in focus that shows rows.
fn focused_target_selections(stoat: &Stoat) -> Option<(BufferId, Arc<[Selection<Anchor>]>)> {
    let ws = stoat.active_workspace();
    let excerpt = ws.focused_diff_changes()?.focused_excerpt()?;
    if !excerpt.shows_rows() {
        return None;
    }
    let editor = ws.editors.get(excerpt.editor_id)?;
    Some((excerpt.buffer_id, editor.selections.shared_anchors()))
}

/// The status row's answer to a key that finds no excerpt in focus that shows
/// rows on the focused changes view.
///
/// The answer agrees with the pane. "no changes" needs an empty stack over a
/// landed repository tally, where the pane reads "no changes" too. Any other
/// stack is still loading. An empty pane reads "building changes..." until the
/// tally lands, and an excerpt shows no rows until its diff map folds.
pub(super) fn no_target_status(ws: &Workspace) -> &'static str {
    match ws.focused_diff_changes() {
        Some(state) if state.excerpts.is_empty() && !ws.tally_pending() => "no changes",
        _ => "changes still loading",
    }
}

/// The row of the view that the focused changes view's cursor sits on.
///
/// `None` when the cursor lies off the rows the stack's last paint showed.
fn stack_screen_row(stoat: &mut Stoat) -> Option<u32> {
    let ws = stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id()?;
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let state = diff_changes_views.get_mut(id)?;
    diff_changes::layout_stack(state, editors);
    let row = diff_changes::cursor_stack_row(state, editors)?;
    let screen_row = row
        .checked_sub(state.scroll)
        .filter(|&row| row < state.viewport_rows)?;
    u32::try_from(screen_row).ok()
}

/// The working directory of the repository that holds the active workspace.
///
/// The changes view names each file relative to it. The root resolves once
/// per root, so a call opens no repository while the root holds.
fn repo_workdir(stoat: &mut Stoat) -> Option<PathBuf> {
    stoat.sync_repo_root();
    stoat.active_workspace().repo_root.clone()
}

/// The rows of the focused changes view's last paint, zero before the first.
pub fn viewport_rows(stoat: &Stoat) -> usize {
    stoat
        .active_workspace()
        .focused_diff_changes()
        .map_or(0, |state| state.viewport_rows)
}

/// `rows` as a cursor step toward `dir`.
fn page_delta(rows: usize, dir: PageDir) -> i32 {
    let rows = i32::try_from(rows).unwrap_or(i32::MAX);
    match dir {
        PageDir::Down => rows,
        PageDir::Up => -rows,
    }
}

/// Remove the changes view that pane `pane_id` of `ws` shows, with the widen
/// it took, and answer its state.
///
/// `None` when the pane shows no changes view.
fn take_down(ws: &mut Workspace, pane_id: PaneId) -> Option<DiffChangesState> {
    let View::DiffChanges(id) = ws.panes.pane(pane_id).view else {
        return None;
    };
    let state = ws.diff_changes_views.remove(id)?;
    if ws.panes.widened() == Some(pane_id) {
        ws.panes.unwiden();
    }
    Some(state)
}

/// The focused excerpt's index and its cursor's display row.
///
/// `None` with no excerpt in focus, and while the focused excerpt waits on its
/// diff map.
fn step_place(stoat: &mut Stoat) -> Option<(usize, u32)> {
    let ws = stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id()?;
    let state = ws.diff_changes_views.get(id)?;
    let excerpt = state.focused_excerpt()?;
    if !excerpt.shows_rows() {
        return None;
    }
    let editor = ws.editors.get_mut(excerpt.editor_id)?;
    Some((state.focused, view::cursor_display_row(editor)))
}

/// Focus the nearest excerpt past the focused one that paints rows, with the
/// cursor at the start of its first row going down or of its last row going
/// up, and report whether one lies that way.
///
/// A spliced base row maps to no buffer position, so the cursor lands on the
/// outermost row that maps to one.
///
/// An insert session crosses with the cursor, as [`carry_insert_session`]
/// carries it.
fn cross_excerpt(stoat: &mut Stoat, down: bool) -> bool {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return false;
    };
    let Workspace {
        diff_changes_views,
        editors,
        buffers,
        ..
    } = ws;
    let Some(state) = diff_changes_views.get_mut(id) else {
        return false;
    };
    let Some(index) = excerpts_past(state, down)
        .into_iter()
        .find(|&index| state.excerpts[index].shows_rows())
    else {
        return false;
    };
    let Some(editor) = editors.get_mut(state.excerpts[index].editor_id) else {
        return false;
    };

    let point = {
        let snapshot = editor.display_map.snapshot();
        edge_row(&snapshot, down)
            .and_then(|row| snapshot.display_to_buffer(DisplayPoint::new(row, 0)))
    };
    let Some(point) = point else {
        return false;
    };
    diff_changes::place_cursor(editor, point);
    let left = state.focused_excerpt().map(|excerpt| excerpt.editor_id);
    state.focused = index;

    let entered = state.excerpts[index].editor_id;
    if left.is_some_and(|left| carry_insert_session(editors, buffers, left, entered)) {
        // Both record the left editor's insert entry, and the next exit from
        // insert mode reads them against the entered editor.
        stoat.restore_cursor = false;
        stoat.auto_indent_cursors.clear();
    }
    true
}

/// Move an insert session from the editor `left` to the editor `entered`, and
/// report whether `left` held one.
///
/// The entered editor takes insert mode and `left` returns to normal mode. The
/// left buffer's undo group seals and the entered buffer opens one, as leaving
/// and entering insert mode do, so each file's typing undoes as its own step.
fn carry_insert_session(
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    left: EditorId,
    entered: EditorId,
) -> bool {
    let Some(editor) = editors
        .get_mut(left)
        .filter(|editor| editor.mode == "insert")
    else {
        return false;
    };
    if let Some(buffer) = buffers.get(editor.buffer_id) {
        let after = editor.selections.shared_anchors();
        buffer.write().expect("poisoned").seal_group(after);
    }
    editor.mode = "normal".into();

    let Some(editor) = editors.get_mut(entered) else {
        return true;
    };
    editor.mode = "insert".into();
    if let Some(buffer) = buffers.get(editor.buffer_id) {
        let before = || editor.selections.shared_anchors();
        buffer.write().expect("poisoned").try_begin_group(before);
    }
    true
}

/// The indices of the excerpts past the focused one, nearest first: below it
/// when `down` is set, else above it.
fn excerpts_past(state: &DiffChangesState, down: bool) -> Vec<usize> {
    match down {
        true => (state.focused + 1..state.excerpts.len()).collect(),
        false => (0..state.focused).rev().collect(),
    }
}

/// Move every cursor of `editor` by `steps` steps of the row walk toward
/// `down`, as that many calls of [`movement::move_vertical`] move it, each
/// cursor keeping its goal column.
fn walk_cursors(editor: &mut EditorState, steps: u32, down: bool) {
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let rope = buffer.rope();
    movement::move_cursors(&mut editor.selections, buffer, false, |read| {
        let cursor = cursor_offset(rope, read.tail, read.head);
        let at = snapshot.buffer_to_display(rope.offset_to_point(cursor));
        let goal = match read.goal {
            SelectionGoal::Column(column) => column,
            SelectionGoal::None => at.column,
        };
        let point = walk_landing(&snapshot, at.row, goal, steps, down)?;
        Some((rope.point_to_offset(point), SelectionGoal::Column(goal)))
    });
}

/// Carry a walk past the focused excerpt's edge through the painted excerpts
/// beyond it, as [`cross_excerpt`] carries it a step at a time, with `left`
/// steps still to take after the crossing.
///
/// A crossing puts one cursor at the start of the entered excerpt's outermost
/// landing row, and the steps left walk on from there. A walk that reaches an
/// excerpt's edge with steps left takes one more step there and crosses
/// again, so every excerpt it passes through keeps its cursor where the row
/// walk leaves it. The walk stops at an excerpt with no landing row.
fn cross_rows(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    mut left: u32,
    down: bool,
) {
    for index in excerpts_past(state, down) {
        let excerpt = &state.excerpts[index];
        if !excerpt.shows_rows() {
            continue;
        }
        let Some(editor) = editors.get_mut(excerpt.editor_id) else {
            break;
        };
        let entered = {
            let snapshot = editor.display_map.snapshot();
            edge_row(&snapshot, down).and_then(|entry| {
                let start = snapshot.display_to_buffer(DisplayPoint::new(entry, 0))?;
                Some((start, walk_rows(&snapshot, entry, left, down).0))
            })
        };
        let Some((start, taken)) = entered else {
            break;
        };
        diff_changes::place_cursor(editor, start);
        state.focused = index;
        if taken == left {
            walk_cursors(editor, left, down);
            return;
        }
        walk_cursors(editor, taken + 1, down);
        left -= taken + 1;
    }
}

/// Where `steps` steps of the row walk toward `down` leave a cursor at display
/// row `row` with goal column `goal`, or `None` where no step moves it.
///
/// A step lands with the column [`movement::clip_to_goal`] gives on the row it
/// aimed at, so a step over a removed base row lands at the near edge of the
/// row past it. A step at the excerpt's edge aims past it, clamped to the
/// excerpt, and leaves the cursor where that clip puts it, or where the last
/// step left it.
fn walk_landing(
    snapshot: &DisplaySnapshot,
    row: u32,
    goal: u32,
    steps: u32,
    down: bool,
) -> Option<Point> {
    let travel = match down {
        true => Bias::Right,
        false => Bias::Left,
    };
    let land =
        |target| snapshot.display_to_buffer(movement::clip_to_goal(snapshot, target, goal, travel));
    let (taken, at, aimed) = walk_rows(snapshot, row, steps, down);
    let moved = aimed.and_then(land);
    if taken == steps {
        return moved;
    }
    let edge = match down {
        true => (at + 1).min(snapshot.max_point().row),
        false => at.saturating_sub(1),
    };
    land(edge).or(moved)
}

/// The steps of the row walk from display row `row` toward `down` that reach
/// another row, at most `steps`, the row they reach, and the row the last of
/// them aimed at.
///
/// A step reaches the next row toward `down` that maps to a buffer position,
/// as a text row and the top fold's placeholder do. It aims at the row beside
/// its start, and [`movement::clip_to_goal`] carries an aim at a removed base
/// row, which maps to none, on to that next row. The steps fall short at the
/// excerpt's edge.
fn walk_rows(
    snapshot: &DisplaySnapshot,
    row: u32,
    steps: u32,
    down: bool,
) -> (u32, u32, Option<u32>) {
    let last = snapshot.max_point().row;
    let (mut at, mut taken, mut aimed) = (row, 0, None);
    while taken < steps {
        let next = match down {
            true => (at + 1..=last).find(|&row| lands(snapshot, row)),
            false => (0..at).rev().find(|&row| lands(snapshot, row)),
        };
        let Some(next) = next else {
            break;
        };
        aimed = Some(match down {
            true => at + 1,
            false => at - 1,
        });
        at = next;
        taken += 1;
    }
    (taken, at, aimed)
}

/// The outermost display row of an excerpt that a cursor lands on, the first
/// going down and the last going up.
fn edge_row(snapshot: &DisplaySnapshot, down: bool) -> Option<u32> {
    let last = snapshot.max_point().row;
    match down {
        true => (0..=last).find(|&row| lands(snapshot, row)),
        false => (0..=last).rev().find(|&row| lands(snapshot, row)),
    }
}

/// Whether a cursor lands on display row `row`, which holds when the row maps
/// to a buffer position.
fn lands(snapshot: &DisplaySnapshot, row: u32) -> bool {
    snapshot
        .display_to_buffer(DisplayPoint::new(row, 0))
        .is_some()
}
