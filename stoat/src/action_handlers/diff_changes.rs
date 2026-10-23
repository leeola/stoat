//! The handlers of a [`View::DiffChanges`] pane.
//!
//! They open and close the changes view, flip between it and the per-file diff,
//! move its cursor through the stack, carry the change walk across its
//! excerpts, and respawn a pane whose state did not survive a restore.

use super::{
    jump,
    movement::{self, ChangeDir},
    pane, review,
    view::{self, PageDir, ViewAlign},
};
use crate::{
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    buffer_registry::OpenOrigin,
    diff_changes::{self, DiffChangesState, Landing},
    display_map::DisplayPoint,
    pane::{PaneId, View},
    workspace::Workspace,
};
use std::path::PathBuf;
use stoat_scheduler::Executor;
use stoat_text::Point;

/// Flip between the changes view and the per-file diff.
///
/// On a focused changes view this opens the per-file diff at the cursor's hunk.
/// Anywhere else it shows a changes view over the focused view.
pub(super) fn toggle_diff_changes(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().focused_diff_changes_id().is_some() {
        return diff_changes_open(stoat);
    }
    open_diff_changes(stoat)
}

/// Close the changes view the focused pane shows and put back the view it
/// covered.
///
/// The excerpts close with it, and so does each buffer only the stack opened.
/// A covered view that is gone closes the pane instead, and a last pane gets a
/// fresh scratch editor. A changes view opened from a per-file diff returns to
/// that diff, so a close peels back one layer.
pub(crate) fn close_diff_changes(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    if ws.focused_diff_changes_id().is_none() {
        return UpdateEffect::None;
    }
    let focus = ws.panes.focus();
    let Some(state) = take_down(ws, focus) else {
        return UpdateEffect::None;
    };
    let (covered, covered_diff) = (state.covered, state.covered_diff);
    diff_changes::release_excerpts(stoat, state.excerpts);
    put_back_covered(stoat, focus, covered, covered_diff);
    UpdateEffect::Redraw
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
    if let Some(displaced) = ws.parked_diff_changes.replace(state) {
        diff_changes::release_excerpts(stoat, displaced.excerpts);
    }
    put_back_covered(stoat, focus, covered, false);
}

/// Show `covered` in pane `focus` once its changes view is gone, in normal
/// mode.
///
/// A covered view that is gone closes the pane instead, and a last pane gets a
/// fresh scratch editor. With `covered_diff`, a covered editor returns to the
/// per-file diff it showed.
fn put_back_covered(stoat: &mut Stoat, focus: PaneId, covered: Option<View>, covered_diff: bool) {
    let ws = stoat.active_workspace();
    let live = pane::view_is_live(ws, covered.as_ref());
    let rediff = live && covered_diff && matches!(covered, Some(View::Editor(_)));
    if live {
        pane::show_view_or_scratch(stoat, focus, covered);
    } else if !pane::close_pane_by_id(stoat, focus) {
        pane::show_view_or_scratch(stoat, focus, None);
    }
    stoat.set_focused_mode("normal".to_string());
    if rediff {
        review::enter_diff_view(stoat);
    }
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
    let Some(state) = take_down(ws, pane_id) else {
        return;
    };
    diff_changes::dispose_excerpts(ws, state.excerpts);
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
}

/// Park the changes view and open its focused excerpt's file in the per-file
/// diff, at the excerpt's cursor.
///
/// The file opens in the pane after the changes view comes down and puts back
/// the view it covered, over the excerpt's own buffer, so an edit made in the
/// excerpt and its undo history carry over. The stack parks rather than
/// closing, so the flip back reuses its excerpts. A stack with no folded
/// excerpt in focus, as before the first diff map lands, leaves the changes
/// view open and says so in the status row.
///
/// The cursor's line keeps the screen row it held in the stack, where the file
/// has rows enough above and below it. A cursor off the stack's painted rows
/// opens with its line centered.
pub(super) fn diff_changes_open(stoat: &mut Stoat) -> UpdateEffect {
    let Some(((rel_path, point), workdir)) = focused_target(stoat).zip(repo_workdir(stoat)) else {
        stoat.set_status("no change under the cursor");
        return UpdateEffect::Redraw;
    };
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
    if let Some(editor) = super::focused_editor_mut(stoat) {
        match screen_row {
            Some(row) => editor.held_screen_row = Some(row),
            None => {
                view::align_view(editor, ViewAlign::Center);
            },
        }
    }
    UpdateEffect::Redraw
}

/// Move the focused changes view's cursor by `delta` display rows.
///
/// A step past an excerpt's last row goes on to the first row of the next
/// excerpt, and a step before its first row to the last row of the previous
/// one, so the steps walk the rows the stack paints. An excerpt that waits on
/// its diff map paints no row, so a step from it holds still and a crossing
/// passes over it. At either end of the stack the cursor stays.
pub(super) fn diff_changes_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
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

/// Carry the change walk past the focused excerpt's last hunk to the first
/// hunk of the next excerpt, or before its first hunk to the last hunk of the
/// previous one.
///
/// The hop lands as the per-file diff's hop into the next changed file lands,
/// with the origin on the jumplist and the landing recorded, so `DiffBack` and
/// `DiffForward` go on walking changes. An excerpt whose diff map holds no hunk
/// is passed over. At either end of the stack the cursor stays, and the status
/// row says that no more changes lie that way.
pub(super) fn hop_excerpt(stoat: &mut Stoat, dir: ChangeDir) -> UpdateEffect {
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
        stoat.set_status("no more changes");
        return UpdateEffect::Redraw;
    };
    state.focused = index;

    movement::land_file_hop(stoat, dir, origin, false, (start, start..start + 1));
    UpdateEffect::Redraw
}

/// Move the focused excerpt's cursor by the rows of the stack's last paint.
pub(super) fn diff_changes_page(stoat: &mut Stoat, dir: PageDir) -> UpdateEffect {
    let rows = viewport_rows(stoat);
    diff_changes_step(stoat, page_delta(rows, dir))
}

/// Move the focused excerpt's cursor by half the rows of the stack's last
/// paint, and by one row at least.
pub(super) fn diff_changes_half_page(stoat: &mut Stoat, dir: PageDir) -> UpdateEffect {
    let rows = (viewport_rows(stoat) / 2).max(1);
    diff_changes_step(stoat, page_delta(rows, dir))
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

    let kept = diff_changes::kept_rows(editor);
    let row = match last {
        true => kept.last().map_or(0, |run| run.end - 1),
        false => kept.first().map_or(0, |run| run.start),
    };
    diff_changes::place_cursor(editor, Point::new(row, 0));
    state.focused = index;
    UpdateEffect::Redraw
}

/// Put the focused changes view's cursor on the cell a press lands on, at
/// stack row `row` and screen column `column`.
///
/// The row's excerpt takes the focus, and its cursor goes to the cell, as a
/// press in an editor puts it. A header row puts the cursor at the start of
/// its excerpt's first row.
pub(crate) fn diff_changes_press(stoat: &mut Stoat, row: usize, column: u16) -> UpdateEffect {
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
    diff_changes::layout_stack(state, editors);
    match diff_changes::press_cell(state, editors, row, column) {
        true => UpdateEffect::Redraw,
        false => UpdateEffect::None,
    }
}

/// Scroll the focused changes view's stack by `delta` rows and leave its
/// cursor, as a wheel scrolls an editor.
///
/// The scroll stays between the stack's top and its last page. The cursor
/// stays off screen until it moves, since only a moved cursor pulls the stack
/// back.
pub(crate) fn diff_changes_scroll(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
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
pub(crate) fn respawn_diff_changes_panes(stoat: &mut Stoat) {
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
fn open_diff_changes(stoat: &mut Stoat) -> UpdateEffect {
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

/// The focused excerpt's file, relative to the repository, and the buffer
/// position of its cursor.
///
/// `None` with no folded excerpt in focus.
fn focused_target(stoat: &mut Stoat) -> Option<(String, Point)> {
    let ws = stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id()?;
    let excerpt = ws.diff_changes_views.get(id)?.focused_excerpt()?;
    excerpt.folded_version?;
    let rel_path = excerpt.rel_path.clone();
    let editor = ws.editors.get_mut(excerpt.editor_id)?;
    Some((rel_path, diff_changes::cursor_point(editor)))
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
fn viewport_rows(stoat: &Stoat) -> usize {
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
    excerpt.folded_version?;
    let editor = ws.editors.get_mut(excerpt.editor_id)?;
    Some((state.focused, view::cursor_display_row(editor)))
}

/// Focus the nearest excerpt past the focused one that paints rows, with the
/// cursor at the start of its first row going down or of its last row going
/// up, and report whether one lies that way.
///
/// A spliced base row maps to no buffer position, so the cursor lands on the
/// outermost row that maps to one.
fn cross_excerpt(stoat: &mut Stoat, down: bool) -> bool {
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
    let Some(index) = excerpts_past(state, down)
        .into_iter()
        .find(|&index| state.excerpts[index].folded_version.is_some())
    else {
        return false;
    };
    let Some(editor) = editors.get_mut(state.excerpts[index].editor_id) else {
        return false;
    };

    let point = {
        let snapshot = editor.display_map.snapshot();
        let landing = |row| snapshot.display_to_buffer(DisplayPoint::new(row, 0));
        let last = snapshot.max_point().row;
        match down {
            true => (0..=last).find_map(landing),
            false => (0..=last).rev().find_map(landing),
        }
    };
    let Some(point) = point else {
        return false;
    };
    diff_changes::place_cursor(editor, point);
    state.focused = index;
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

#[cfg(test)]
mod tests {
    use super::{diff_changes_step, respawn_diff_changes_panes};
    use crate::{
        action_handlers::{commits, dispatch, focused_editor_mut, pane},
        app::UpdateEffect,
        buffer::BufferId,
        buffer_lifecycle,
        diff_changes::{self, DiffChangesId, DiffChangesState, StackStamp},
        editor_state::EditorId,
        pane::{Axis, DockPanel, DockSide, DockVisibility, FocusTarget, View},
        run::pty::PtyNotification,
        term_session::TermId,
        test_harness::{editor, TestHarness},
    };
    use std::path::{Path, PathBuf};
    use stoat_action::Action;
    use stoat_config::MinimapMode;
    use stoat_text::Point;

    /// Two changed files, with `a.txt` open in the focused pane.
    fn two_files() -> TestHarness {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        h
    }

    /// Two changed files whose one change sits on the last of nine lines, so
    /// the lines above the change lie outside its context.
    fn deep_files() -> TestHarness {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario(
            "/repo",
            &[
                (
                    "a.txt",
                    "1\n2\n3\n4\n5\n6\n7\n8\n9\n",
                    "1\n2\n3\n4\n5\n6\n7\n8\nNINE\n",
                ),
                (
                    "b.txt",
                    "a\nb\nc\nd\ne\nf\ng\nh\ni\n",
                    "a\nb\nc\nd\ne\nf\ng\nh\nI\n",
                ),
            ],
        );
        h
    }

    /// Two changed files of sixty lines in a window `height` rows tall, each
    /// changed on its forty-first line, so each excerpt folds rows above and
    /// below its hunk.
    fn long_files(height: u16) -> TestHarness {
        let mut h = TestHarness::with_size(100, height);
        let base: String = (1..=60).map(|n| format!("{n}\n")).collect();
        let working = base.replace("\n41\n", "\nFORTY-ONE\n");
        h.stage_review_scenario(
            "/repo",
            &[("a.txt", &base, &working), ("b.txt", &base, &working)],
        );
        h
    }

    /// The screen row of the focused editor's cursor after a paint.
    fn screen_row(h: &mut TestHarness) -> Option<u16> {
        h.render_composited();
        focused_editor_mut(&mut h.stoat)?
            .cursor_screen_cell
            .map(|(_, row)| row)
    }

    /// The focused excerpt's file and the buffer row of its cursor.
    fn place(h: &mut TestHarness) -> (String, u32) {
        let ws = h.stoat.active_workspace_mut();
        let (rel_path, editor_id) = {
            let excerpt = ws
                .focused_diff_changes()
                .and_then(DiffChangesState::focused_excerpt)
                .expect("a focused excerpt");
            (excerpt.rel_path.clone(), excerpt.editor_id)
        };
        let editor = ws.editors.get_mut(editor_id).expect("an excerpt editor");
        (rel_path, diff_changes::cursor_point(editor).row)
    }

    fn at(rel_path: &str, row: u32) -> (String, u32) {
        (rel_path.to_string(), row)
    }

    /// The file of the focused editor.
    fn focused_path(h: &TestHarness) -> Option<PathBuf> {
        let (_, buffer) = h.stoat.focused_editor_ids()?;
        h.stoat
            .active_workspace()
            .buffers
            .path_for(buffer)
            .map(Path::to_path_buf)
    }

    /// A second tab of two panes whose focused pane shows a shell over an
    /// changes view, and that shell.
    fn shell_over_a_changes_view() -> (TestHarness, TermId) {
        let mut h = two_files();
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.cover_with_diff_changes(false);
        dispatch(&mut h.stoat, &stoat_action::Terminal);
        let ws = h.stoat.active_workspace();
        let View::Terminal(shell) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the terminal action shows a terminal");
        };
        (h, shell)
    }

    /// The changes view's keys read its focused excerpt's mode, which starts at
    /// normal whatever chord the covered editor held, and the diff dials step
    /// on it as on the diff view.
    #[test]
    fn a_changes_view_takes_its_focused_excerpts_mode_and_answers_the_dials() {
        let mut h = two_files();
        h.stoat.set_focused_mode("space".to_string());
        h.cover_with_diff_changes(false);
        h.settle();
        let fresh = h.stoat.focused_mode().to_string();
        h.stoat.set_focused_mode("space".to_string());
        let mode = h.stoat.focused_mode().to_string();
        h.stoat.handle_diff_tint_step(1);
        let excerpt_mode = {
            let ws = h.stoat.active_workspace();
            ws.focused_diff_changes()
                .and_then(DiffChangesState::focused_excerpt)
                .map(|excerpt| ws.editors[excerpt.editor_id].mode.clone())
        };
        assert_eq!(
            (
                h.stoat.current_view(),
                fresh,
                mode,
                excerpt_mode,
                h.stoat.diff_tint
            ),
            (
                Some("diff_changes"),
                "normal".to_string(),
                "space".to_string(),
                Some("space".to_string()),
                1
            ),
        );
    }

    /// A layout key on the changes view closes it, as it closes every
    /// full-window screen, and the pane shows the editor the changes view
    /// covered at its own width and in normal mode. A changes view opened from
    /// the per-file diff returns to that diff, which widens the pane again.
    #[test]
    fn a_layout_key_closes_the_changes_view_to_the_view_it_covered() {
        for (covered_diff, view, widened) in [(false, "file", false), (true, "diff", true)] {
            let mut h = two_files();
            let (editor, _) = h.stoat.focused_editor_ids().expect("editor");
            h.stoat.set_focused_mode("insert".to_string());
            h.cover_with_diff_changes(covered_diff);

            dispatch(&mut h.stoat, &stoat_action::SplitRight);
            let ws = h.stoat.active_workspace();
            assert_eq!(
                (
                    ws.diff_changes_views.len(),
                    ws.panes.split_pane_ids().len(),
                    ws.panes.pane(ws.panes.focus()).view.clone(),
                    ws.panes.widened().is_some(),
                    h.stoat.current_view(),
                    h.stoat.focused_mode(),
                    h.stoat.pending_message.as_deref(),
                ),
                (
                    0,
                    1,
                    View::Editor(editor),
                    widened,
                    Some(view),
                    "normal",
                    Some("changes closed")
                ),
                "covered_diff {covered_diff}",
            );
        }
    }

    /// A changes view over a dead view closes its pane, and a last pane gets a
    /// scratch editor.
    #[test]
    fn closing_a_changes_view_over_a_label_closes_the_pane_or_falls_back_to_a_scratch() {
        let mut h = two_files();
        dispatch(&mut h.stoat, &stoat_action::SplitRight);

        let mut closed = Vec::new();
        for _ in 0..2 {
            let ws = h.stoat.active_workspace_mut();
            let focus = ws.panes.focus();
            ws.panes.pane_mut(focus).view = View::Label("label".into());
            h.cover_with_diff_changes(false);
            dispatch(&mut h.stoat, &stoat_action::SplitRight);
            closed.push((
                h.stoat.active_workspace().panes.split_pane_ids().len(),
                h.stoat.current_view(),
            ));
        }
        assert_eq!(closed, [(1, Some("file")), (1, Some("file"))]);
    }

    /// The changes view belongs to its pane. A split off it opens a scratch
    /// editor and rests the chord of the changes view's focused excerpt, and a
    /// close of its pane drops it with the editor it covered.
    #[test]
    fn a_changes_view_stays_with_its_own_pane() {
        let mut h = two_files();
        let (editor, _) = h.stoat.focused_editor_ids().expect("editor");
        let id = h.cover_with_diff_changes(false);
        h.settle();
        h.stoat.set_focused_mode("space".to_string());
        let source = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.unwiden();

        pane::split_pane(&mut h.stoat, Axis::Vertical);
        let (kept, split_view, mode) = {
            let ws = h.stoat.active_workspace();
            let mode = ws.diff_changes_views[id]
                .focused_excerpt()
                .map(|excerpt| ws.editors[excerpt.editor_id].mode.clone());
            (
                ws.panes.pane(source).view.clone(),
                ws.panes.pane(ws.panes.focus()).view.clone(),
                mode,
            )
        };
        pane::close_pane_by_id(&mut h.stoat, source);
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                kept,
                matches!(split_view, View::Editor(_)),
                mode,
                ws.diff_changes_views.len(),
                ws.editors.contains_key(editor),
            ),
            (
                View::DiffChanges(id),
                true,
                Some("normal".to_string()),
                0,
                false
            ),
        );
    }

    #[test]
    fn closing_a_changes_view_pane_keeps_a_shell_another_pane_shows() {
        let mut h = two_files();
        dispatch(&mut h.stoat, &stoat_action::Terminal);
        // The split action runs a shell of its own, so the tree split, which
        // copies the view, is what puts a second pane over this one shell.
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(Axis::Vertical);
        let View::Terminal(shell) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the split shows the shell too");
        };
        h.cover_with_diff_changes(false);

        dispatch(&mut h.stoat, &stoat_action::ClosePane);
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.diff_changes_views.len(), ws.terms.contains_key(shell)),
            (0, true)
        );
    }

    /// A shell opened over the changes view holds it beneath, and the pane or
    /// the tab that closes takes it along.
    #[test]
    fn a_changes_view_beneath_a_shell_closes_with_its_pane_or_tab() {
        let actions: [&dyn Action; 2] = [&stoat_action::ClosePane, &stoat_action::CloseTab];
        let counts = actions.map(|action| {
            let (mut h, _) = shell_over_a_changes_view();
            let open = h.stoat.active_workspace().diff_changes_views.len();
            dispatch(&mut h.stoat, action);
            (open, h.stoat.active_workspace().diff_changes_views.len())
        });
        assert_eq!(counts, [(1, 0); 2]);
    }

    /// A shell that exits in a parked tab closes its pane, and the changes view
    /// beneath the shell goes with it.
    #[test]
    fn an_exited_shell_in_a_parked_tab_drops_the_changes_view_beneath() {
        let (mut h, shell) = shell_over_a_changes_view();
        dispatch(&mut h.stoat, &stoat_action::PrevTab);
        h.stoat
            .handle_pty_notification(PtyNotification::TermExited {
                workspace: h.stoat.active_workspace,
                term_id: shell,
            });
        assert_eq!(h.stoat.active_workspace().diff_changes_views.len(), 0);
    }

    #[test]
    fn a_shell_over_the_changes_view_gives_it_back_when_it_exits() {
        let mut h = two_files();
        h.cover_with_diff_changes(false);
        dispatch(&mut h.stoat, &stoat_action::Terminal);
        let ws = h.stoat.active_workspace();
        let View::Terminal(term_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the terminal action shows a terminal");
        };

        h.stoat
            .handle_pty_notification(PtyNotification::TermExited {
                workspace: h.stoat.active_workspace,
                term_id,
            });
        assert_eq!(h.stoat.current_view(), Some("diff_changes"));
    }

    /// `j` past an excerpt's last row goes on to the next file's first row, and
    /// `k` there goes back to the last row. A step before the stack holds an
    /// excerpt moves nothing, and a step past either end of the stack stays.
    #[test]
    fn j_crosses_into_the_next_file_and_k_back() {
        let mut h = deep_files();
        h.cover_with_diff_changes(false);
        let unbuilt = diff_changes_step(&mut h.stoat, 1);

        h.settle();
        let walk: Vec<(String, u32)> = ["j j j j", "j", "k"]
            .into_iter()
            .map(|keys| {
                h.type_keys(keys);
                place(&mut h)
            })
            .collect();
        diff_changes_step(&mut h.stoat, -100);
        let top = place(&mut h);
        diff_changes_step(&mut h.stoat, 100);
        assert_eq!(
            (unbuilt, walk, top, place(&mut h)),
            (
                UpdateEffect::None,
                vec![at("a.txt", 9), at("b.txt", 0), at("a.txt", 9)],
                at("a.txt", 0),
                at("b.txt", 9),
            ),
        );
    }

    /// The editor's word motion acts on the focused excerpt, so `w` moves its
    /// cursor along the file's own line.
    #[test]
    fn w_moves_the_excerpt_cursor_by_a_word() {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "one two\n")]);
        h.cover_with_diff_changes(false);
        h.settle();
        let before = editor::focused_cursor_point(&mut h.stoat);

        h.type_keys("w");
        assert_eq!(
            (before, editor::focused_cursor_point(&mut h.stoat)),
            (Point::new(0, 0), Point::new(0, 3)),
        );
    }

    /// A changes view with no excerpt has no focused editor, so the editor
    /// keys find nothing to act on.
    #[test]
    fn an_empty_stack_has_no_focused_editor() {
        let mut h = TestHarness::with_size(100, 24);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git().add_repo(PathBuf::from("/repo"));
        h.cover_with_diff_changes(false);
        h.settle();
        assert_eq!(
            (h.stoat.current_view(), h.stoat.focused_editor_ids()),
            (Some("diff_changes"), None),
        );
    }

    /// Keys go to a focused dock, so the changes view in the split pane beneath
    /// it lends the dock no mode.
    #[test]
    fn a_focused_dock_takes_no_mode_from_the_changes_view() {
        let mut h = two_files();
        h.cover_with_diff_changes(false);
        h.settle();
        h.stoat.set_focused_mode("space".to_string());
        let ws = h.stoat.active_workspace_mut();
        let dock = ws.docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Hidden,
            default_width: 30,
            area: Default::default(),
        });
        ws.focus = FocusTarget::Dock(dock);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    /// A restore brings back the pane with a dead id, which gets a live state
    /// of its own and a stack, though another pane holds focus. A live changes
    /// view keeps its state, and a respawn that finds no dead pane opens no
    /// changes view.
    #[test]
    fn a_restored_changes_view_pane_gets_a_fresh_state_and_a_stack() {
        let mut h = two_files();
        respawn_diff_changes_panes(&mut h.stoat);
        h.settle();
        let opened_for_no_pane = h.stoat.active_workspace().diff_changes_views.len();

        let restored = h.stoat.active_workspace().panes.focus();
        pane::split_pane(&mut h.stoat, Axis::Vertical);
        let live = h.stoat.active_workspace().panes.focus();
        pane::split_pane(&mut h.stoat, Axis::Vertical);
        let ws = h.stoat.active_workspace_mut();
        let live_id = ws.diff_changes_views.insert(DiffChangesState::default());
        ws.panes.pane_mut(live).view = View::DiffChanges(live_id);
        ws.panes.pane_mut(restored).view = View::DiffChanges(DiffChangesId::default());
        respawn_diff_changes_panes(&mut h.stoat);
        h.settle();
        let ws = h.stoat.active_workspace();
        let kept = (
            ws.panes.pane(live).view.clone(),
            ws.diff_changes_views.len(),
        );
        let state = match ws.panes.pane(restored).view {
            View::DiffChanges(id) => ws.diff_changes_views.get(id).map(|state| {
                let files: Vec<&str> = state
                    .excerpts
                    .iter()
                    .map(|excerpt| excerpt.rel_path.as_str())
                    .collect();
                (state.covered.clone(), files)
            }),
            _ => None,
        };
        assert_eq!(
            (opened_for_no_pane, kept, state),
            (
                0,
                (View::DiffChanges(live_id), 2),
                Some((None, vec!["a.txt", "b.txt"]))
            ),
        );
    }

    /// A git event moves the stamp, and the stack lists its files anew at the
    /// new stamp.
    #[test]
    fn a_changes_view_relists_when_the_tree_moves() {
        let synced = |h: &TestHarness| {
            let ws = h.stoat.active_workspace();
            let state = ws.focused_diff_changes().expect("a changes view");
            (state.synced, Some(StackStamp::read(ws)))
        };
        let mut h = two_files();
        h.cover_with_diff_changes(false);
        h.settle();
        let (before, _) = synced(&h);

        h.stoat.active_workspace_mut().invalidate_all_diffs();
        let (_, moved) = synced(&h);
        h.settle();
        let (after, current) = synced(&h);
        assert_eq!(
            (before != moved, after != before, after == current),
            (true, true, true)
        );
    }

    /// From a plain editor the changes view lands on the editor's line once its
    /// file's excerpt is folded. A line that a fold hides lands on the first
    /// row of the next shown run below it.
    #[test]
    fn diff_changes_from_a_plain_editor_lands_below_its_line() {
        let mut h = deep_files();
        h.open_file(Path::new("/repo/b.txt"));
        h.type_keys("j j");
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        assert_eq!(
            (
                h.stoat.current_view(),
                h.stoat.active_workspace().panes.widened().is_some(),
                place(&mut h),
            ),
            (Some("diff_changes"), true, at("b.txt", 5)),
        );
    }

    /// From the per-file diff the changes view lands on the diff's line, with
    /// the editor plain beneath it, and a close returns to the diff.
    #[test]
    fn diff_changes_from_the_diff_lands_on_its_line_and_q_returns_to_the_diff() {
        let mut h = deep_files();
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        h.settle();
        let (editor, _) = h.stoat.focused_editor_ids().expect("editor");

        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let landed = place(&mut h);
        let plain = {
            let ws = h.stoat.active_workspace();
            !ws.editors[editor].diff_view && !ws.panes.pane(ws.panes.focus()).diff_mode
        };
        h.type_keys("q");
        assert_eq!(
            (
                landed,
                plain,
                h.stoat.current_view(),
                h.stoat.focused_editor_ids().map(|(id, _)| id),
            ),
            (at("a.txt", 8), true, Some("diff"), Some(editor)),
        );
    }

    /// Ctrl-t on the changes view opens the cursor's file in the per-file diff
    /// on the cursor's line, and Ctrl-t there returns to the changes view on
    /// that line. Each flip keeps the line on the screen row it held.
    #[test]
    fn ctrl_t_flips_between_the_changes_view_and_the_diff_on_one_line() {
        let mut h = long_files(24);
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        h.type_keys("n n");
        let stacked = screen_row(&mut h);

        h.type_keys("Ctrl-t");
        h.settle();
        let opened = (
            h.stoat.current_view(),
            focused_path(&h),
            editor::focused_cursor_point(&mut h.stoat).row,
            screen_row(&mut h),
        );

        h.type_keys("Ctrl-t");
        h.settle();
        let back = (h.stoat.current_view(), place(&mut h), screen_row(&mut h));
        assert_eq!(
            (stacked.is_some(), opened, back),
            (
                true,
                (
                    Some("diff"),
                    Some(PathBuf::from("/repo/b.txt")),
                    40,
                    stacked
                ),
                (Some("diff_changes"), at("b.txt", 40), stacked),
            ),
        );
    }

    /// The focused changes view's excerpts as their editors and buffers, in
    /// stack order.
    fn excerpt_ids(h: &TestHarness) -> Vec<(EditorId, BufferId)> {
        h.stoat
            .active_workspace()
            .focused_diff_changes()
            .map(|state| {
                state
                    .excerpts
                    .iter()
                    .map(|excerpt| (excerpt.editor_id, excerpt.buffer_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The flip to the per-file diff parks the stack, so the flip back finds
    /// the same excerpt editors and buffers, and no buffer closes or opens on
    /// the language servers.
    #[test]
    fn a_flip_and_back_keeps_the_stack_excerpts() {
        let mut h = long_files(24);
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let before = excerpt_ids(&h);
        let traffic = |h: &TestHarness| {
            (
                h.fake_lsp().observed_opens().len(),
                h.fake_lsp().observed_closes().len(),
            )
        };
        let quiet = traffic(&h);

        h.type_keys("Ctrl-t");
        h.settle();
        h.type_keys("Ctrl-t");
        h.settle();
        assert_eq!(
            (before.len(), excerpt_ids(&h), traffic(&h)),
            (2, before, quiet),
        );
    }

    /// A flip and a flip back read the root the workspace resolved, so neither
    /// opens a repository.
    #[test]
    fn a_flip_and_back_open_no_repository() {
        let mut h = long_files(24);
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let opened = h.fake_git().discover_calls();

        h.type_keys("Ctrl-t");
        h.settle();
        h.type_keys("Ctrl-t");
        h.settle();
        assert_eq!(h.fake_git().discover_calls() - opened, 0);
    }

    /// A close after a flip back releases the stack as a close always does, so
    /// the buffer only the stack opened closes, and nothing stays parked.
    #[test]
    fn a_close_after_a_flip_back_releases_the_stack_buffers() {
        let mut h = long_files(24);
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        h.type_keys("Ctrl-t");
        h.settle();
        h.type_keys("Ctrl-t");
        h.settle();
        h.type_keys("Escape");
        h.settle();
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.buffers.id_for_path(Path::new("/repo/b.txt")),
                ws.parked_diff_changes.is_some(),
            ),
            (None, false),
        );
    }

    /// A changed file over the inline read ceiling reads on the pool. Its
    /// excerpt joins the stack in path order once the read installs. The stack
    /// owns its buffer, the focus stays on its file, and a second adopt of the
    /// file adds nothing.
    #[test]
    fn a_large_changed_file_joins_the_stack_once_its_read_lands() {
        let mut h = TestHarness::with_size(100, 24);
        let line = "x".repeat(1000);
        let big: String = (0..1100).map(|n| format!("{line}{n}\n")).collect();
        let edited = big.replacen('x', "y", 1);
        h.stage_review_scenario(
            "/repo",
            &[("z.txt", "z\n", "Z\n"), ("big.txt", &big, &edited)],
        );
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let listed = |h: &TestHarness| -> (Vec<(String, bool)>, Option<String>) {
            let state = h.stoat.active_workspace().focused_diff_changes();
            let excerpts = state
                .map(|state| {
                    state
                        .excerpts
                        .iter()
                        .map(|excerpt| (excerpt.rel_path.clone(), excerpt.owns_buffer))
                        .collect()
                })
                .unwrap_or_default();
            let focused = state
                .and_then(DiffChangesState::focused_excerpt)
                .map(|excerpt| excerpt.rel_path.clone());
            (excerpts, focused)
        };
        let before = listed(&h);

        // Read before a settle, whose relist sorts the stack again, so the
        // order is the one the install itself left.
        buffer_lifecycle::install_pending_opens(&mut h.stoat);
        let after = listed(&h);

        let (view, buffer_id, buffer) = {
            let ws = h.stoat.active_workspace();
            let id = ws
                .buffers
                .id_for_path(Path::new("/repo/big.txt"))
                .expect("the read installed");
            let view = ws.focused_diff_changes_id().expect("a changes view");
            (view, id, ws.buffers.get(id).expect("the buffer"))
        };
        diff_changes::adopt_excerpt(&mut h.stoat, view, "big.txt".to_string(), buffer_id, buffer);
        let z = || ("z.txt".to_string(), true);
        let big = || ("big.txt".to_string(), true);
        let focused_z = || Some("z.txt".to_string());
        assert_eq!(
            [before, after, listed(&h)],
            [
                (vec![z()], focused_z()),
                (vec![big(), z()], focused_z()),
                (vec![big(), z()], focused_z()),
            ],
        );
    }

    /// One stack parks at a time, so a flip from a changes view in another tab
    /// releases the stack an earlier flip parked.
    #[test]
    fn a_second_flip_releases_the_stack_parked_before_it() {
        let mut h = long_files(24);
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let first_parked: Vec<EditorId> = excerpt_ids(&h).into_iter().map(|(id, _)| id).collect();
        h.type_keys("Ctrl-t");
        h.settle();

        dispatch(&mut h.stoat, &stoat_action::PrevTab);
        h.type_keys("Ctrl-t");
        h.settle();
        let ws = h.stoat.active_workspace();
        let kept: Vec<bool> = first_parked
            .iter()
            .map(|id| ws.editors.contains_key(*id))
            .collect();
        assert_eq!((first_parked.len(), kept), (2, vec![false, false]));
    }

    /// DiffFiles on the changes view opens the cursor's file in the per-file
    /// diff on the cursor's line, as Ctrl-t does.
    #[test]
    fn diff_files_on_the_changes_view_opens_the_cursor_hunk_in_the_diff() {
        let mut h = deep_files();
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        h.type_keys("n n");

        dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        h.settle();
        assert_eq!(
            (
                h.stoat.current_view(),
                focused_path(&h),
                editor::focused_cursor_point(&mut h.stoat).row,
            ),
            (Some("diff"), Some(PathBuf::from("/repo/b.txt")), 8),
        );
    }

    /// n and p run the editor's change walk, which goes on from an excerpt's
    /// last hunk into the next excerpt, and a step past either end of the stack
    /// stays and says so.
    #[test]
    fn n_and_p_walk_the_hunks_across_excerpts_and_stop_at_either_end() {
        let mut h = deep_files();
        h.cover_with_diff_changes(false);
        h.settle();
        let walk: Vec<(String, u32)> = ["n", "n", "n", "p", "p"]
            .into_iter()
            .map(|key| {
                h.type_keys(key);
                place(&mut h)
            })
            .collect();
        assert_eq!(
            (walk, h.stoat.pending_message.as_deref()),
            (
                vec![
                    at("a.txt", 8),
                    at("b.txt", 8),
                    at("b.txt", 8),
                    at("a.txt", 8),
                    at("a.txt", 8)
                ],
                Some("no more changes"),
            ),
        );
    }

    #[test]
    fn an_open_before_the_first_excerpt_keeps_the_changes_view() {
        let mut h = deep_files();
        h.cover_with_diff_changes(false);
        dispatch(&mut h.stoat, &stoat_action::DiffChangesOpen);
        assert_eq!(
            (h.stoat.current_view(), h.stoat.pending_message.as_deref()),
            (Some("diff_changes"), Some("no change under the cursor")),
        );
    }

    /// A page steps by the rows of the last paint, a half page by half of them
    /// and by one row before any paint, and either one goes on into the next
    /// file at an excerpt's edge. An edge lands on the stack's first or last
    /// row.
    #[test]
    fn pages_step_through_the_stack_and_edges_reach_the_ends() {
        let mut h = deep_files();
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        h.cover_with_diff_changes(false);
        h.settle();
        h.render_composited();
        let set_viewport = |h: &mut TestHarness, rows: usize| {
            if let Some(state) = h.stoat.active_workspace_mut().focused_diff_changes_mut() {
                state.viewport_rows = rows;
            }
        };

        set_viewport(&mut h, 0);
        dispatch(&mut h.stoat, &stoat_action::DiffChangesHalfPageDown);
        let mut places = vec![place(&mut h)];
        set_viewport(&mut h, 4);
        let actions: [&dyn Action; 6] = [
            &stoat_action::DiffChangesPageDown,
            &stoat_action::DiffChangesHalfPageDown,
            &stoat_action::DiffChangesHalfPageUp,
            &stoat_action::DiffChangesPageUp,
            &stoat_action::DiffChangesLast,
            &stoat_action::DiffChangesFirst,
        ];
        for action in actions {
            dispatch(&mut h.stoat, action);
            places.push(place(&mut h));
        }
        assert_eq!(
            places,
            [
                at("a.txt", 6),
                at("b.txt", 0),
                at("b.txt", 6),
                at("b.txt", 0),
                at("a.txt", 6),
                at("b.txt", 9),
                at("a.txt", 5)
            ],
        );
    }

    /// The side buttons walk the hunks across the excerpts as n and p do. A
    /// hop into the next excerpt records its landing, so a later press walks on
    /// rather than retracing the jumplist out of the changes view.
    #[test]
    fn the_side_buttons_walk_the_hunks_across_excerpts() {
        let mut h = deep_files();
        h.cover_with_diff_changes(false);
        h.settle();
        let presses: [&dyn Action; 4] = [
            &stoat_action::DiffBack,
            &stoat_action::DiffBack,
            &stoat_action::DiffBack,
            &stoat_action::DiffForward,
        ];
        let walk: Vec<(Option<&str>, (String, u32))> = presses
            .into_iter()
            .map(|action| {
                dispatch(&mut h.stoat, action);
                (h.stoat.current_view(), place(&mut h))
            })
            .collect();
        assert_eq!(
            walk,
            [
                (Some("diff_changes"), at("a.txt", 8)),
                (Some("diff_changes"), at("b.txt", 8)),
                (Some("diff_changes"), at("b.txt", 8)),
                (Some("diff_changes"), at("a.txt", 8)),
            ],
        );
    }

    /// `space G s` on an excerpt's hunk stages it, and the refold that follows
    /// the stage keeps the cursor on its buffer position.
    #[test]
    fn space_g_s_stages_an_excerpt_hunk_and_the_refold_keeps_the_cursor() {
        let mut h = deep_files();
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/b.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        h.type_keys("n");
        let folded = |h: &TestHarness| {
            h.stoat
                .active_workspace()
                .focused_diff_changes()
                .and_then(DiffChangesState::focused_excerpt)
                .and_then(|excerpt| excerpt.folded_version)
        };
        let before = (place(&mut h), folded(&h));

        h.type_keys("space G s");
        h.settle_diff_jobs();
        let patches = h.fake_git().applied_patches(Path::new("/repo")).len();
        assert_eq!(
            (
                before.0.clone(),
                patches,
                place(&mut h),
                folded(&h) != before.1
            ),
            (at("b.txt", 8), 1, before.0, true),
        );
    }

    /// A landing on an unsaved edit waits for the diff map that holds the edit,
    /// and lands on the edit once its file's excerpt is folded.
    #[test]
    fn a_landing_on_a_fresh_edit_waits_for_the_map_that_holds_it() {
        let mut h = deep_files();
        track_clean_c(&mut h);
        h.open_file(Path::new("/repo/c.txt"));
        h.type_keys("j i Z Escape");

        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        let waiting = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .and_then(|state| state.pending_landing.as_ref())
            .map(|landing| (landing.rel_path.clone(), landing.row));
        h.settle();
        assert_eq!(
            (waiting, place(&mut h)),
            (Some(("c.txt".to_string(), 1)), at("c.txt", 1)),
        );
    }

    /// Either half of the diff view, the pane's latch or the editor's flag,
    /// counts as a diff that the changes view's close returns to.
    #[test]
    fn either_half_of_the_diff_view_brings_the_close_back_to_the_diff() {
        let views = [true, false].map(|latch| {
            let mut h = deep_files();
            h.open_file(Path::new("/repo/a.txt"));
            if latch {
                let ws = h.stoat.active_workspace_mut();
                let focus = ws.panes.focus();
                ws.panes.pane_mut(focus).diff_mode = true;
            } else if let Some(editor) = focused_editor_mut(&mut h.stoat) {
                editor.set_diff_view(true);
            }
            dispatch(&mut h.stoat, &stoat_action::DiffChanges);
            h.type_keys("q");
            h.stoat.current_view()
        });
        assert_eq!(views, [Some("diff"); 2]);
    }

    /// An open from a changes view over a commits list puts the list back
    /// first, so the list waits behind the opened file as it waits behind any
    /// diff opened from it.
    #[test]
    fn an_open_from_a_changes_view_over_a_list_keeps_the_list_behind_the_file() {
        let mut h = deep_files();
        h.open_commits("/repo");
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        dispatch(&mut h.stoat, &stoat_action::DiffChangesOpen);
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.current_view(),
                ws.commit_lists.len(),
                commits::covered_commits(ws, ws.panes.focus()).is_some(),
            ),
            (Some("diff"), 1, true),
        );
    }

    /// The open keeps the cursor's line on the screen row it held in the
    /// changes view, which the longer file reaches with a scroll.
    #[test]
    fn an_open_holds_the_cursor_screen_row() {
        let mut h = long_files(24);
        h.cover_with_diff_changes(false);
        h.settle();
        let stacked = screen_row(&mut h);

        dispatch(&mut h.stoat, &stoat_action::DiffChangesOpen);
        let opened = screen_row(&mut h);
        let scroll = focused_editor_mut(&mut h.stoat).map_or(0, |editor| editor.scroll_row);
        assert_eq!(
            (stacked.is_some(), opened, scroll > 0),
            (true, stacked, true)
        );
    }

    /// A landing from a plain editor on a line the stack shows keeps the line
    /// on the screen row it held in the editor, though the least scroll that
    /// shows it puts it on the last row.
    #[test]
    fn a_landing_from_a_plain_editor_holds_its_screen_row() {
        let mut h = long_files(12);
        h.open_file(Path::new("/repo/b.txt"));
        h.type_keys("4 1 G");
        let plain = screen_row(&mut h);

        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        assert_eq!(
            (plain.is_some(), place(&mut h), screen_row(&mut h)),
            (true, at("b.txt", 40), plain),
        );
    }

    /// A landing on a file with no change lands on the first excerpt's first
    /// shown row once the list shows that the file has none.
    #[test]
    fn a_landing_on_a_clean_file_lands_on_the_first_row() {
        let mut h = deep_files();
        track_clean_c(&mut h);
        h.open_file(Path::new("/repo/c.txt"));
        h.type_keys("j");
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let pending = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .map(|state| state.pending_landing.clone());
        assert_eq!((place(&mut h), pending), (at("a.txt", 5), Some(None)));
    }

    /// Track c.txt in the repository with no change.
    fn track_clean_c(h: &mut TestHarness) {
        h.fake_git().add_repo("/repo").head_file("c.txt", "x\ny\n");
        h.fake_fs().insert_file(Path::new("/repo/c.txt"), b"x\ny\n");
    }
}
