//! The handlers of a [`View::Overview`] pane: its close, its cursor steps, and
//! the respawn of a pane whose state did not survive a restore.

use super::{pane, review};
use crate::{
    app::{Stoat, UpdateEffect},
    overview::{self, OverviewState},
    pane::{PaneId, View},
    render::commits::preview_row_count,
};

/// Close the overview the focused pane shows and put back the view it covered.
///
/// A covered view that is gone closes the pane instead, and a last pane gets a
/// fresh scratch editor. An overview opened from a per-file diff returns to
/// that diff, so a close peels back one layer.
pub(crate) fn close_overview(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(id) = ws.focused_overview_id() else {
        return UpdateEffect::None;
    };
    let (covered, covered_diff) = ws
        .overviews
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

/// Move the focused overview's cursor by `delta` rows, held inside the
/// document's rows.
pub(crate) fn overview_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let rows = ws
        .overview_doc
        .as_ref()
        .and_then(|built| built.doc.as_deref())
        .map_or(0, preview_row_count);
    let Some(state) = ws.focused_overview_mut() else {
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

/// Put the focused overview's cursor on `row`, as a press on that row does.
pub(crate) fn overview_select(stoat: &mut Stoat, row: usize) -> UpdateEffect {
    let Some(state) = stoat.active_workspace_mut().focused_overview_mut() else {
        return UpdateEffect::None;
    };
    state.cursor = row;
    UpdateEffect::Redraw
}

/// Give a fresh state to every overview pane of the active workspace whose
/// state did not survive, then start the document's build.
///
/// The panes of every tab count, a parked one included, so a tab shows a live
/// overview when the reader next switches to it.
///
/// An overview pane rides `PaneTree` serde as [`View::Overview`], but its state
/// is live state the session file does not hold, so the id is dead after a
/// restore or a workspace copy. A fresh state covers no view, so its close
/// leaves a scratch editor.
pub(crate) fn respawn_overview_panes(stoat: &mut Stoat) {
    let dead_panes = {
        let ws = stoat.active_workspace();
        ws.pane_trees()
            .enumerate()
            .flat_map(|(tree, panes)| {
                panes
                    .split_pane_ids()
                    .into_iter()
                    .filter_map(move |id| match panes.pane(id).view {
                        View::Overview(dead) if !ws.overviews.contains_key(dead) => {
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
        let id = ws.overviews.insert(OverviewState::default());
        if let Some(panes) = ws.pane_trees_mut().nth(tree) {
            panes.pane_mut(pane_id).view = View::Overview(id);
        }
    }
    overview::ensure_overview_doc(stoat);
}

#[cfg(test)]
mod tests {
    use super::{overview_step, respawn_overview_panes};
    use crate::{
        action_handlers::{dispatch, pane},
        overview::{ensure_overview_doc, OverviewId, OverviewStamp, OverviewState},
        pane::{Axis, DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::commits::preview_row_count,
        run::pty::PtyNotification,
        term_session::TermId,
        test_harness::TestHarness,
    };
    use std::path::Path;
    use stoat_action::Action;

    /// Two changed files, with `a.txt` open in the focused pane.
    fn two_files() -> TestHarness {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        h
    }

    /// A second tab of two panes whose focused pane shows a shell over an
    /// overview, and that shell.
    fn shell_over_an_overview() -> (TestHarness, TermId) {
        let mut h = two_files();
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.cover_with_overview(false);
        dispatch(&mut h.stoat, &stoat_action::Terminal);
        let ws = h.stoat.active_workspace();
        let View::Terminal(shell) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the terminal action shows a terminal");
        };
        (h, shell)
    }

    /// The overview's keys read its own mode, which starts at normal, and the
    /// diff dials step on it as on the diff view.
    #[test]
    fn an_overview_pane_holds_its_own_mode_and_answers_the_dials() {
        let mut h = two_files();
        h.cover_with_overview(false);
        let fresh = h.stoat.focused_mode().to_string();
        h.stoat.set_focused_mode("space".to_string());
        let mode = h.stoat.focused_mode().to_string();
        h.stoat.handle_diff_tint_step(1);
        let state_mode = h
            .stoat
            .active_workspace()
            .focused_overview()
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
                Some("overview"),
                "normal".to_string(),
                "space".to_string(),
                Some("space".to_string()),
                1
            ),
        );
    }

    /// A layout key on the overview closes it, as it closes every full-window
    /// screen, and the pane shows the editor the overview covered at its own
    /// width and in normal mode. An overview opened from the per-file diff
    /// returns to that diff, which widens the pane again.
    #[test]
    fn a_layout_key_closes_the_overview_to_the_view_it_covered() {
        for (covered_diff, view, widened) in [(false, "file", false), (true, "diff", true)] {
            let mut h = two_files();
            let (editor, _) = h.stoat.focused_editor_ids().expect("editor");
            h.stoat.set_focused_mode("insert".to_string());
            h.cover_with_overview(covered_diff);

            dispatch(&mut h.stoat, &stoat_action::SplitRight);
            let ws = h.stoat.active_workspace();
            assert_eq!(
                (
                    ws.overviews.len(),
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
                    Some("overview closed")
                ),
                "covered_diff {covered_diff}",
            );
        }
    }

    /// An overview over a dead view closes its pane, and a last pane gets a
    /// scratch editor.
    #[test]
    fn closing_an_overview_over_a_label_closes_the_pane_or_falls_back_to_a_scratch() {
        let mut h = two_files();
        dispatch(&mut h.stoat, &stoat_action::SplitRight);

        let mut closed = Vec::new();
        for _ in 0..2 {
            let ws = h.stoat.active_workspace_mut();
            let focus = ws.panes.focus();
            ws.panes.pane_mut(focus).view = View::Label("label".into());
            h.cover_with_overview(false);
            dispatch(&mut h.stoat, &stoat_action::SplitRight);
            closed.push((
                h.stoat.active_workspace().panes.split_pane_ids().len(),
                h.stoat.current_view(),
            ));
        }
        assert_eq!(closed, [(1, Some("file")), (1, Some("file"))]);
    }

    /// The overview belongs to its pane. A split off it opens a scratch editor
    /// and rests the overview's chord, and a close of its pane drops it with
    /// the editor it covered.
    #[test]
    fn an_overview_stays_with_its_own_pane() {
        let mut h = two_files();
        let (editor, _) = h.stoat.focused_editor_ids().expect("editor");
        let id = h.cover_with_overview(false);
        h.stoat.set_focused_mode("space".to_string());
        let source = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.unwiden();

        pane::split_pane(&mut h.stoat, Axis::Vertical);
        let (kept, split_view, mode) = {
            let ws = h.stoat.active_workspace();
            (
                ws.panes.pane(source).view.clone(),
                ws.panes.pane(ws.panes.focus()).view.clone(),
                ws.overviews[id].mode.clone(),
            )
        };
        pane::close_pane_by_id(&mut h.stoat, source);
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                kept,
                matches!(split_view, View::Editor(_)),
                mode,
                ws.overviews.len(),
                ws.editors.contains_key(editor),
            ),
            (View::Overview(id), true, "normal".to_string(), 0, false),
        );
    }

    #[test]
    fn closing_an_overview_pane_keeps_a_shell_another_pane_shows() {
        let mut h = two_files();
        dispatch(&mut h.stoat, &stoat_action::Terminal);
        // The split action runs a shell of its own, so the tree split, which
        // copies the view, is what puts a second pane over this one shell.
        let ws = h.stoat.active_workspace_mut();
        ws.panes.split(Axis::Vertical);
        let View::Terminal(shell) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the split shows the shell too");
        };
        h.cover_with_overview(false);

        dispatch(&mut h.stoat, &stoat_action::ClosePane);
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.overviews.len(), ws.terms.contains_key(shell)),
            (0, true)
        );
    }

    /// A shell opened over the overview holds it beneath, and the pane or the
    /// tab that closes takes it along.
    #[test]
    fn an_overview_beneath_a_shell_closes_with_its_pane_or_tab() {
        let actions: [&dyn Action; 2] = [&stoat_action::ClosePane, &stoat_action::CloseTab];
        let counts = actions.map(|action| {
            let (mut h, _) = shell_over_an_overview();
            let open = h.stoat.active_workspace().overviews.len();
            dispatch(&mut h.stoat, action);
            (open, h.stoat.active_workspace().overviews.len())
        });
        assert_eq!(counts, [(1, 0); 2]);
    }

    /// A shell that exits in a parked tab closes its pane, and the overview
    /// beneath the shell goes with it.
    #[test]
    fn an_exited_shell_in_a_parked_tab_drops_the_overview_beneath() {
        let (mut h, shell) = shell_over_an_overview();
        dispatch(&mut h.stoat, &stoat_action::PrevTab);
        h.stoat
            .handle_pty_notification(PtyNotification::TermExited {
                workspace: h.stoat.active_workspace,
                term_id: shell,
            });
        assert_eq!(h.stoat.active_workspace().overviews.len(), 0);
    }

    #[test]
    fn a_shell_over_the_overview_gives_it_back_when_it_exits() {
        let mut h = two_files();
        h.cover_with_overview(false);
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
        assert_eq!(h.stoat.current_view(), Some("overview"));
    }

    /// A step holds the cursor inside the document's rows, and a step before
    /// the first document lands moves nothing.
    #[test]
    fn an_overview_step_stays_inside_the_document() {
        let cursor = |h: &TestHarness| {
            h.stoat
                .active_workspace()
                .focused_overview()
                .map(|state| state.cursor)
        };
        let mut h = two_files();
        h.cover_with_overview(false);
        overview_step(&mut h.stoat, 1);
        let unbuilt = cursor(&h);

        h.settle();
        let rows = h
            .stoat
            .active_workspace()
            .overview_doc
            .as_ref()
            .and_then(|built| built.doc.as_deref())
            .map_or(0, preview_row_count);
        overview_step(&mut h.stoat, -1);
        let above_first = cursor(&h);
        overview_step(&mut h.stoat, 100);
        assert_eq!(
            (unbuilt, above_first, cursor(&h)),
            (Some(0), Some(0), Some(rows - 1)),
        );
    }

    /// Keys go to a focused dock, so the overview in the split pane beneath it
    /// lends the dock no mode.
    #[test]
    fn a_focused_dock_takes_no_mode_from_the_overview() {
        let mut h = two_files();
        h.cover_with_overview(false);
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
    /// overview keeps its state, and a respawn that finds no dead pane builds
    /// nothing.
    #[test]
    fn a_restored_overview_pane_gets_a_fresh_state_and_a_document() {
        let mut h = two_files();
        respawn_overview_panes(&mut h.stoat);
        h.settle();
        let built_for_no_pane = h.stoat.active_workspace().overview_doc.is_some();

        let restored = h.stoat.active_workspace().panes.focus();
        pane::split_pane(&mut h.stoat, Axis::Vertical);
        let live = h.stoat.active_workspace().panes.focus();
        pane::split_pane(&mut h.stoat, Axis::Vertical);
        let ws = h.stoat.active_workspace_mut();
        let live_id = ws.overviews.insert(OverviewState::default());
        ws.panes.pane_mut(live).view = View::Overview(live_id);
        ws.panes.pane_mut(restored).view = View::Overview(OverviewId::default());
        respawn_overview_panes(&mut h.stoat);
        h.settle();
        let ws = h.stoat.active_workspace();
        let kept = (ws.panes.pane(live).view.clone(), ws.overviews.len());
        let state = match ws.panes.pane(restored).view {
            View::Overview(id) => ws
                .overviews
                .get(id)
                .map(|state| (state.cursor, state.covered.clone())),
            _ => None,
        };
        let files: Vec<&str> = ws
            .overview_doc
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
                (View::Overview(live_id), 2),
                Some((0, None)),
                vec!["a.txt", "b.txt"]
            ),
        );
    }

    /// Only an overview on screen follows the tree. After a git event a frame
    /// rebuilds the document under one and leaves it stale under a plain pane.
    #[test]
    fn a_focused_overview_rebuilds_when_the_tree_moves() {
        let current = |h: &TestHarness| {
            let ws = h.stoat.active_workspace();
            ws.overview_doc
                .as_ref()
                .is_some_and(|built| built.stamp == OverviewStamp::read(ws))
        };
        let mut h = two_files();
        ensure_overview_doc(&mut h.stoat);
        h.settle();
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.stoat.drive_background();
        let plain = current(&h);

        h.cover_with_overview(false);
        h.settle();
        h.stoat.active_workspace_mut().invalidate_all_diffs();
        h.stoat.drive_background();
        let focused = current(&h);
        assert_eq!((plain, focused), (false, true));
    }
}
