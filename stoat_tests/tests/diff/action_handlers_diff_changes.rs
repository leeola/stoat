use crossterm::event::{Event, KeyCode, MouseButton, MouseEventKind};
use std::{
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    action_handlers::{
        commits,
        diff_changes::{
            diff_changes_jump, diff_changes_scroll, diff_changes_step, respawn_diff_changes_panes,
            viewport_rows,
        },
        dispatch, focused_editor_mut, jump, pane, view,
    },
    apc_emit,
    app::UpdateEffect,
    buffer::BufferId,
    buffer_lifecycle, diff_changes,
    diff_changes::{DiffChangesId, DiffChangesState, RevealFlash, StackStamp},
    diff_map::{DiffHunk, DiffHunkStatus, DiffMap, TokenDetail},
    display_map::{DisplayPoint, InlayPoint},
    editor_state::EditorId,
    host::NoopLsp,
    pane::{Axis, DockPanel, DockSide, DockVisibility, FocusTarget, View},
    run::pty::PtyNotification,
    term_session::TermId,
    test_fixture::mouse_event,
    test_harness::{editor, keys, TestHarness},
    workspace::Workspace,
};
use stoat_action::Action;
use stoat_config::MinimapMode;
use stoat_text::{cursor_offset, Point};
use tokio::sync::mpsc;

/// Two changed files, with `a.txt` open in the focused pane.
fn two_files() -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
    h.open_file(Path::new("/repo/a.txt"));
    h
}

/// A settled changes view over a repository with no changed file.
fn empty_stack() -> TestHarness {
    empty_stack_covering(false)
}

/// [`empty_stack`] whose changes view opened from the per-file diff when
/// `covered_diff` holds.
fn empty_stack_covering(covered_diff: bool) -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git().add_repo(PathBuf::from("/repo"));
    h.cover_with_diff_changes(covered_diff);
    h.settle();
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

/// The view and the status row after each of `actions`, each one
/// dispatched over a cleared status row.
fn answers(
    h: &mut TestHarness,
    actions: &[&dyn Action],
) -> Vec<(Option<&'static str>, Option<String>)> {
    actions
        .iter()
        .map(|&action| {
            h.stoat.pending_message = None;
            dispatch(&mut h.stoat, action);
            (h.stoat.current_view(), h.stoat.pending_message.clone())
        })
        .collect()
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

/// The focused editor's file and its selections as buffer offsets, read
/// from the focused excerpt in a changes view.
fn focused_selections(h: &mut TestHarness) -> (Option<PathBuf>, Vec<(usize, usize, bool)>) {
    let path = focused_path(h);
    let editor = focused_editor_mut(&mut h.stoat).expect("a focused editor");
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let spans = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| {
            (
                buffer.resolve_anchor(&sel.start),
                buffer.resolve_anchor(&sel.end),
                sel.reversed,
            )
        })
        .collect();
    (path, spans)
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
            2
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

/// A jump back from the second file of the stack lands on the first file's
/// excerpt, and the changes view stays.
#[test]
fn a_jump_back_lands_on_the_excerpt_it_left() {
    let mut h = two_files();
    h.cover_with_diff_changes(false);
    h.settle();
    jump::push_jump(&mut h.stoat);
    h.type_keys("j j");
    let crossed = place(&mut h);

    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(
        (crossed, h.stoat.current_view(), place(&mut h)),
        (at("b.txt", 0), Some("diff_changes"), at("a.txt", 0)),
    );
}

/// A jump back into a file folded to its header opens the file, as every
/// landing on a file does, and the cursor takes the place the jump left.
#[test]
fn a_jump_back_into_a_folded_file_opens_it() {
    let mut h = two_files();
    h.cover_with_diff_changes(false);
    h.settle();
    jump::push_jump(&mut h.stoat);
    h.type_keys("j j");
    let crossed = place(&mut h);
    let first_folded = |h: &mut TestHarness| {
        let ws = h.stoat.active_workspace_mut();
        let id = ws.focused_diff_changes_id().expect("a changes view");
        ws.diff_changes_views[id].excerpts[0].collapsed
    };
    {
        let ws = h.stoat.active_workspace_mut();
        let id = ws.focused_diff_changes_id().expect("a changes view");
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = ws;
        diff_changes::toggle_file(&mut diff_changes_views[id], editors, 0);
    }
    let folded = first_folded(&mut h);

    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(
        (crossed, folded, first_folded(&mut h), place(&mut h)),
        (at("b.txt", 0), true, false, at("a.txt", 0)),
    );
}

/// A jump back to a file that the stack does not show closes the changes
/// view and shows that file.
#[test]
fn a_jump_back_to_a_file_outside_the_stack_closes_the_view() {
    let mut h = TestHarness::with_size(100, 24);
    h.fake_fs().insert_file(Path::new("/notes.txt"), b"notes\n");
    h.open_file(Path::new("/notes.txt"));
    jump::push_jump(&mut h.stoat);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
    h.cover_with_diff_changes(false);
    h.settle();

    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(
        (
            h.stoat.current_view(),
            h.stoat.active_workspace().diff_changes_views.len(),
        ),
        (Some("file"), 0),
    );
}

/// Down at an excerpt's last row and Up at its first row cross into the
/// neighboring file, as `j` and `k` do in normal mode. The entered file
/// takes insert mode, and the file the cursor left returns to normal mode.
#[test]
fn insert_mode_arrows_cross_into_the_neighboring_file() {
    let crossings = ["j j j j i Down", "j j j j j i Up"].map(|keys| {
        let mut h = deep_files();
        h.cover_with_diff_changes(false);
        h.settle();
        h.type_keys(keys);
        (place(&mut h), excerpt_modes(&h))
    });
    assert_eq!(
        crossings,
        [
            (
                at("b.txt", 0),
                ["normal", "insert"].map(String::from).to_vec()
            ),
            (
                at("a.txt", 9),
                ["insert", "normal"].map(String::from).to_vec()
            ),
        ],
    );
}

/// The mode of each excerpt's editor, in stack order.
fn excerpt_modes(h: &TestHarness) -> Vec<String> {
    let ws = h.stoat.active_workspace();
    let state = ws.focused_diff_changes().expect("a changes view");
    state
        .excerpts
        .iter()
        .map(|excerpt| ws.editors[excerpt.editor_id].mode.clone())
        .collect()
}

/// An insert session that crosses into another file leaves each file's
/// typing one undo step of its own. `u` in the entered file takes back all
/// it typed there, and an edit back in the left file undoes apart from the
/// typing before the crossing.
#[test]
fn each_files_typing_undoes_as_its_own_step_after_an_insert_crossing() {
    let mut h = deep_files();
    h.cover_with_diff_changes(false);
    h.settle();

    h.type_keys("j j j j i x y Down z w Escape u k ~ u");
    assert_eq!(
        ["/repo/a.txt", "/repo/b.txt"].map(|path| h.buffer_text_at(Path::new(path))),
        [
            "1\n2\n3\n4\n5\n6\n7\n8\nNINE\nxy",
            "a\nb\nc\nd\ne\nf\ng\nh\nI\n"
        ],
    );
}

/// A jump leaves every excerpt's cursor where the row-by-row walk leaves
/// it, for every distance: past the top fold, over a removed base row
/// either way, across the boundaries, past a file that waits on its diff
/// map, and against either end of the stack with the goal column kept. A
/// page up at the stack's top stays.
#[test]
fn a_jump_lands_where_the_row_walk_lands() {
    let mut h = TestHarness::with_size(100, 24);
    let lines = |prefix: &str| {
        (1..=9)
            .map(|n| format!("{prefix} {n}\n"))
            .collect::<String>()
    };
    let (a, b, c) = (lines("a"), lines("b"), lines("c"));
    h.stage_review_scenario(
        "/repo",
        &[
            ("a.txt", &a, &a.replace("a 2\n", "")),
            ("b.txt", &b, &b.replace("b 5\n", "").replace("b 9", "B")),
            ("c.txt", &c, &c.replace("c 9\n", "C NINE")),
        ],
    );
    h.cover_with_diff_changes(false);
    h.settle();
    let editor_ids = |h: &TestHarness| -> Vec<EditorId> {
        let state = h.stoat.active_workspace().focused_diff_changes();
        let excerpts = &state.expect("a changes view").excerpts;
        excerpts.iter().map(|excerpt| excerpt.editor_id).collect()
    };
    let land = |h: &mut TestHarness, (index, start): (usize, Point), delta: i32, walk: bool| {
        for (at, id) in editor_ids(h).into_iter().enumerate() {
            let point = if at == index { start } else { Point::new(0, 0) };
            let editor = h.stoat.active_workspace_mut().editors.get_mut(id);
            diff_changes::place_cursor(editor.expect("an excerpt editor"), point);
        }
        let ws = h.stoat.active_workspace_mut();
        ws.focused_diff_changes_mut()
            .expect("a changes view")
            .focused = index;
        let effect = match walk {
            true => diff_changes_step(&mut h.stoat, delta),
            false => diff_changes_jump(&mut h.stoat, delta),
        };
        let cursors: Vec<Point> = editor_ids(h)
            .into_iter()
            .map(|id| {
                let editor = h.stoat.active_workspace_mut().editors.get_mut(id);
                diff_changes::cursor_point(editor.expect("an excerpt editor"))
            })
            .collect();
        let focused = h.stoat.active_workspace().focused_diff_changes();
        (effect, focused.expect("a changes view").focused, cursors)
    };

    // From above and below b.txt's removed row, from the stack's top, and
    // from c.txt's last rows, with b.txt folded and then waiting. A walk
    // up into a.txt passes its removed row onto the top row.
    let starts = [
        (1, Point::new(2, 2)),
        (1, Point::new(5, 2)),
        (0, Point::new(0, 0)),
        (2, Point::new(6, 2)),
    ];
    let folded = h
        .stoat
        .active_workspace()
        .focused_diff_changes()
        .and_then(|state| state.excerpts[1].folded_version);
    let (mut walked, mut jumped) = (Vec::new(), Vec::new());
    for b_folded in [folded, None] {
        if let Some(state) = h.stoat.active_workspace_mut().focused_diff_changes_mut() {
            state.excerpts[1].folded_version = b_folded;
        }
        for start in starts {
            for delta in -14..=24 {
                walked.push(land(&mut h, start, delta, true));
                jumped.push(land(&mut h, start, delta, false));
            }
        }
    }
    let (_, focused, cursors) = land(&mut h, (0, Point::new(0, 0)), -4, false);
    assert_eq!((jumped, focused, cursors[0]), (walked, 0, Point::new(0, 0)),);
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

/// Each editing key acts on the changes view's focused excerpt as it acts
/// on a plain editor over the same file, so the file's text and the cursor
/// end the same.
#[test]
fn editing_keys_act_on_the_excerpt_as_in_a_plain_editor() {
    let path = Path::new("/repo/a.txt");
    for (keys, text) in [
        ("i z Escape u", "one two\nthree\n"),
        ("x d", "three\n"),
        ("o z Escape", "one two\nz\nthree\n"),
        ("A z Escape", "one twoz\nthree\n"),
        ("I z Escape", "zone two\nthree\n"),
        ("Alt-j", "one two three\n"),
        ("r z", "zne two\nthree\n"),
        ("c z Escape", "zne two\nthree\n"),
        ("~", "One two\nthree\n"),
        ("m s (", "(o)ne two\nthree\n"),
        ("m i w d", " two\nthree\n"),
        ("/ t h r Enter", "one two\nthree\n"),
    ] {
        let mut plain = one_two_three();
        plain.open_file(path);
        plain.settle();
        plain.type_keys(keys);

        let mut stack = one_two_three();
        stack.cover_with_diff_changes(false);
        stack.settle();
        stack.type_keys(keys);

        let want = (text.to_string(), plain.primary_head_offset());
        assert_eq!(
            [
                (plain.focused_buffer_text(), plain.primary_head_offset()),
                (stack.buffer_text_at(path), excerpt_head_offset(&mut stack)),
            ],
            [want.clone(), want],
            "the text and the cursor after {keys:?}, in a plain editor and in the stack",
        );
    }
}

/// One changed file, whose hunk turns the line `a` into two lines.
fn one_two_three() -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "one two\nthree\n")]);
    h
}

/// The cursor offset of the focused excerpt's newest selection, read as
/// [`TestHarness::primary_head_offset`] reads a plain editor's.
fn excerpt_head_offset(h: &mut TestHarness) -> usize {
    let editor = focused_editor_mut(&mut h.stoat).expect("a focused excerpt");
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let newest = editor.selections.newest_anchor();
    cursor_offset(
        buffer.rope(),
        buffer.resolve_anchor(&newest.tail()),
        buffer.resolve_anchor(&newest.head()),
    )
}

/// A search lands its cursor on a row a fold hides, and the rows around the
/// match open, so the cursor's display row is the match's own row.
#[test]
fn a_search_into_a_fold_shows_the_rows_around_the_match() {
    let mut h = folded_long_files();

    h.type_keys("/ 5 9 Enter");
    assert_eq!(
        (place(&mut h), cursor_shown_row(&mut h)),
        (at("a.txt", 58), Some(58)),
    );
}

/// A motion on a row the cursor opened keeps the row open, so the cursor
/// stays in sight while it moves there.
#[test]
fn a_motion_on_a_revealed_row_keeps_it_shown() {
    let mut h = folded_long_files();

    h.type_keys("/ 5 9 Enter h");
    assert_eq!(
        (place(&mut h), cursor_shown_row(&mut h)),
        (at("a.txt", 58), Some(58)),
    );
}

/// An insert-mode Down from the last row a hunk's context shows lands on
/// the fold row's first position, which opens four rows around it. The
/// next paint holds the cursor's line on the screen row of that fold row.
#[test]
fn an_insert_mode_down_at_the_last_context_row_shows_the_next_row() {
    let mut h = folded_long_files();
    put_cursor(&mut h, 43);
    let rows = focused_excerpt_rows(&mut h);
    h.type_keys("i");
    let fold_row = cursor_screen_row(&mut h).map(|row| row + 1);

    h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
    let held = h
        .stoat
        .active_workspace()
        .focused_diff_changes()
        .and_then(|state| state.held_screen_row);
    assert_eq!(
        (place(&mut h), focused_excerpt_rows(&mut h), held),
        (at("a.txt", 44), rows + 4, fold_row),
    );
}

/// The `j` walk takes a step onto a fold row in normal mode, and that row
/// stays folded.
#[test]
fn a_j_onto_the_fold_row_stays_on_it() {
    let mut h = folded_long_files();
    put_cursor(&mut h, 43);
    let rows = focused_excerpt_rows(&mut h);

    h.type_keys("j");
    assert_eq!(
        (place(&mut h), focused_excerpt_rows(&mut h)),
        (at("a.txt", 44), rows),
    );
}

/// The rows a cursor opened fold again once the cursor leaves them.
#[test]
fn the_revealed_rows_fold_again_once_the_cursor_leaves() {
    let mut h = folded_long_files();
    put_cursor(&mut h, 43);
    let rows = focused_excerpt_rows(&mut h);

    h.type_keys("i Down");
    let revealed = focused_excerpt_rows(&mut h);
    h.type_keys("Escape k k k k");
    assert_eq!([revealed, focused_excerpt_rows(&mut h)], [rows + 4, rows]);
}

/// [`long_files`] in a changes view, settled and painted, so both excerpts
/// are folded and the stack has a viewport.
fn folded_long_files() -> TestHarness {
    let mut h = long_files(24);
    h.cover_with_diff_changes(false);
    h.settle();
    h.render_composited();
    h
}

/// Put the focused excerpt's cursor at the start of buffer row `row`.
fn put_cursor(h: &mut TestHarness, row: u32) {
    let editor = focused_editor_mut(&mut h.stoat).expect("a focused excerpt");
    diff_changes::place_cursor(editor, Point::new(row, 0));
}

/// The display rows the focused excerpt shows.
fn focused_excerpt_rows(h: &mut TestHarness) -> usize {
    let ws = h.stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id().expect("a changes view");
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let excerpt = diff_changes_views[id]
        .focused_excerpt()
        .expect("a focused excerpt");
    diff_changes::excerpt_rows(excerpt, editors)
}

/// The row of the stack's view that the focused excerpt's cursor sits on.
fn cursor_screen_row(h: &mut TestHarness) -> Option<u16> {
    let ws = h.stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id()?;
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let state = diff_changes_views.get_mut(id)?;
    diff_changes::layout_stack(state, editors);
    let row = diff_changes::cursor_stack_row(state, editors)?;
    u16::try_from(row.checked_sub(state.scroll)?).ok()
}

/// The buffer row that the focused editor's cursor display row starts with.
///
/// A fold row starts with its fold's first row, so a cursor that a fold
/// hides answers that row and not its own.
fn cursor_shown_row(h: &mut TestHarness) -> Option<u32> {
    let editor = focused_editor_mut(&mut h.stoat)?;
    let row = view::cursor_display_row(editor);
    editor
        .display_map
        .snapshot()
        .display_to_buffer(DisplayPoint::new(row, 0))
        .map(|point| point.row)
}

/// Two expands up from a hunk's first row open ten lines above it each, and
/// each fold row counts the lines it still hides.
#[test]
fn an_expand_up_from_a_hunk_opens_ten_lines_above_it() {
    let mut h = folded_long_files();
    act(&mut h, &stoat_action::DiffChangesExpandUp);
    act(&mut h, &stoat_action::DiffChangesExpandUp);
    assert_eq!(
        (shown_rows(&mut h), fold_row_texts(&mut h)),
        (
            rows_of(&[0..=0, 17..=43, 44..=44]),
            ["\u{22ef} 17 lines", "\u{22ef} 16 lines"]
                .map(String::from)
                .to_vec(),
        ),
    );
}

/// An expand down opens ten lines below the cursor's hunk and keeps both the
/// last opened line and the cursor on screen. The cursor stays.
#[test]
fn an_expand_down_opens_ten_lines_below_and_scrolls_to_them() {
    let mut h = folded_long_files();
    let before = place(&mut h);
    act(&mut h, &stoat_action::DiffChangesExpandDown);

    let (scroll, cursor, total) = stack_view(&mut h);
    let viewport = viewport_rows(&h.stoat);
    let last_opened = stack_row_of(&mut h, 53);
    assert_eq!(
        (
            shown_rows(&mut h),
            place(&mut h),
            (scroll..scroll + viewport).contains(&last_opened),
            (scroll..scroll + viewport).contains(&cursor),
        ),
        (rows_of(&[0..=0, 37..=53, 54..=54]), before, true, true),
        "{scroll} of {total} rows",
    );
}

/// An expand on the fold row above a file's first hunk grows the hunk up,
/// whichever way it asks, since no hunk lies above that fold.
#[test]
fn an_expand_on_the_top_fold_row_grows_it_up_only() {
    let mut h = folded_long_files();
    put_cursor(&mut h, 0);
    act(&mut h, &stoat_action::DiffChangesExpandDown);
    assert_eq!(shown_rows(&mut h), rows_of(&[0..=0, 27..=43, 44..=44]));
}

/// An expand on the fold row between two hunks opens the edge it asks for,
/// and the short fold it leaves opens whole.
#[test]
fn an_expand_on_a_middle_fold_row_opens_the_asked_edge() {
    let mut h = two_hunk_file();
    put_cursor(&mut h, 24);
    act(&mut h, &stoat_action::DiffChangesExpandDown);
    let down = shown_rows(&mut h);

    put_cursor(&mut h, 34);
    act(&mut h, &stoat_action::DiffChangesExpandUp);
    assert_eq!(
        (down, shown_rows(&mut h)),
        (
            rows_of(&[0..=0, 17..=33, 34..=34, 37..=43, 44..=44]),
            rows_of(&[0..=0, 17..=43, 44..=44]),
        ),
    );
}

/// Expand All on the fold row of a fold of ten lines or fewer opens it
/// whole.
#[test]
fn an_expand_all_on_a_short_fold_opens_it_whole() {
    let mut h = folded_long_files();
    put_cursor(&mut h, 40);
    act(&mut h, &stoat_action::DiffChangesExpandDown);
    put_cursor(&mut h, 54);
    act(&mut h, &stoat_action::DiffChangesExpandAll);
    assert_eq!(shown_rows(&mut h), rows_of(&[0..=0, 37..=60]));
}

/// Collapse folds the opened rows around the cursor again, and the cursor
/// that the fold hides moves to the fold row.
#[test]
fn collapse_refolds_the_revealed_rows_and_lands_on_the_fold_row() {
    let mut h = folded_long_files();
    act(&mut h, &stoat_action::DiffChangesExpandUp);
    put_cursor(&mut h, 30);
    act(&mut h, &stoat_action::DiffChangesCollapse);
    assert_eq!(
        (shown_rows(&mut h), place(&mut h)),
        (rows_of(&[0..=0, 37..=43, 44..=44]), at("a.txt", 0)),
    );
}

/// An expand with no fold left that way says so in the status row and opens
/// nothing.
#[test]
fn an_expand_with_no_fold_that_way_says_so() {
    let mut h = folded_long_files();
    act(&mut h, &stoat_action::DiffChangesExpandDown);
    act(&mut h, &stoat_action::DiffChangesExpandDown);
    let opened = shown_rows(&mut h);

    dispatch(&mut h.stoat, &stoat_action::DiffChangesExpandDown);
    assert_eq!(
        (shown_rows(&mut h), h.stoat.pending_message.as_deref()),
        (opened, Some("no fold below")),
    );
}

/// `z k` opens ten lines above the cursor's hunk and leaves the view menu.
#[test]
fn z_k_opens_ten_lines_above_the_hunk() {
    let mut h = folded_long_files();
    h.type_keys("z k");
    assert_eq!(
        (shown_rows(&mut h), h.stoat.focused_mode()),
        (rows_of(&[0..=0, 27..=43, 44..=44]), "normal"),
    );
}

/// An expand marks the rows it opened on the excerpt's editor, so they
/// stand out for a moment.
#[test]
fn an_expand_marks_the_opened_rows_for_a_moment() {
    let mut h = folded_long_files();
    act(&mut h, &stoat_action::DiffChangesExpandUp);
    let editor = focused_editor_mut(&mut h.stoat).expect("a focused excerpt");
    assert_eq!(
        editor.reveal_flashes,
        vec![RevealFlash {
            rows: 27..37,
            age: 0.0
        }],
    );
}

/// A press on the top fold row's button opens the ten lines above the hunk
/// and leaves the cursor and the focus where they were.
#[test]
fn a_press_on_the_top_fold_button_opens_ten_lines() {
    let mut h = folded_long_files();
    let before = place(&mut h);
    press_stack(&mut h, MouseButton::Left, 1, 1);
    assert_eq!(
        (shown_rows(&mut h), place(&mut h)),
        (rows_of(&[0..=0, 27..=43, 44..=44]), before),
    );
}

/// A press on a fold row past its buttons puts the cursor on the row and
/// opens nothing.
#[test]
fn a_press_on_a_fold_row_past_its_buttons_places_the_cursor() {
    let mut h = folded_long_files();
    let rows = shown_rows(&mut h);
    press_stack(&mut h, MouseButton::Left, 1, 8);
    assert_eq!((shown_rows(&mut h), place(&mut h)), (rows, at("a.txt", 0)));
}

/// A press on the second button of a fold between two hunks grows the hunk
/// below up by ten lines.
#[test]
fn a_press_on_the_second_button_grows_the_hunk_below_up() {
    let mut h = two_hunk_file();
    let before = place(&mut h);
    let row = stack_row_of(&mut h, 24);
    press_stack(&mut h, MouseButton::Left, row, 3);
    assert_eq!(
        (shown_rows(&mut h), place(&mut h)),
        (
            rows_of(&[0..=0, 17..=23, 24..=24, 27..=43, 44..=44]),
            before
        ),
    );
}

/// A press on a Fold Button opens the fold and leaves the stack's scroll
/// where a wheel left it, with the cursor above the view. The last fold,
/// which a wheel scroll to the stack's last page shows, opens ten rows from
/// its top. The second button of the fold between the hunks grows the hunk
/// below up by ten rows.
#[test]
fn a_press_on_a_fold_button_keeps_the_scroll_off_the_cursor() {
    let press_after_scroll = |fold_row: u32, column: u16, delta: i32| {
        let mut h = two_hunk_file_rows(10);
        diff_changes_scroll(&mut h.stoat, delta);
        h.render_composited();
        let (scroll, cursor, _) = stack_view(&mut h);
        let row = stack_row_of(&mut h, fold_row);
        let viewport = viewport_rows(&h.stoat);
        assert!(
            cursor < scroll && (scroll + 2..scroll + viewport).contains(&row),
            "cursor row {cursor} above the view of {viewport} rows from {scroll}, \
             fold row {row} in it under the pinned rows",
        );

        press_stack(&mut h, MouseButton::Left, row, column);
        h.render_composited();
        ((shown_rows(&mut h), stack_view(&mut h).0), scroll)
    };
    let (last, last_scroll) = press_after_scroll(44, 1, i32::MAX);
    let (middle, middle_scroll) = press_after_scroll(24, 3, 6);

    assert_eq!(
        [last, middle],
        [
            (rows_of(&[0..=0, 17..=23, 24..=24, 37..=54]), last_scroll),
            (
                rows_of(&[0..=0, 17..=23, 24..=24, 27..=43, 44..=44]),
                middle_scroll
            ),
        ],
    );
}

/// A middle press on a Fold Button opens the fold and asks the server
/// nothing, since the press places no cursor to ask from.
#[test]
fn a_middle_press_on_a_fold_button_opens_the_fold_and_asks_no_server() {
    let mut h = folded_long_files();
    h.allow_host_swap();
    h.stoat.set_lsp_host(Arc::new(NoopLsp));
    h.stoat.pending_message = None;
    press_stack(&mut h, MouseButton::Middle, 1, 1);
    assert_eq!(
        (shown_rows(&mut h), h.stoat.pending_message.take()),
        (rows_of(&[0..=0, 27..=43, 44..=44]), None),
    );
}

/// Press `button` on stack row `row` of the focused changes view, `column`
/// cells past the stack's left edge as of the last paint.
fn press_stack(h: &mut TestHarness, button: MouseButton, row: usize, column: u16) {
    let (content, scroll) = {
        let state = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .expect("a changes view");
        (state.content_rect.expect("a painted stack"), state.scroll)
    };
    h.stoat.update(mouse_event(
        MouseEventKind::Down(button),
        content.x + column,
        content.y + (row - scroll) as u16,
    ));
}

/// One changed file of sixty lines with hunks on rows 20 and 40, in a
/// settled and painted changes view of a window `height` rows tall, so its
/// excerpt folds rows 0 to 16, 24 to 36, and 44 on.
fn two_hunk_file_rows(height: u16) -> TestHarness {
    let mut h = TestHarness::with_size(100, height);
    let base: String = (1..=60).map(|n| format!("{n}\n")).collect();
    let working = base
        .replace("\n21\n", "\nTWENTY-ONE\n")
        .replace("\n41\n", "\nFORTY-ONE\n");
    h.stage_review_scenario("/repo", &[("a.txt", &base, &working)]);
    h.cover_with_diff_changes(false);
    h.settle();
    h.render_composited();
    h
}

/// The two-hunk file of [`two_hunk_file_rows`] in a window 24 rows tall.
fn two_hunk_file() -> TestHarness {
    two_hunk_file_rows(24)
}

/// Dispatch `action`, then press Escape, so the key epilogue runs after the
/// action as it runs after the Enter of a palette run.
fn act(h: &mut TestHarness, action: &dyn Action) {
    dispatch(&mut h.stoat, action);
    h.type_keys("Escape");
}

/// The buffer row each display row of the focused excerpt starts with, so a
/// fold row reads as the first row it hides.
fn shown_rows(h: &mut TestHarness) -> Vec<u32> {
    let editor = focused_editor_mut(&mut h.stoat).expect("a focused excerpt");
    let snapshot = editor.display_map.snapshot();
    (0..snapshot.line_count())
        .filter_map(|row| snapshot.display_to_buffer(DisplayPoint::new(row, 0)))
        .map(|point| point.row)
        .collect()
}

/// The text of each fold row of the focused excerpt, in buffer order.
fn fold_row_texts(h: &mut TestHarness) -> Vec<String> {
    let editor = focused_editor_mut(&mut h.stoat).expect("a focused excerpt");
    let snapshot = editor.display_map.snapshot();
    let folds = snapshot.fold_snapshot();
    folds
        .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
        .into_iter()
        .filter(|fold| fold.placeholder.is_fold_row())
        .map(|fold| fold.placeholder.text.to_string())
        .collect()
}

/// The stack row of the focused excerpt's buffer row `row`.
fn stack_row_of(h: &mut TestHarness, row: u32) -> usize {
    let ws = h.stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id().expect("a changes view");
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let state = &mut diff_changes_views[id];
    diff_changes::layout_stack(state, editors);
    let excerpt = state.focused_excerpt().expect("a focused excerpt");
    let editor = editors.get_mut(excerpt.editor_id).expect("an editor");
    let display = editor
        .display_map
        .snapshot()
        .buffer_to_display(Point::new(row, 0));
    excerpt.top + diff_changes::header_rows(excerpt) + display.row as usize
}

/// The rows of `runs`, one after another.
fn rows_of(runs: &[RangeInclusive<u32>]) -> Vec<u32> {
    runs.iter().flat_map(|run| run.clone()).collect()
}

/// A changes view with no excerpt has no focused editor, so the editor
/// keys find nothing to act on.
#[test]
fn an_empty_stack_has_no_focused_editor() {
    let h = empty_stack();
    assert_eq!(
        (h.stoat.current_view(), h.stoat.focused_editor_ids()),
        (Some("diff_changes"), None),
    );
}

/// `space d c` closes a changes view with no change left in it, and puts
/// back the view it covered.
#[test]
fn diff_changes_on_an_empty_changes_view_closes_it() {
    let mut h = empty_stack();
    h.type_keys("space d c");
    assert_eq!(
        (
            h.stoat.current_view(),
            h.stoat
                .active_workspace()
                .focused_diff_changes_id()
                .is_none()
        ),
        (Some("file"), true),
    );
}

#[test]
fn diff_changes_on_a_stacked_changes_view_closes_it() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();

    dispatch(&mut h.stoat, &stoat_action::DiffChanges);

    assert_eq!(
        (h.stoat.current_view(), focused_path(&h)),
        (Some("file"), Some(PathBuf::from("/repo/a.txt"))),
    );
}

#[test]
fn an_empty_stack_says_no_changes_to_a_hop_and_an_open() {
    let mut h = empty_stack();
    assert_eq!(
        answers(
            &mut h,
            &[
                &stoat_action::GotoNextChange,
                &stoat_action::GotoPrevChange,
                &stoat_action::DiffChangesOpen,
            ],
        ),
        vec![(Some("diff_changes"), Some("no changes".to_owned())); 3],
        "each key answers the empty stack and keeps the changes view"
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

/// An open re-reads the git state, so a commit that no watcher event
/// reported leaves no file of the tree before it in the list.
#[test]
fn an_open_after_an_unseen_commit_lists_the_tree_as_it_stands() {
    let mut h = deep_files();
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();
    h.fake_git()
        .add_repo("/repo")
        .clear_changes()
        .head_file("a.txt", "1\n2\n3\n4\n5\n6\n7\n8\nNINE\n")
        .head_file("b.txt", "a\nb\nc\nd\ne\nf\ng\nh\nI\n");

    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle_diff_jobs();
    assert_eq!(
        (h.stoat.current_view(), listed_paths(&h)),
        (Some("diff_changes"), Vec::<String>::new()),
    );
}

/// An open re-reads the git state, so a file changed after the last tally
/// with no watcher event joins the list.
///
/// The stack parks across a flip to the diff, and its excerpts keep their
/// current maps, so the open that brings it back wants no map. A want is
/// what owes a tally otherwise.
#[test]
fn an_open_lists_a_file_changed_since_the_last_tally() {
    let mut h = deep_files();
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle_diff_jobs();
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle_diff_jobs();
    h.fake_fs().insert_file(Path::new("/repo/c.txt"), b"C\n");
    h.fake_git()
        .add_repo("/repo")
        .modified("c.txt", "c\n", "C\n");

    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle_diff_jobs();
    assert_eq!(listed_paths(&h), ["a.txt", "b.txt", "c.txt"]);
}

/// An open re-arms the background warm, whose cache holds diffs against the
/// git state that the open re-reads.
#[test]
fn an_open_rearms_the_diff_warm() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    h.stoat.active_workspace_mut().diff_warmed = true;

    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    assert!(
        !h.stoat.active_workspace().diff_warmed,
        "the next background pump warms the cache again",
    );
}

/// The files of the focused changes view's excerpts, in stack order.
fn listed_paths(h: &TestHarness) -> Vec<String> {
    let ws = h.stoat.active_workspace();
    ws.focused_diff_changes_id()
        .and_then(|id| ws.diff_changes_views.get(id))
        .map(|state| {
            state
                .excerpts
                .iter()
                .map(|excerpt| excerpt.rel_path.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// A file finder pick of a file the stack shows keeps the changes view and
/// lands on the file's first hunk, below the context rows a fold leaves.
#[test]
fn a_finder_pick_lands_on_the_files_first_hunk() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();

    dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
    h.type_keys("b enter");
    h.settle();
    assert_eq!(
        (h.stoat.current_view(), place(&mut h)),
        (Some("diff_changes"), at("b.txt", 8))
    );
}

#[test]
fn a_buffer_pick_lands_on_the_files_first_hunk() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/b.txt"));
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();

    dispatch(&mut h.stoat, &stoat_action::OpenBufferPicker);
    let titled = h.snapshot().content.contains("Buffers \u{00B7} diff");
    h.type_keys("b enter");
    h.settle();
    assert_eq!(
        (titled, h.stoat.current_view(), place(&mut h)),
        (true, Some("diff_changes"), at("b.txt", 8))
    );
}

#[test]
fn a_pick_with_no_excerpt_opens_the_file_over_the_covered_view() {
    let mut h = two_files();
    h.fake_git().add_repo("/repo").head_file("c.txt", "c\n");
    h.fake_fs().insert_file("/repo/c.txt", "c\n");
    h.open_file(Path::new("/repo/c.txt"));
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();

    dispatch(&mut h.stoat, &stoat_action::OpenBufferPicker);
    h.type_keys("c enter");
    h.settle();
    assert_eq!(
        (h.stoat.current_view(), focused_path(&h)),
        (Some("file"), Some(PathBuf::from("/repo/c.txt")))
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

/// `q` lands on the focused excerpt's file with the excerpt's selections,
/// so the close leaves the cursor where it was in the stack.
#[test]
fn q_lands_on_the_focused_excerpts_file_with_its_selections() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    h.type_keys("j j j j j v l l v");
    let in_stack = focused_selections(&mut h);
    h.type_keys("q");
    assert_eq!(
        (
            in_stack.0.clone(),
            h.stoat.current_view(),
            focused_selections(&mut h)
        ),
        (Some(PathBuf::from("/repo/b.txt")), Some("file"), in_stack),
    );
}

/// A changes view opened from the per-file diff returns to the diff, on
/// the file the cursor ended in.
#[test]
fn q_from_a_covered_diff_lands_in_the_diff_on_the_excerpt_file() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    h.type_keys("j j j j j");
    h.type_keys("q");
    assert_eq!(
        (h.stoat.current_view(), focused_path(&h)),
        (Some("diff"), Some(PathBuf::from("/repo/b.txt"))),
    );
}

/// `DiffChanges` on a changes view opened from the per-file diff lands in
/// the plain file the cursor ended in, and does not return to the diff as
/// `q` does.
#[test]
fn diff_changes_on_a_changes_view_from_the_diff_lands_in_the_plain_file() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    h.type_keys("j j j j j");
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    assert_eq!(
        (h.stoat.current_view(), focused_path(&h)),
        (Some("file"), Some(PathBuf::from("/repo/b.txt"))),
    );
}

/// `DiffChanges` on an empty changes view opened from the per-file diff
/// puts back the covered editor as a plain file.
#[test]
fn diff_changes_on_an_empty_changes_view_from_the_diff_lands_in_the_plain_file() {
    let mut h = empty_stack_covering(true);
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    assert_eq!(
        (
            h.stoat.current_view(),
            h.stoat
                .active_workspace()
                .focused_diff_changes_id()
                .is_none()
        ),
        (Some("file"), true),
    );
}

/// With the cursor in the file the changes view covered, `q` keeps the
/// pane's editor and moves its selections to the excerpt's.
#[test]
fn q_with_the_cursor_in_the_covered_file_keeps_the_pane_editor() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    let (editor, _) = h.stoat.focused_editor_ids().expect("editor");
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    let landed = place(&mut h);
    let in_stack = focused_selections(&mut h);
    h.type_keys("q");
    assert_eq!(
        (
            landed,
            h.stoat.focused_editor_ids().map(|(id, _)| id),
            focused_selections(&mut h)
        ),
        (at("a.txt", 5), Some(editor), in_stack),
    );
}

/// The close that lands on another file records the covered one, so `g a`
/// returns to it.
#[test]
fn q_records_the_covered_buffer_for_goto_last_accessed() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    h.type_keys("j j j j j");
    h.type_keys("q");
    let landed = focused_path(&h);
    dispatch(&mut h.stoat, &stoat_action::GotoLastAccessed);
    assert_eq!(
        (landed, focused_path(&h)),
        (
            Some(PathBuf::from("/repo/b.txt")),
            Some(PathBuf::from("/repo/a.txt"))
        ),
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
    h.type_keys("] g ] g");
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

#[test]
fn a_flip_to_the_diff_keeps_the_cursor_on_screen_before_and_after_its_hunks_land() {
    assert_flip_shows_the_cursor(&mut long_files(24));
}

#[test]
fn a_stoatty_flip_to_the_diff_keeps_the_cursor_on_screen_before_and_after_its_hunks_land() {
    let mut h = long_files(24);
    let (tx, _rx) = mpsc::unbounded_channel::<Vec<u8>>();
    h.stoat.set_apc_tx(tx);
    h.resize(160, 48);
    assert_flip_shows_the_cursor(&mut h);
}

#[test]
fn a_flip_into_a_file_that_wraps_on_the_pool_keeps_the_cursor_on_screen() {
    assert_flip_shows_the_cursor(&mut wrapped_files());
}

#[test]
fn a_flip_whose_hunks_land_late_holds_the_stack_row_once_they_land() {
    let mut h = long_files(24);
    stack_on_b(&mut h);
    let stacked = screen_row(&mut h);
    move_b_base(&mut h);

    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    let before = frame_view(&mut h);
    h.settle();
    assert_eq!(
        (stacked.is_some(), on_screen(before), screen_row(&mut h)),
        (true, true, stacked),
        "the view before the hunks land, {before:?}, shows the cursor, and the landing puts it on its stack row",
    );
}

#[test]
fn a_flip_back_before_the_hunks_land_leaves_the_file_unheld() {
    let mut h = long_files(24);
    stack_on_b(&mut h);
    move_b_base(&mut h);
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.stoat.render();
    let (editor, _) = h.stoat.focused_editor_ids().expect("the diff's editor");

    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    let held = h
        .stoat
        .active_workspace()
        .editors
        .get(editor)
        .map(|state| state.held_screen_row);
    assert_eq!(held, Some(None));
}

/// A cursor the stack scrolled off holds no stack row, so the flip lands it
/// as a jump does, centered against the rows the diff paints.
#[test]
fn a_flip_from_a_cursor_off_the_stack_lands_it_centered_on_the_painted_rows() {
    let mut h = long_files(24);
    stack_on_b(&mut h);
    h.stoat
        .active_workspace_mut()
        .focused_diff_changes_mut()
        .expect("a changes view")
        .scroll = 1000;

    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    let (scroll_row, cursor_row, viewport, _) = frame_view(&mut h);
    assert_eq!(scroll_row, cursor_row - (viewport - 1) / 2);
}

#[test]
fn a_diff_left_past_its_last_row_paints_its_last_rows() {
    let mut h = long_files(24);
    h.open_file(Path::new("/repo/b.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();
    let max_scroll = frame_view(&mut h).3;

    let editor = focused_editor_mut(&mut h.stoat).expect("a focused editor");
    editor.scroll_row = max_scroll + 30;
    editor.scroll_offset = (max_scroll + 30) as f32;
    frame_view(&mut h);
    let editor = focused_editor_mut(&mut h.stoat).expect("a focused editor");
    assert_eq!(
        (editor.scroll_row, editor.scroll_offset),
        (max_scroll + 3, (max_scroll + 3) as f32),
        "the pull keeps the scrolloff padding of blank rows under the last line",
    );
}

/// A jump that lands at a diff's end floats the last line by the padding,
/// and the paint after it keeps the float.
#[test]
fn a_landing_at_a_diffs_end_keeps_its_padding_over_two_paints() {
    let mut h = long_files(24);
    h.open_file(Path::new("/repo/b.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();
    let max_scroll = frame_view(&mut h).3;

    let state = focused_editor_mut(&mut h.stoat).expect("a focused editor");
    editor::place_cursor(state, 60, 0);
    state.pending_center = Some(3);
    let landed = frame_view(&mut h).0;
    let kept = frame_view(&mut h).0;
    assert_eq!((landed, kept), (max_scroll + 3, max_scroll + 3));
}

/// Two changed files of 150 lines too wide for a diff column, each changed
/// on its 121st line.
///
/// A file this long wraps a new width on the pool, so a fresh editor over
/// either paints a frame of unwrapped rows before its rows wrap.
fn wrapped_files() -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    let words = "word ".repeat(30);
    let base: String = (1..=150).map(|n| format!("{n} {words}\n")).collect();
    let working = base.replace(&format!("\n121 {words}\n"), &format!("\nCHANGED {words}\n"));
    h.stage_review_scenario(
        "/repo",
        &[("a.txt", &base, &working), ("b.txt", &base, &working)],
    );
    h
}

/// Open the changes view over `a.txt` and walk its cursor onto `b.txt`'s
/// change.
fn stack_on_b(h: &mut TestHarness) {
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    h.type_keys("] g ] g");
}

/// Give `long_files`' `b.txt` a HEAD with thirty more lines past its tenth,
/// and stale its diff map, so the next hunks splice thirty deleted rows
/// above its change.
fn move_b_base(h: &mut TestHarness) {
    let base: String = (1..=60).map(|n| format!("{n}\n")).collect();
    let moved = base.replace("\n10\n", &format!("\n10\n{}", "gone\n".repeat(30)));
    h.fake_git().add_repo("/repo").head_file("b.txt", &moved);
    let b = h
        .stoat
        .active_workspace()
        .buffers
        .id_for_path(Path::new("/repo/b.txt"))
        .expect("the stack opened b.txt");
    h.stoat
        .active_workspace_mut()
        .invalidate_diff(b, Path::new("/repo/b.txt"));
}

/// Flip from the changes view to `b.txt`'s diff, and assert the view shows
/// the cursor on the frame before the hunks land and on the frame after,
/// with the hold gone once the rows are final.
fn assert_flip_shows_the_cursor(h: &mut TestHarness) {
    stack_on_b(h);
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    let before = frame_view(h);
    h.settle();
    let after = frame_view(h);
    let held = focused_editor_mut(&mut h.stoat).map(|editor| editor.held_screen_row);
    assert_eq!(
        (on_screen(before), on_screen(after), held),
        (true, true, Some(None)),
        "views as (scroll_row, cursor_row, viewport, max_scroll): {before:?} then {after:?}",
    );
}

/// Paint a frame as the loop does, with the pools a stoatty terminal reads
/// emitted after the paint, and read the focused editor's view from it.
fn frame_view(h: &mut TestHarness) -> (u32, u32, u32, u32) {
    h.stoat.render();
    apc_emit::emit_smooth_scroll(&mut h.stoat);
    view_bounds(h)
}

/// The focused editor's view as its top row, its cursor's display row, its
/// height, and the top row that puts the last display row at the bottom.
fn view_bounds(h: &mut TestHarness) -> (u32, u32, u32, u32) {
    let editor = focused_editor_mut(&mut h.stoat).expect("a focused editor");
    let viewport = editor.viewport_rows.expect("a painted view");
    let max_scroll = view::max_scroll_offset(editor) as u32;
    (
        editor.scroll_row,
        view::cursor_display_row(editor),
        viewport,
        max_scroll,
    )
}

/// Whether a view from [`view_bounds`] shows its cursor and starts no lower
/// than the top row that puts the last display row at the bottom.
fn on_screen((scroll_row, cursor_row, viewport, max_scroll): (u32, u32, u32, u32)) -> bool {
    scroll_row <= cursor_row && cursor_row < scroll_row + viewport && scroll_row <= max_scroll
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
///
/// The flip back opens the changes view, which re-reads the git state in
/// jobs on the pool. The pool holds those jobs while the flips run, so the
/// count holds the flips' own work alone.
#[test]
fn a_flip_and_back_open_no_repository() {
    let mut h = long_files(24);
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    let opened = h.fake_git().discover_calls();

    h.hold_blocking();
    h.type_keys("Ctrl-t");
    h.settle();
    h.type_keys("Ctrl-t");
    h.settle();
    assert_eq!(
        (
            h.stoat.current_view(),
            h.fake_git().discover_calls() - opened
        ),
        (Some("diff_changes"), 0),
    );
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
    h.type_keys("q");
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
    h.type_keys("] g ] g");

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

/// DiffFiles on an empty changes view closes it and turns the diff on for
/// the view it covered, and a second DiffFiles turns the diff off.
#[test]
fn diff_files_on_an_empty_changes_view_opens_the_covered_files_diff() {
    let mut h = empty_stack();
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();
    let opened = (
        h.stoat.current_view(),
        h.stoat
            .active_workspace()
            .focused_diff_changes_id()
            .is_none(),
    );

    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    assert_eq!(
        (opened, h.stoat.current_view()),
        ((Some("diff"), true), Some("file")),
    );
}

/// DiffFiles on an empty changes view that opened from the per-file diff
/// turns the diff on for the view it covered.
#[test]
fn diff_files_on_an_empty_changes_view_from_the_diff_opens_the_diff() {
    let mut h = empty_stack_covering(true);
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();
    assert_eq!(
        (
            h.stoat.current_view(),
            h.stoat
                .active_workspace()
                .focused_diff_changes_id()
                .is_none()
        ),
        (Some("diff"), true),
    );
}

/// DiffFiles before the first excerpt folds closes the changes view and
/// opens the diff of the file it covered.
#[test]
fn diff_files_before_the_first_excerpt_opens_the_covered_files_diff() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();
    assert_eq!(
        (h.stoat.current_view(), focused_path(&h)),
        (Some("diff"), Some(PathBuf::from("/repo/a.txt"))),
    );
}

/// `] g` and `[ g` run the editor's change walk, which goes on from an
/// excerpt's last hunk into the next excerpt, and a step past either end of
/// the stack stays and says so.
#[test]
fn the_bracket_walk_crosses_the_excerpts_and_stops_at_either_end() {
    let mut h = deep_files();
    h.cover_with_diff_changes(false);
    h.settle();
    let walk: Vec<(String, u32)> = ["] g", "] g", "] g", "[ g", "[ g"]
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

/// A walk back into the excerpt above lands on the last stop of its unit.
/// That excerpt's one hunk holds runs on rows 1 and 3, so the structured
/// walk lands on row 3 and the git walk on the top of the hunk.
#[test]
fn a_walk_back_into_an_excerpt_lands_on_the_last_stop_of_its_unit() {
    let walk = |back: &dyn Action| {
        let mut h = TestHarness::with_size(100, 24);
        h.stage_review_scenario(
            "/repo",
            &[
                ("a.txt", "a\nb\nc\nd\ne\n", "a\nB\nC\nD\ne\n"),
                ("b.txt", "b\n", "B\n"),
            ],
        );
        h.cover_with_diff_changes(false);
        h.settle();

        let ws = h.stoat.active_workspace_mut();
        let buffer_id = ws
            .buffers
            .id_for_path(Path::new("/repo/a.txt"))
            .expect("a.txt");
        let split = DiffHunk {
            status: DiffHunkStatus::Modified,
            buffer_start_line: 1,
            buffer_line_range: 1..4,
            base_byte_range: 2..8,
            anchor_range: None,
            token_detail: Some(Arc::new(TokenDetail {
                buffer_spans: Vec::new(),
                base_spans: Vec::new(),
                tree: true,
            })),
            unstaged_lines: std::iter::once(1..4).collect(),
            marked_rows: vec![1..2, 3..4],
            relocated_rows: Vec::new(),
        };
        ws.install_test_diff_map(buffer_id, DiffMap::from_hunks([split], None));

        let presses: [&dyn Action; 3] = [
            &stoat_action::GotoNextGitChange,
            &stoat_action::GotoNextGitChange,
            back,
        ];
        for action in presses {
            dispatch(&mut h.stoat, action);
        }
        place(&mut h)
    };

    assert_eq!(
        [
            walk(&stoat_action::GotoPrevChange),
            walk(&stoat_action::GotoPrevGitChange),
        ],
        [at("a.txt", 3), at("a.txt", 1)],
    );
}

#[test]
fn an_open_before_the_first_excerpt_keeps_the_changes_view() {
    let mut h = deep_files();
    h.cover_with_diff_changes(false);
    dispatch(&mut h.stoat, &stoat_action::DiffChangesOpen);
    assert_eq!(
        (h.stoat.current_view(), h.stoat.pending_message.as_deref()),
        (Some("diff_changes"), Some("changes still loading")),
    );
}

/// The pane reads "building changes..." until the tally lands, so a hop
/// before then finds the changes loading, not absent.
#[test]
fn a_hop_before_the_first_excerpt_says_changes_still_loading() {
    let mut h = deep_files();
    h.cover_with_diff_changes(false);

    let loading = (
        Some("diff_changes"),
        Some("changes still loading".to_owned()),
    );
    assert_eq!(
        answers(
            &mut h,
            &[&stoat_action::GotoNextChange, &stoat_action::GotoPrevChange],
        ),
        vec![loading; 2],
    );
}

#[test]
fn an_open_on_an_unfolded_excerpt_says_changes_still_loading() {
    let mut h = deep_files();
    h.cover_with_diff_changes(false);
    h.settle();
    let state = h
        .stoat
        .active_workspace_mut()
        .focused_diff_changes_mut()
        .expect("a changes view");
    state.excerpts[state.focused].folded_version = None;

    let loading = (
        Some("diff_changes"),
        Some("changes still loading".to_owned()),
    );
    assert_eq!(
        answers(&mut h, &[&stoat_action::DiffChangesOpen]),
        [loading]
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

/// `z t`, `z b`, and `z z` scroll the stack so the cursor's row sits at the
/// top, the bottom, and the middle of the view, and the cursor stays. The
/// cursor's file starts above the view, so its header pins on the first row
/// and the top is the row under it. The select-mode menu aligns the same
/// way.
#[test]
fn the_view_keys_align_the_stack_on_the_cursor() {
    let mut h = stack_on_the_fifth_change();
    let (_, row, _) = stack_view(&mut h);
    let last = viewport_rows(&h.stoat) - 1;

    let aligned: Vec<(usize, usize)> = ["z t", "z b", "z z", "v z t"]
        .into_iter()
        .map(|keys| {
            h.type_keys(keys);
            let (scroll, row, _) = stack_view(&mut h);
            (scroll, row)
        })
        .collect();
    assert_eq!(
        aligned,
        [
            (row - 1, row),
            (row - last, row),
            (row - last / 2, row),
            (row - 1, row)
        ],
    );
}

/// A changes view over a file of 200 numbered lines with a change on every
/// twentieth, painted once with the cursor on the fifth change.
///
/// The ten changes fill several pages of the stack, so the cursor's row
/// sits far from the first and the last page, and no alignment of it
/// clamps.
fn stack_on_the_fifth_change() -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    let base: String = (1..=200).map(|n| format!("{n}\n")).collect();
    let working: String = (1..=200)
        .map(|n| match n % 20 {
            0 => format!("CHANGED {n}\n"),
            _ => format!("{n}\n"),
        })
        .collect();
    h.stage_review_scenario("/repo", &[("a.txt", &base, &working)]);
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    h.settle();
    h.type_keys("] g ] g ] g ] g ] g");
    h.render_composited();
    h
}

/// A change walk centers the stack on each landing, in the file it stands
/// in and across an excerpt hop either way, so the key epilogue's follow
/// leaves the stack where the walk put it.
#[test]
fn a_change_walk_centers_the_stack_on_its_landing() {
    let landings = walk_the_stack(None);
    let centered: Vec<(usize, bool)> = landings
        .iter()
        .map(|&(_, row, total, _)| (row.saturating_sub(2).min(total - 4), false))
        .collect();
    let got: Vec<(usize, bool)> = landings
        .iter()
        .map(|&(scroll, _, _, followed)| (scroll, followed))
        .collect();
    assert_eq!(got, centered);
}

/// A jump bias keeps a row more of where the walk came from on screen, so a
/// landing walked down sits a row below the center, and one walked up a row
/// above it.
#[test]
fn a_biased_change_walk_lands_a_row_off_the_center() {
    let offsets: Vec<usize> = walk_the_stack(Some(1))
        .iter()
        .map(|&(scroll, row, ..)| row - scroll)
        .collect();
    assert_eq!(offsets, [3, 3, 1]);
}

/// The stack of `long_files` in a four-row view, walked to the next change,
/// on into `b.txt`, then back, as `(scroll, cursor stack row, stack rows,
/// followed)` at each landing. `followed` says whether the key epilogue's
/// follow moved the stack again.
fn walk_the_stack(jump_scrolloff: Option<u32>) -> Vec<(usize, usize, usize, bool)> {
    let mut h = long_files(24);
    h.stoat.settings.jump_scrolloff = jump_scrolloff;
    h.open_file(Path::new("/repo/a.txt"));
    four_row_stack(&mut h);

    let walk: [&dyn Action; 3] = [
        &stoat_action::GotoNextChange,
        &stoat_action::GotoNextChange,
        &stoat_action::GotoPrevChange,
    ];
    walk.into_iter()
        .map(|action| {
            dispatch(&mut h.stoat, action);
            let (scroll, row, total) = stack_view(&mut h);
            let followed = diff_changes::follow_cursor(&mut h.stoat, 1);
            (scroll, row, total, followed)
        })
        .collect()
}

/// Before the stack's first paint it has no height to center in, so a walk
/// leaves the scroll for the paint to settle.
#[test]
fn a_change_walk_before_the_first_paint_leaves_the_stack_scroll() {
    let mut h = long_files(24);
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    h.settle();

    dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    assert_eq!(stack_view(&mut h).0, 0);
}

/// While the focused excerpt waits on its fold it has no rows in the stack,
/// so a walk in it leaves the stack's scroll where it is.
#[test]
fn a_change_walk_on_an_unfolded_excerpt_leaves_the_stack_scroll() {
    let mut h = long_files(24);
    h.open_file(Path::new("/repo/a.txt"));
    four_row_stack(&mut h);
    let state = h
        .stoat
        .active_workspace_mut()
        .focused_diff_changes_mut()
        .expect("a changes view");
    state.excerpts[state.focused].folded_version = None;
    let before = state.scroll;

    dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    let scroll = h
        .stoat
        .active_workspace_mut()
        .focused_diff_changes_mut()
        .expect("a changes view")
        .scroll;
    assert_eq!(scroll, before);
}

/// A chunk taller than the view has no whole-chunk center to reach, so the
/// stack centers the cursor's row, as an editor does.
#[test]
fn a_change_walk_onto_a_chunk_taller_than_the_view_centers_its_cursor_row() {
    let mut h = TestHarness::with_size(100, 24);
    let base: String = (1..=60).map(|n| format!("{n}\n")).collect();
    let working = base.replace("\n40\n", "\n40\nA\nB\nC\nD\nE\nF\n");
    h.stage_review_scenario("/repo", &[("a.txt", &base, &working)]);
    h.open_file(Path::new("/repo/a.txt"));
    four_row_stack(&mut h);

    dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    let (scroll, row, total) = stack_view(&mut h);
    assert_eq!(scroll, row.saturating_sub(2).min(total - 4));
}

/// Cover the pane of `h` with a settled changes view, paint it once, and
/// shrink its view to four rows, so its center is stack row 2.
fn four_row_stack(h: &mut TestHarness) {
    h.cover_with_diff_changes(false);
    h.settle();
    h.render_composited();
    if let Some(state) = h.stoat.active_workspace_mut().focused_diff_changes_mut() {
        state.viewport_rows = 4;
    }
}

/// The focused stack's scroll, its cursor's stack row, and its row count.
fn stack_view(h: &mut TestHarness) -> (usize, usize, usize) {
    let ws = h.stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id().expect("a changes view");
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    let state = &mut diff_changes_views[id];
    let total = diff_changes::layout_stack(state, editors);
    let row = diff_changes::cursor_stack_row(state, editors).expect("a cursor stack row");
    (state.scroll, row, total)
}

/// The side buttons walk the hunks across the excerpts as `] g` and `[ g`
/// do. A hop into the next excerpt records its landing, so a later press
/// walks on rather than retracing the jumplist out of the changes view.
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
    assert_eq!(
        side_walk(&mut h, &presses),
        [
            (Some("diff_changes"), at("a.txt", 8)),
            (Some("diff_changes"), at("b.txt", 8)),
            (Some("diff_changes"), at("b.txt", 8)),
            (Some("diff_changes"), at("a.txt", 8)),
        ],
    );
}

/// A jumplist push after a landing, with the cursor moved off the landed
/// hunk, leaves the side buttons walking the hunks. A retrace of the push
/// swaps the pane to a plain editor on the saved file, so the buttons
/// never retrace in a changes view.
///
/// The `j` puts the cursor on a.txt's last row, below its one hunk, so the
/// walk goes on to b.txt.
#[test]
fn the_side_buttons_walk_on_after_a_push_off_the_landing() {
    let mut h = deep_files();
    h.cover_with_diff_changes(false);
    h.settle();
    dispatch(&mut h.stoat, &stoat_action::DiffBack);
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("j");
    assert_eq!(
        side_walk(
            &mut h,
            &[&stoat_action::DiffBack, &stoat_action::DiffForward]
        ),
        [
            (Some("diff_changes"), at("b.txt", 8)),
            (Some("diff_changes"), at("a.txt", 8)),
        ],
    );
}

/// Forward history that the pane gathered before the changes view covered
/// it leaves the forward button walking the hunks too. A retrace forward
/// swaps the pane to a plain editor on the entry's file.
///
/// In the plain editor a walk lands a.txt's hunk, a save and a `k` leave
/// it, and a jump back to the save leaves the `k` row as forward history.
#[test]
fn the_forward_button_walks_past_forward_history_from_before_the_view() {
    let mut h = deep_files();
    h.open_file(Path::new("/repo/a.txt"));
    h.settle();
    dispatch(&mut h.stoat, &stoat_action::DiffBack);
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("k");
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    h.cover_with_diff_changes(false);
    h.settle();
    assert_eq!(
        side_walk(&mut h, &[&stoat_action::DiffForward]),
        [(Some("diff_changes"), at("a.txt", 8))],
    );
}

/// The view and the focused excerpt's place after each of `presses`.
fn side_walk(
    h: &mut TestHarness,
    presses: &[&dyn Action],
) -> Vec<(Option<&'static str>, (String, u32))> {
    presses
        .iter()
        .map(|action| {
            dispatch(&mut h.stoat, *action);
            (h.stoat.current_view(), place(h))
        })
        .collect()
}

/// `space h s` on an excerpt's hunk stages it, and the refold that follows
/// the stage keeps the cursor on its buffer position.
#[test]
fn space_h_s_stages_an_excerpt_hunk_and_the_refold_keeps_the_cursor() {
    let mut h = deep_files();
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/b.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    h.type_keys("] g");
    let folded = |h: &TestHarness| {
        h.stoat
            .active_workspace()
            .focused_diff_changes()
            .and_then(DiffChangesState::focused_excerpt)
            .and_then(|excerpt| excerpt.folded_version)
    };
    let before = (place(&mut h), folded(&h));

    h.type_keys("space h s");
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

/// An untracked file with content stacks as an excerpt, and `space h s` on
/// its one added hunk adds the file to the index as the patch that creates
/// it there.
#[test]
fn space_h_s_on_an_untracked_excerpt_adds_the_file_to_the_index() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    h.fake_fs().insert_file(Path::new("/repo/n.txt"), b"x\ny\n");
    h.fake_git().add_repo(Path::new("/repo")).untracked("n.txt");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    h.type_keys("] g");
    let landed = place(&mut h).0;

    h.type_keys("space h s");
    h.settle_diff_jobs();
    assert_eq!(
        (landed, h.fake_git().applied_patches(Path::new("/repo"))),
        (
            "n.txt".to_string(),
            vec![
                "diff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ \
                 b/n.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n"
                    .to_string()
            ],
        ),
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
