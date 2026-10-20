//! The handlers of a [`View::DiffChanges`] pane.
//!
//! They open and close the changes view, flip between it and the per-file diff,
//! step its cursor, and respawn a pane whose state did not survive a restore.

use super::{
    movement::{self, ChangeDir},
    pane, review,
    view::{self, PageDir, ViewAlign},
};
use crate::{
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    buffer_registry::OpenOrigin,
    diff_changes::{self, DiffChangesState},
    pane::{PaneId, View},
    render::commits::preview_row_count,
    review_session::DiffDocument,
    workspace::Workspace,
};
use std::{path::PathBuf, sync::Arc};
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
/// A covered view that is gone closes the pane instead, and a last pane gets a
/// fresh scratch editor. A changes view opened from a per-file diff returns to
/// that diff, so a close peels back one layer.
pub(crate) fn close_diff_changes(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_diff_changes_id() else {
        return UpdateEffect::None;
    };
    let (covered, covered_diff) = ws
        .diff_changes_views
        .remove(id)
        .map_or((None, false), |state| (state.covered, state.covered_diff));
    let focus = ws.panes.focus();
    if ws.panes.widened() == Some(focus) {
        ws.panes.unwiden();
    }

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
    UpdateEffect::Redraw
}

/// Close the changes view and open the file its cursor row shows in the
/// per-file diff, on the line that the row shows.
///
/// The file opens in the pane after the changes view's close puts back the view
/// it covered. A row with no file under it, as before the first document
/// lands, leaves the changes view open and says so in the status row.
pub(super) fn diff_changes_open(stoat: &mut Stoat) -> UpdateEffect {
    let target = {
        let ws = stoat.active_workspace();
        let cursor = ws.focused_diff_changes().map(|state| state.cursor);
        cursor
            .zip(document(ws))
            .and_then(|(cursor, doc)| diff_changes::target_of(&doc, cursor))
    };
    let Some(((rel_path, row), workdir)) = target.zip(repo_workdir(stoat)) else {
        stoat.set_status("no change under the cursor");
        return UpdateEffect::Redraw;
    };

    if let Some(state) = stoat.active_workspace_mut().focused_diff_changes_mut() {
        state.covered_diff = false;
    }
    close_diff_changes(stoat);

    let focus = stoat.active_workspace().panes.focus();
    let path = workdir.join(rel_path);
    if buffer_lifecycle::open_file_in_pane_now(stoat, focus, &path, OpenOrigin::Visited).is_none() {
        return UpdateEffect::Redraw;
    }

    let offset = super::focused_editor_mut(stoat).map(|editor| {
        let buffer = editor.display_map.buffer_snapshot();
        buffer.rope().point_to_offset(Point::new(row, 0))
    });
    if let Some(offset) = offset {
        movement::jump_to_offset(stoat, offset);
    }
    review::enter_diff_view(stoat);
    if let Some(editor) = super::focused_editor_mut(stoat) {
        view::align_view(editor, ViewAlign::Center);
    }
    UpdateEffect::Redraw
}

/// Move the focused changes view's cursor by `delta` rows, held inside the
/// document's rows.
pub(crate) fn diff_changes_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let rows = document(ws).map_or(0, |doc| preview_row_count(&doc));
    let Some(state) = ws.focused_diff_changes_mut() else {
        return UpdateEffect::None;
    };
    if rows == 0 {
        return UpdateEffect::None;
    }
    state.cursor = state
        .cursor
        .saturating_add_signed(delta as isize)
        .min(rows - 1);
    UpdateEffect::Redraw
}

/// Move the focused changes view's cursor to the header of the next or the
/// previous chunk.
///
/// A step back from inside a chunk lands on that chunk's own header. At either
/// end the cursor stays, and the status row says that no more changes lie that
/// way, as the change walk of the per-file diff says.
pub(super) fn diff_changes_chunk_step(stoat: &mut Stoat, dir: ChangeDir) -> UpdateEffect {
    let target = {
        let ws = stoat.active_workspace();
        let cursor = ws.focused_diff_changes().map(|state| state.cursor);
        let Some((cursor, doc)) = cursor.zip(document(ws)) else {
            return UpdateEffect::None;
        };
        let starts = diff_changes::chunk_starts(&doc);
        match dir {
            ChangeDir::Next => starts.into_iter().find(|&start| start > cursor),
            ChangeDir::Prev => starts.into_iter().rev().find(|&start| start < cursor),
        }
    };
    let Some(row) = target else {
        stoat.set_status("no more changes");
        return UpdateEffect::Redraw;
    };
    diff_changes_select(stoat, row)
}

/// Move the focused changes view's cursor by the rows of its last paint.
pub(super) fn diff_changes_page(stoat: &mut Stoat, dir: PageDir) -> UpdateEffect {
    let rows = viewport_rows(stoat);
    diff_changes_step(stoat, page_delta(rows, dir))
}

/// Move the focused changes view's cursor by half the rows of its last paint,
/// and by one row at least.
pub(super) fn diff_changes_half_page(stoat: &mut Stoat, dir: PageDir) -> UpdateEffect {
    let rows = (viewport_rows(stoat) / 2).max(1);
    diff_changes_step(stoat, page_delta(rows, dir))
}

/// Put the focused changes view's cursor on the first row, or on the last row
/// when `last` is set.
pub(super) fn diff_changes_edge(stoat: &mut Stoat, last: bool) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let rows = document(ws).map_or(0, |doc| preview_row_count(&doc));
    let Some(state) = ws.focused_diff_changes_mut() else {
        return UpdateEffect::None;
    };
    state.cursor = match last {
        true => rows.saturating_sub(1),
        false => 0,
    };
    UpdateEffect::Redraw
}

/// Put the focused changes view's cursor on `row`, as a press on that row does.
pub(crate) fn diff_changes_select(stoat: &mut Stoat, row: usize) -> UpdateEffect {
    let Some(state) = stoat.active_workspace_mut().focused_diff_changes_mut() else {
        return UpdateEffect::None;
    };
    state.cursor = row;
    UpdateEffect::Redraw
}

/// Give a fresh state to every changes view pane of the active workspace whose
/// state did not survive, then start the document's build.
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
    diff_changes::ensure_diff_changes_doc(stoat);
}

/// Show a changes view in the focused pane, over the view the pane showed.
///
/// A focused editor's cursor line waits as the landing, so the changes view
/// opens on that line's chunk once a document exists. An editor in the diff
/// view leaves it first, so the editor comes back plain beneath the changes
/// view, and the close returns to the diff.
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

    let ws = stoat.active_workspace_mut();
    let focus = ws.panes.focus();
    let id = ws.diff_changes_views.insert(DiffChangesState {
        covered: Some(covered),
        covered_diff,
        pending_landing: landing,
        ..DiffChangesState::default()
    });
    ws.panes.pane_mut(focus).view = View::DiffChanges(id);
    ws.panes.widen(focus);

    diff_changes::ensure_diff_changes_doc(stoat);
    diff_changes::land_pending(stoat.active_workspace_mut());
    UpdateEffect::Redraw
}

/// The focused editor's file, relative to the repository, and the row of its
/// cursor.
///
/// `None` when no editor has focus, or when its buffer has no file inside the
/// repository.
fn editor_landing(stoat: &mut Stoat) -> Option<(String, u32)> {
    let workdir = repo_workdir(stoat)?;
    let (_, buffer_id) = stoat.focused_editor_ids()?;
    let rel_path = {
        let path = stoat.active_workspace().buffers.path_for(buffer_id)?;
        path.strip_prefix(&workdir).ok()?.display().to_string()
    };

    let editor = super::focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let selection = editor.selections.newest_anchor();
    let cursor = stoat_text::cursor_offset(
        buffer.rope(),
        buffer.resolve_anchor(&selection.tail()),
        buffer.resolve_anchor(&selection.head()),
    );
    Some((rel_path, buffer.rope().offset_to_point(cursor).row))
}

/// The working directory of the repository that holds the active workspace.
///
/// The changes view names each file relative to it.
fn repo_workdir(stoat: &Stoat) -> Option<PathBuf> {
    stoat
        .git_host
        .discover(&stoat.active_workspace().git_root)?
        .workdir()
}

/// The workspace's last changes view document, which every changes view pane
/// shows.
fn document(ws: &Workspace) -> Option<Arc<DiffDocument>> {
    ws.diff_changes_doc
        .as_ref()
        .and_then(|built| built.doc.clone())
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

#[cfg(test)]
mod tests {
    use super::{diff_changes_step, document, respawn_diff_changes_panes};
    use crate::{
        action_handlers::{commits, dispatch, focused_editor_mut, pane, review::diff_cache_key},
        diff_changes::{
            chunk_starts, ensure_diff_changes_doc, target_of, DiffChangesId, DiffChangesStamp,
            DiffChangesState,
        },
        pane::{Axis, DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::commits::preview_row_count,
        review_session::DiffDocument,
        run::pty::PtyNotification,
        term_session::TermId,
        test_harness::{editor, TestHarness},
    };
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_action::Action;

    /// Two changed files, with `a.txt` open in the focused pane.
    fn two_files() -> TestHarness {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        h
    }

    /// Two changed files whose one change sits on the last of nine lines, so
    /// the lines above the change lie outside its chunk.
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

    /// The active workspace's changes view document.
    fn doc(h: &TestHarness) -> Arc<DiffDocument> {
        document(h.stoat.active_workspace()).expect("a built changes view document")
    }

    /// The row of the focused changes view's cursor.
    fn cursor(h: &TestHarness) -> usize {
        h.stoat
            .active_workspace()
            .focused_diff_changes()
            .expect("a focused changes view")
            .cursor
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

    /// The changes view's keys read its own mode, which starts at normal, and
    /// the diff dials step on it as on the diff view.
    #[test]
    fn a_changes_view_pane_holds_its_own_mode_and_answers_the_dials() {
        let mut h = two_files();
        h.cover_with_diff_changes(false);
        let fresh = h.stoat.focused_mode().to_string();
        h.stoat.set_focused_mode("space".to_string());
        let mode = h.stoat.focused_mode().to_string();
        h.stoat.handle_diff_tint_step(1);
        let state_mode = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .map(|state| state.mode.clone());
        assert_eq!(
            (
                h.stoat.current_view(),
                fresh,
                mode,
                state_mode,
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
    /// editor and rests the changes view's chord, and a close of its pane drops
    /// it with the editor it covered.
    #[test]
    fn a_changes_view_stays_with_its_own_pane() {
        let mut h = two_files();
        let (editor, _) = h.stoat.focused_editor_ids().expect("editor");
        let id = h.cover_with_diff_changes(false);
        h.stoat.set_focused_mode("space".to_string());
        let source = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.unwiden();

        pane::split_pane(&mut h.stoat, Axis::Vertical);
        let (kept, split_view, mode) = {
            let ws = h.stoat.active_workspace();
            (
                ws.panes.pane(source).view.clone(),
                ws.panes.pane(ws.panes.focus()).view.clone(),
                ws.diff_changes_views[id].mode.clone(),
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
            (View::DiffChanges(id), true, "normal".to_string(), 0, false),
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

    /// A step holds the cursor inside the document's rows, and a step before
    /// the first document lands moves nothing.
    #[test]
    fn a_changes_view_step_stays_inside_the_document() {
        let cursor = |h: &TestHarness| {
            h.stoat
                .active_workspace()
                .focused_diff_changes()
                .map(|state| state.cursor)
        };
        let mut h = two_files();
        h.cover_with_diff_changes(false);
        diff_changes_step(&mut h.stoat, 1);
        let unbuilt = cursor(&h);

        h.settle();
        let rows = h
            .stoat
            .active_workspace()
            .diff_changes_doc
            .as_ref()
            .and_then(|built| built.doc.as_deref())
            .map_or(0, preview_row_count);
        diff_changes_step(&mut h.stoat, -1);
        let above_first = cursor(&h);
        diff_changes_step(&mut h.stoat, 100);
        assert_eq!(
            (unbuilt, above_first, cursor(&h)),
            (Some(0), Some(0), Some(rows - 1)),
        );
    }

    /// Keys go to a focused dock, so the changes view in the split pane beneath
    /// it lends the dock no mode.
    #[test]
    fn a_focused_dock_takes_no_mode_from_the_changes_view() {
        let mut h = two_files();
        h.cover_with_diff_changes(false);
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
    /// of its own and a document, though another pane holds focus. A live
    /// changes view keeps its state, and a respawn that finds no dead pane
    /// builds nothing.
    #[test]
    fn a_restored_changes_view_pane_gets_a_fresh_state_and_a_document() {
        let mut h = two_files();
        respawn_diff_changes_panes(&mut h.stoat);
        h.settle();
        let built_for_no_pane = h.stoat.active_workspace().diff_changes_doc.is_some();

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
            View::DiffChanges(id) => ws
                .diff_changes_views
                .get(id)
                .map(|state| (state.cursor, state.covered.clone())),
            _ => None,
        };
        let files: Vec<&str> = ws
            .diff_changes_doc
            .as_ref()
            .and_then(|built| built.doc.as_deref())
            .map(|doc| {
                doc.files
                    .iter()
                    .map(|file| file.rel_path.as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            (built_for_no_pane, kept, state, files),
            (
                false,
                (View::DiffChanges(live_id), 2),
                Some((0, None)),
                vec!["a.txt", "b.txt"]
            ),
        );
    }

    /// Only a changes view on screen follows the tree. After a git event a
    /// frame rebuilds the document under one and leaves it stale under a plain
    /// pane.
    #[test]
    fn a_focused_changes_view_rebuilds_when_the_tree_moves() {
        let current = |h: &TestHarness| {
            let ws = h.stoat.active_workspace();
            ws.diff_changes_doc
                .as_ref()
                .is_some_and(|built| built.stamp == DiffChangesStamp::read(ws))
        };
        let mut h = two_files();
        ensure_diff_changes_doc(&mut h.stoat);
        h.settle();
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.stoat.drive_background();
        let plain = current(&h);

        h.cover_with_diff_changes(false);
        h.settle();
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.stoat.drive_background();
        let focused = current(&h);
        assert_eq!((plain, focused), (false, true));
    }

    /// From a plain editor the changes view opens on the editor's line once the
    /// first document lands. A line above every change of its file lands on
    /// the header of the chunk below it.
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
                cursor(&h),
            ),
            (Some("diff_changes"), true, chunk_starts(&doc(&h))[1]),
        );
    }

    /// From the per-file diff the changes view opens on the diff's line, with
    /// the editor plain beneath it, and a close returns to the diff.
    #[test]
    fn diff_changes_from_the_diff_lands_on_its_line_and_q_returns_to_the_diff() {
        let mut h = deep_files();
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        h.settle();
        let (editor, _) = h.stoat.focused_editor_ids().expect("editor");

        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        let landed = {
            let (doc, at) = (doc(&h), cursor(&h));
            (target_of(&doc, at), chunk_starts(&doc).contains(&at))
        };
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
            (
                (Some(("a.txt".to_string(), 8)), false),
                true,
                Some("diff"),
                Some(editor),
            ),
        );
    }

    /// Ctrl-t on the changes view opens the cursor's file in the per-file diff
    /// on the row's line, and Ctrl-t there returns to the changes view on that
    /// line.
    #[test]
    fn ctrl_t_flips_between_the_changes_view_and_the_diff_on_one_line() {
        let mut h = deep_files();
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        h.type_keys("n Ctrl-t");
        h.settle();
        let opened = (
            h.stoat.current_view(),
            focused_path(&h),
            editor::focused_cursor_point(&mut h.stoat).row,
        );

        h.type_keys("Ctrl-t");
        h.settle();
        let back = {
            let (doc, at) = (doc(&h), cursor(&h));
            (
                h.stoat.current_view(),
                target_of(&doc, at),
                chunk_starts(&doc).contains(&at),
            )
        };
        assert_eq!(
            (opened, back),
            (
                (Some("diff"), Some(PathBuf::from("/repo/b.txt")), 8),
                (Some("diff_changes"), Some(("b.txt".to_string(), 8)), false),
            ),
        );
    }

    /// n and p walk the chunk headers, and j and k step one row. A p from
    /// inside a chunk lands on that chunk's header, and a step past either end
    /// stays and says so.
    #[test]
    fn n_and_p_walk_the_chunk_headers_and_stop_at_either_end() {
        let mut h = deep_files();
        h.cover_with_diff_changes(false);
        h.settle();
        let starts = chunk_starts(&doc(&h));
        let walk: Vec<usize> = ["n", "n", "j", "j", "k", "p", "p", "p"]
            .into_iter()
            .map(|key| {
                h.type_keys(key);
                cursor(&h)
            })
            .collect();
        assert_eq!(
            (walk, h.stoat.pending_message.as_deref()),
            (
                vec![
                    starts[1],
                    starts[1],
                    starts[1] + 1,
                    starts[1] + 2,
                    starts[1] + 1,
                    starts[1],
                    starts[0],
                    starts[0]
                ],
                Some("no more changes"),
            ),
        );
    }

    #[test]
    fn an_open_before_the_first_document_keeps_the_changes_view() {
        let mut h = deep_files();
        h.cover_with_diff_changes(false);
        dispatch(&mut h.stoat, &stoat_action::DiffChangesOpen);
        assert_eq!(
            (h.stoat.current_view(), h.stoat.pending_message.as_deref()),
            (Some("diff_changes"), Some("no change under the cursor")),
        );
    }

    /// A page steps by the rows of the last paint, a half page by half of them
    /// and by one row before any paint, and an edge lands on the first or the
    /// last row.
    #[test]
    fn pages_step_by_the_painted_rows_and_edges_reach_the_ends() {
        let mut h = deep_files();
        h.cover_with_diff_changes(false);
        h.settle();
        let rows = preview_row_count(&doc(&h));
        let set_viewport = |h: &mut TestHarness, rows: usize| {
            if let Some(state) = h.stoat.active_workspace_mut().focused_diff_changes_mut() {
                state.viewport_rows = rows;
            }
        };

        set_viewport(&mut h, 0);
        dispatch(&mut h.stoat, &stoat_action::DiffChangesHalfPageDown);
        let mut cursors = vec![cursor(&h)];
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
            cursors.push(cursor(&h));
        }
        assert_eq!(cursors, [1, 5, 7, 5, 1, rows - 1, 0]);
    }

    /// A rebuild keeps the cursor on its chunk though a grown chunk above it
    /// moves the chunk's rows down, and the landing the changes view opened on
    /// does not pull the cursor back.
    #[test]
    fn a_rebuild_keeps_the_cursor_on_its_chunk() {
        let last_start = |h: &TestHarness| *chunk_starts(&doc(h)).last().expect("a chunk");
        let mut h = deep_files();
        h.open_file(Path::new("/repo/b.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let before = last_start(&h);
        if let Some(state) = h.stoat.active_workspace_mut().focused_diff_changes_mut() {
            state.cursor = before + 2;
        }

        h.fake_fs().insert_file(
            Path::new("/repo/a.txt"),
            b"1\n2\n3\n4\nFIVE\n6\n7\n8\nNINE\n",
        );
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.stoat.drive_background();
        h.settle();
        let after = last_start(&h);
        assert_eq!((after > before, cursor(&h)), (true, after + 2));
    }

    /// A landing on an unsaved edit waits out the document built before the
    /// edit, and lands on the edit once the next build holds it.
    #[test]
    fn a_landing_on_a_fresh_edit_waits_for_the_build_that_holds_it() {
        let mut h = deep_files();
        track_clean_c(&mut h);
        ensure_diff_changes_doc(&mut h.stoat);
        h.settle();
        h.open_file(Path::new("/repo/c.txt"));
        h.type_keys("j i Z Escape");

        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        let waiting = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .and_then(|state| state.pending_landing.clone());
        h.settle();
        assert_eq!(
            (waiting, target_of(&doc(&h), cursor(&h))),
            (
                Some(("c.txt".to_string(), 1)),
                Some(("c.txt".to_string(), 1)),
            ),
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

    /// The open centers the line of its hunk in the view, as the align command
    /// centers it.
    #[test]
    fn an_open_centers_the_hunk_line_in_the_view() {
        let mut h = TestHarness::with_size(100, 24);
        let base: String = (1..=60).map(|n| format!("{n}\n")).collect();
        let working = base.replace("\n41\n", "\nFORTY-ONE\n");
        h.stage_review_scenario("/repo", &[("long.txt", &base, &working)]);
        h.cover_with_diff_changes(false);
        h.settle();
        let scroll = |h: &mut TestHarness| {
            focused_editor_mut(&mut h.stoat).map_or(0, |editor| editor.scroll_row)
        };

        dispatch(&mut h.stoat, &stoat_action::DiffChangesOpen);
        let opened = scroll(&mut h);
        dispatch(&mut h.stoat, &stoat_action::AlignViewCenter);
        assert_eq!((opened, opened > 0), (scroll(&mut h), true));
    }

    /// A landing on a file with no change lands on the first row once a
    /// current document shows that the file has none.
    #[test]
    fn a_landing_on_a_clean_file_lands_on_the_first_row() {
        let mut h = deep_files();
        track_clean_c(&mut h);
        h.open_file(Path::new("/repo/c.txt"));
        h.type_keys("j");
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let state = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .map(|state| (state.cursor, state.pending_landing.clone()));
        assert_eq!(state, Some((0, None)));
    }

    /// A second changes view after one edit to one open file diffs that file
    /// alone. Every other file keeps its cache key, so its lookup hits, and
    /// the rebuild adds the one entry of the edit.
    ///
    /// Empty hunks planted under the untouched files' keys tell a hit from a
    /// fresh diff, because a fresh diff shows each file's one chunk.
    #[test]
    fn a_second_changes_view_after_one_edit_diffs_the_edited_file_alone() {
        let untouched = [("b\n", "B\n"), ("c\n", "C\n"), ("d\n", "D\n")];
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario(
            "/repo",
            &[
                ("a.txt", "a\n", "A\n"),
                ("b.txt", "b\n", "B\n"),
                ("c.txt", "c\n", "C\n"),
                ("d.txt", "d\n", "D\n"),
            ],
        );
        h.open_file(Path::new("/repo/a.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        h.type_keys("q i x Escape");
        h.settle();
        let before = {
            let cache = h.stoat.diff_cache();
            let mut cache = cache.lock().expect("diff_cache poisoned");
            for (base, buffer) in untouched {
                cache.insert(
                    diff_cache_key(base, buffer, None),
                    Arc::new(Vec::new()),
                    true,
                );
            }
            cache.len()
        };

        dispatch(&mut h.stoat, &stoat_action::DiffChanges);
        h.settle();
        let added = h
            .stoat
            .diff_cache()
            .lock()
            .expect("diff_cache poisoned")
            .len()
            - before;
        let files: Vec<(String, String, usize)> = doc(&h)
            .files
            .iter()
            .map(|file| {
                let text = file.buffer_text.to_string();
                (file.rel_path.clone(), text, file.chunks.len())
            })
            .collect();
        let expected = [
            ("a.txt", "xA\n", 1),
            ("b.txt", "B\n", 0),
            ("c.txt", "C\n", 0),
            ("d.txt", "D\n", 0),
        ]
        .map(|(path, text, chunks)| (path.to_string(), text.to_string(), chunks));
        assert_eq!((added, files), (1, expected.to_vec()));
    }

    /// Track c.txt in the repository with no change.
    fn track_clean_c(h: &mut TestHarness) {
        h.fake_git().add_repo("/repo").head_file("c.txt", "x\ny\n");
        h.fake_fs().insert_file(Path::new("/repo/c.txt"), b"x\ny\n");
    }
}
