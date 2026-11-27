use ratatui::buffer::Buffer;
use slotmap::SlotMap;
use std::{
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
};
use stoat::{
    action_handlers::dispatch,
    auto_reload,
    buffer::BufferId,
    buffer_registry::AutoReloadMode,
    diff_changes::{
        cursor_point, cursor_stack_row, excerpt_rows, expand_from_cursor, follow_cursor,
        header_rows, hidden_lines, kept_rows, layout_stack, listed_files, place_cursor, press_cell,
        pump_diff_changes_stack, reveal_cursor, select_row, toggle_file, DiffChangesState, Excerpt,
        ExpandDir, Landing, StackStamp,
    },
    diff_map::{DiffHunkStatus, DiffMap, DiffStatus},
    display_map::{DisplayPoint, InlayPoint},
    editor_state::{EditorId, EditorState},
    host::{git::FileKind, FakeRepoBuilder},
    test_fixture::diff_changes::{map_of, numbered, scope_text},
    test_harness::TestHarness,
    workspace::{
        diff::{DiffBase, WorktreeBase},
        Workspace,
    },
};
use stoat_action::{CloseDiffChanges, DeleteSelection, DiffChanges, Undo};
use stoat_config::MinimapMode;
use stoat_text::Point;

/// A hunk on a file's first line has no fold above it, so its Scope takes a
/// row under the header. The excerpt's rows and its cursor's stack row
/// start past that row, and a press on the row lands as a press on the
/// header does.
#[test]
fn a_first_line_hunk_takes_a_scope_row_under_its_header() {
    let base = "fn first() -> u32 {\n    1\n}\n// a\n// b\n// c\n// d\n// e\n// f\n// g\n";
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.rs", base, &base.replacen("u32", "u64", 1))]);
    stacked(&mut h);
    // The paint asks for the excerpt's parse, and the refold after it lands
    // reads the Scope.
    h.render_composited();
    for _ in 0..2 {
        h.stoat.drive_background();
        h.settle();
    }

    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = h.stoat.active_workspace_mut();
    let state = diff_changes_views
        .values_mut()
        .next()
        .expect("a changes view");
    let total = layout_stack(state, editors);
    let (editor_id, scope) = {
        let excerpt = &state.excerpts[0];
        (
            excerpt.editor_id,
            excerpt.scope_row.as_deref().map(scope_text),
        )
    };
    let rows = excerpt_rows(&state.excerpts[0], editors);
    place_cursor(&mut editors[editor_id], Point::new(1, 4));
    let display_row = editors[editor_id]
        .display_map
        .snapshot()
        .buffer_to_display(Point::new(1, 4))
        .row as usize;
    let cursor = cursor_stack_row(state, editors);
    let pressed = press_cell(state, editors, 1, 40);

    assert_eq!(
        (
            scope,
            total,
            cursor,
            pressed,
            cursor_point(&mut editors[editor_id])
        ),
        (
            Some("fn first".to_string()),
            2 + rows,
            Some(2 + display_row),
            true,
            Point::new(0, 0)
        ),
    );
}

/// A first-line hunk outside every item names no Scope, so no row shows
/// under the header and the excerpt's rows start right under it.
#[test]
fn a_top_level_first_line_hunk_takes_no_scope_row() {
    let base = "use a::B;\nfn first() -> u32 {\n    1\n}\n";
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario(
        "/repo",
        &[("a.rs", base, &base.replacen("a::B", "a::C", 1))],
    );
    stacked(&mut h);
    h.render_composited();
    for _ in 0..2 {
        h.stoat.drive_background();
        h.settle();
    }

    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = h.stoat.active_workspace_mut();
    let state = diff_changes_views
        .values_mut()
        .next()
        .expect("a changes view");
    let total = layout_stack(state, editors);
    let excerpt = &state.excerpts[0];

    assert_eq!(
        (excerpt.scope_row.clone(), header_rows(excerpt), total),
        (None, 1, 1 + excerpt_rows(excerpt, editors)),
    );
}

/// The Scope of the cursor's row follows the row. It holds while the parse
/// lags an edit, and reads again once a parse of the edited text lands.
#[test]
fn the_cursor_scope_holds_until_a_parse_of_the_edit_lands() {
    let base =
        "mod outer {\n    impl Theme {\n        fn panel() -> u32 {\n            1\n        \
                }\n    }\n}\n";
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.rs", base, &base.replacen('1', "2", 1))]);
    stacked(&mut h);
    let parse = |h: &mut TestHarness| {
        h.render_composited();
        for _ in 0..2 {
            h.stoat.drive_background();
            h.settle();
        }
    };
    let read = |h: &mut TestHarness, row: Option<u32>| {
        let Workspace {
            diff_changes_views,
            editors,
            buffers,
            ..
        } = h.stoat.active_workspace_mut();
        let state = diff_changes_views
            .values_mut()
            .next()
            .expect("a changes view");
        if let Some(row) = row {
            place_cursor(
                &mut editors[state.excerpts[0].editor_id],
                Point::new(row, 0),
            );
        }
        state
            .cursor_scope(editors, buffers)
            .as_deref()
            .map(scope_text)
    };

    parse(&mut h);
    let body = read(&mut h, Some(3));
    let header = read(&mut h, Some(1));
    {
        let ws = h.stoat.active_workspace();
        let id = ws
            .buffers
            .id_for_path(Path::new("/repo/a.rs"))
            .expect("an open buffer");
        ws.buffers
            .get(id)
            .expect("buffer")
            .write()
            .expect("buffer poisoned")
            .edit(0..0, "//\n");
    }
    let held = read(&mut h, None);
    parse(&mut h);
    let reread = read(&mut h, Some(4));

    let function = Some("mod outer \u{203a} impl Theme \u{203a} fn panel".to_string());
    let block = Some("mod outer \u{203a} impl Theme".to_string());
    assert_eq!(
        [body, header, held, reread],
        [function.clone(), block.clone(), block, function],
    );
}

/// A landing on a row its excerpt shows holds its screen row for the next
/// paint. A landing that a fold moves to another row shows another line,
/// so it holds none.
#[test]
fn only_a_landing_on_a_shown_row_holds_its_screen_row() {
    let mut h = TestHarness::with_size(100, 24);
    let base = numbered(20);
    h.stage_review_scenario(
        "/repo",
        &[("a.txt", &base, &base.replace("\n10\n", "\nTEN\n"))],
    );
    stacked(&mut h);
    let land_on = |row| {
        let state = h
            .stoat
            .active_workspace_mut()
            .focused_diff_changes_mut()
            .expect("a changes view");
        state.pending_landing = Some(Landing {
            buffer_id: state.excerpts[0].buffer_id,
            rel_path: "a.txt".to_string(),
            row,
            column: 0,
            screen_row: Some(3),
        });
        pump_diff_changes_stack(&mut h.stoat);
        h.stoat
            .active_workspace()
            .focused_diff_changes()
            .and_then(|state| state.held_screen_row)
    };
    // TEN sits on row 9, inside the shown run of rows 6 to 12, and row 2
    // lies in the fold above it.
    assert_eq!([9, 2].map(land_on), [Some(3), None]);
}

/// A row under the pinned header uncovers to the first row under it, or to
/// its file's first row with the header in place. The follow keeps its
/// scrolloff band under the pinned row.
///
/// Each file takes a header row and twelve paired rows, with no final
/// newline to add an empty row. So a.txt takes stack rows 0 to 12 and
/// b.txt rows 13 to 25, in a view of eleven rows.
#[test]
fn the_scroll_keeps_rows_clear_of_the_pinned_header() {
    let base = (1..=12)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let text = (1..=12)
        .map(|n| format!("{n}!"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut h = TestHarness::with_size(120, 12);
    h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
    h.stage_review_scenario("/repo", &[("a.txt", &base, &text), ("b.txt", &base, &text)]);
    stacked(&mut h);
    h.render_composited();
    let state = |h: &mut TestHarness| {
        let views = &mut h.stoat.active_workspace_mut().diff_changes_views;
        views.values_mut().next().expect("a changes view").scroll
    };

    let uncovered = [(5, 5), (1, 1), (15, 14)].map(|(scroll, row)| {
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = h.stoat.active_workspace_mut();
        let state = diff_changes_views
            .values_mut()
            .next()
            .expect("a changes view");
        state.scroll = scroll;
        state.uncover_row(editors, row);
        state.scroll
    });
    let followed = [(0, 7, 7), (0, 9, 7), (1, 2, 15)].map(|(index, buffer_row, scroll)| {
        {
            let Workspace {
                diff_changes_views,
                editors,
                ..
            } = h.stoat.active_workspace_mut();
            let state = diff_changes_views
                .values_mut()
                .next()
                .expect("a changes view");
            state.focused = index;
            state.scroll = scroll;
            let editor = &mut editors[state.excerpts[index].editor_id];
            place_cursor(editor, Point::new(buffer_row, 0));
        }
        follow_cursor(&mut h.stoat, 3);
        state(&mut h)
    });

    assert_eq!((uncovered, followed), ([4, 0, 13], [4, 6, 13]));
}

/// The repository at /repo, with the harness's fake files under it.
fn repo(h: &mut TestHarness) -> FakeRepoBuilder<'_> {
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo(PathBuf::from("/repo"))
        .with_fs(h.fake_fs())
}

/// The focused changes view's excerpts as each file's path and whether its
/// folds are cut.
fn excerpts(h: &TestHarness) -> Vec<(String, bool)> {
    h.stoat
        .active_workspace()
        .focused_diff_changes()
        .expect("a focused changes view")
        .excerpts
        .iter()
        .map(|excerpt| (excerpt.rel_path.clone(), excerpt.folded_version.is_some()))
        .collect()
}

fn listed(paths: &[&str]) -> Vec<(String, bool)> {
    paths.iter().map(|path| (path.to_string(), true)).collect()
}

/// A changes view over the harness's focused view, settled.
fn stacked(h: &mut TestHarness) {
    h.cover_with_diff_changes(false);
    h.settle();
}

/// Each changed file and each untracked file with content gets an excerpt,
/// folded, in path order.
#[test]
fn changed_and_untracked_files_build_folded_excerpts_in_path_order() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario(
        "/repo",
        &[
            ("c.txt", "c\n", "C\n"),
            ("a.txt", "a\n", "A\n"),
            ("b.txt", "b\n", "B\n"),
        ],
    );
    repo(&mut h).untracked("z.txt");
    h.fake_fs()
        .insert_file(Path::new("/repo/z.txt"), b"scratch\n");

    stacked(&mut h);
    assert_eq!(excerpts(&h), listed(&["a.txt", "b.txt", "c.txt", "z.txt"]));
}

/// Under the HEAD base a file that moved with no edit and an empty new
/// file stack beside an edited file, though neither holds a hunk, so a move
/// never leaves the stack and an empty file shows.
#[test]
fn a_file_moved_with_no_edit_and_an_empty_new_file_get_excerpts() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    repo(&mut h)
        .renamed("old.txt", "moved.txt", "same\n")
        .empty_untracked("empty.txt");
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Head);

    stacked(&mut h);
    assert_eq!(excerpts(&h), listed(&["a.txt", "empty.txt", "moved.txt"]));
}

/// Against the index a staged move with no edit leaves the working tree
/// as the index holds it, so the move takes no excerpt.
#[test]
fn a_staged_move_stays_off_a_stack_against_the_index() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    repo(&mut h).renamed("old.txt", "moved.txt", "same\n");
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);

    stacked(&mut h);
    assert_eq!(excerpts(&h), listed(&["a.txt"]));
}

/// An empty file with no base takes a note row under its header and shows
/// no row, so no cursor stands in it. `j` from the last row of the file
/// above passes it, and a focus that starts on it moves to the file below
/// at the next pump.
#[test]
fn an_empty_file_with_no_base_takes_a_note_row_and_no_cursor() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("c.txt", "c\n", "C\n")]);
    repo(&mut h).empty_untracked("b.txt");
    stacked(&mut h);

    let empty = with_stack(&mut h, |state, editors| {
        let excerpt = &state.excerpts[1];
        let rows = (
            excerpt.rel_path.clone(),
            excerpt.empty,
            header_rows(excerpt),
            excerpt_rows(excerpt, editors),
        );
        state.focused = 0;
        place_cursor(&mut editors[state.excerpts[0].editor_id], Point::new(1, 0));
        rows
    });
    h.type_keys("j");
    let after_j = focus(&mut h).0;
    with_stack(&mut h, |state, _| state.focused = 1);
    pump_diff_changes_stack(&mut h.stoat);

    assert_eq!(
        (empty, after_j, focus(&mut h).0),
        (
            ("b.txt".to_string(), true, 2, 0),
            "c.txt".to_string(),
            "c.txt".to_string()
        ),
    );
}

/// A deleted file opens folded to its header, which counts the lines of its
/// old text, and a focus that starts on it moves to the modified file
/// beside it at the next pump.
#[test]
fn a_deleted_file_opens_folded_to_its_header() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("b.txt", "b\n", "B\n")]);
    repo(&mut h).deleted("a.txt", "one\ntwo\nthree\n");
    stacked(&mut h);

    let folded = with_stack(&mut h, |state, editors| {
        let excerpt = &state.excerpts[0];
        let shape = (
            excerpt.rel_path.clone(),
            header_rows(excerpt),
            excerpt_rows(excerpt, editors),
        );
        state.focused = 0;
        shape
    });
    pump_diff_changes_stack(&mut h.stoat);
    let hidden = {
        let ws = h.stoat.active_workspace();
        let state = ws.focused_diff_changes().expect("a changes view");
        hidden_lines(&state.excerpts[0], &FileKind::Deleted, &ws.buffers)
    };

    assert_eq!(
        (folded, focus(&mut h).0, hidden),
        (("a.txt".to_string(), 1, 0), "b.txt".to_string(), 3),
    );
}

/// A folded file is no stop for `j`, which steps from the last row of the
/// file above it to the first row of the file below it.
#[test]
fn a_folded_file_is_no_stop_for_a_row_walk() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario(
        "/repo",
        &[
            ("a.txt", "a\n", "A\n"),
            ("b.txt", "b\n", "B\n"),
            ("c.txt", "c\n", "C\n"),
        ],
    );
    stacked(&mut h);
    with_stack(&mut h, |state, editors| {
        toggle_file(state, editors, 1);
        state.focused = 0;
        place_cursor(&mut editors[state.excerpts[0].editor_id], Point::new(1, 0));
    });

    h.type_keys("j");
    assert_eq!(focus(&mut h), ("c.txt".to_string(), 0));
}

/// A landing on a folded file opens it, so the file shows its rows again,
/// and its cursor takes the landing's row.
#[test]
fn a_landing_on_a_folded_file_opens_it() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
    stacked(&mut h);
    let (shown, folded) = with_stack(&mut h, |state, editors| {
        let shown = excerpt_rows(&state.excerpts[1], editors);
        toggle_file(state, editors, 1);
        state.pending_landing = Some(Landing {
            buffer_id: state.excerpts[1].buffer_id,
            rel_path: "b.txt".to_string(),
            row: 0,
            column: 0,
            screen_row: None,
        });
        (shown, excerpt_rows(&state.excerpts[1], editors))
    });

    pump_diff_changes_stack(&mut h.stoat);
    let opened = with_stack(&mut h, |state, editors| {
        excerpt_rows(&state.excerpts[1], editors)
    });
    assert_eq!(
        (folded, opened, focus(&mut h)),
        (0, shown, ("b.txt".to_string(), 0)),
        "b.txt shows {shown} rows open",
    );
}

/// A landing on a file outside the stack falls on the first file and opens
/// nothing, so a deleted file that sorts first stays folded, and the focus
/// moves on to the file below it.
#[test]
fn a_landing_outside_the_stack_leaves_a_deleted_first_file_folded() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("b.txt", "b\n", "B\n")]);
    repo(&mut h)
        .deleted("a.txt", "one\n")
        .head_file("d.txt", "same\n");
    h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
    h.open_file(Path::new("/repo/d.txt"));
    dispatch(&mut h.stoat, &DiffChanges);
    h.settle();

    let folds = with_stack(&mut h, |state, _| {
        state
            .excerpts
            .iter()
            .map(|excerpt| (excerpt.rel_path.clone(), excerpt.collapsed))
            .collect::<Vec<_>>()
    });
    let fold = |path: &str, collapsed| (path.to_string(), collapsed);
    assert_eq!(
        (folds, focus(&mut h).0),
        (
            vec![fold("a.txt", true), fold("b.txt", false)],
            "b.txt".to_string()
        ),
    );
}

/// A file with no change folds whole under one fold row, whose Scope reads
/// `content unchanged` in place of a chain, though a text file's language
/// has no outline query.
#[test]
fn a_file_with_no_change_folds_whole_under_content_unchanged() {
    let mut h = TestHarness::with_size(100, 24);
    repo(&mut h).renamed("old.txt", "moved.txt", "one\ntwo\nthree\n");
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Head);
    stacked(&mut h);

    assert_eq!(
        fold_rows_of(&mut h, "moved.txt"),
        vec![(0..4, "content unchanged".to_string())],
    );
}

/// A long untracked file shows its first eleven rows and one fold row over
/// the rest, whose Scope reads `rest of the new file` where a parse of the
/// file reads the end of the file.
#[test]
fn a_long_untracked_file_folds_its_rest_under_a_label() {
    let mut h = TestHarness::with_size(100, 24);
    let text: String = (1..=64).map(|n| format!("line {n}\n")).collect();
    repo(&mut h).untracked("notes.md");
    h.fake_fs()
        .insert_file(Path::new("/repo/notes.md"), text.as_bytes());
    stacked(&mut h);
    // The paint asks for the excerpt's parse, and the refold after it lands
    // reads the Scope.
    h.render_composited();
    for _ in 0..2 {
        h.stoat.drive_background();
        h.settle();
    }

    let rows = {
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = h.stoat.active_workspace_mut();
        let state = diff_changes_views.values().next().expect("a changes view");
        excerpt_rows(&state.excerpts[0], editors)
    };
    assert_eq!(
        (rows, fold_rows_of(&mut h, "notes.md")),
        (12, vec![(11..65, "rest of the new file".to_string())]),
    );
}

/// The fold rows of the excerpt of `rel_path`, each as the buffer rows it
/// hides and the text of its Scope.
fn fold_rows_of(h: &mut TestHarness, rel_path: &str) -> Vec<(Range<u32>, String)> {
    let snapshot = excerpt_editor(h, rel_path).display_map.snapshot();
    let folds = snapshot.fold_snapshot();
    let inlays = folds.inlay_snapshot();
    folds
        .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
        .into_iter()
        .filter(|fold| fold.placeholder.is_fold_row())
        .map(|fold| {
            (
                inlays.to_buffer_point(fold.range.start).row
                    ..inlays.to_buffer_point(fold.range.end).row + 1,
                fold.placeholder
                    .scope
                    .as_deref()
                    .map(scope_text)
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// Under a revision base an untracked file with content stacks beside the
/// files changed since the revision, as the change walk under that base
/// visits it.
#[test]
fn a_revision_base_lists_an_untracked_file() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    repo(&mut h)
        .untracked("z.txt")
        .commit("base0", &[("a.txt", "a\n")]);
    h.fake_fs()
        .insert_file(Path::new("/repo/z.txt"), b"scratch\n");
    dispatch(
        &mut h.stoat,
        &stoat_action::DiffFiles {
            rev: Some("base0".into()),
        },
    );
    h.settle();

    stacked(&mut h);
    assert_eq!(excerpts(&h), listed(&["a.txt", "z.txt"]));
}

/// An excerpt shows its hunk with three rows of context each side, and the
/// rows beyond them fold away.
#[test]
fn an_excerpt_folds_every_row_beyond_the_context_of_its_hunks() {
    let mut h = TestHarness::with_size(100, 24);
    let base = numbered(20);
    h.stage_review_scenario(
        "/repo",
        &[("a.txt", &base, &base.replace("\n10\n", "\nTEN\n"))],
    );
    stacked(&mut h);

    let ws = h.stoat.active_workspace_mut();
    let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
    let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
    let snapshot = editor.display_map.snapshot();
    let shown: Vec<u32> = (0..snapshot.line_count())
        .filter_map(|row| snapshot.display_to_buffer(DisplayPoint::new(row, 0)))
        .map(|point| point.row)
        .collect();
    // The fold row at row 0 stands for the folded top, rows 6 to 12 show the
    // hunk at row 9 with its context, and the fold row at row 13 stands for
    // the rows below. The base line the hunk replaced takes a row with no
    // buffer row.
    assert_eq!(shown, [0, 6, 7, 8, 9, 10, 11, 12, 13]);
}

/// An unsaved edit to a clean open file puts the file in the list, which
/// reads the open buffer's diff map.
#[test]
fn an_unsaved_edit_to_a_clean_file_gets_an_excerpt() {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    repo(&mut h)
        .head_file("d.txt", "same\n")
        .head_file("f.txt", "kept\n");
    h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
    h.fake_fs().insert_file(Path::new("/repo/f.txt"), b"kept\n");
    h.open_file(Path::new("/repo/f.txt"));
    h.open_file(Path::new("/repo/d.txt"));
    h.type_keys("i n e w Escape");
    h.settle_diff_jobs();

    stacked(&mut h);
    assert_eq!(excerpts(&h), listed(&["d.txt"]));
}

/// Ten tests, each a doc comment, an attribute, and a function of three
/// lines.
fn test_blocks() -> String {
    (1..=10)
        .map(|n| {
            format!("/// Case {n}.\n#[test]\nfn case_{n}() {{\n    check(rows({n}), {n});\n}}\n")
        })
        .collect()
}

/// `pub fn scan() {}` with `blocks` in a new `mod tests` below it.
fn lib_with_tests(blocks: &str) -> String {
    let indented: String = blocks.lines().map(|line| format!("    {line}\n")).collect();
    format!("pub fn scan() {{}}\n\n#[cfg(test)]\nmod tests {{\n{indented}}}\n")
}

/// A changes view over `files` in /repo, with every diff map settled.
fn settled_stack(files: &[(&str, &str, &str)]) -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    h.stage_review_scenario("/repo", files);
    stacked(&mut h);
    h.settle_diff_jobs();
    h
}

/// The diff map of the open buffer of `path`.
fn installed_map(h: &TestHarness, path: &str) -> DiffMap {
    let ws = h.stoat.active_workspace();
    let id = ws
        .buffers
        .id_for_path(Path::new(path))
        .expect("an open buffer");
    let buffer = ws.buffers.get(id).expect("a buffer");
    let guard = buffer.read().expect("buffer poisoned");
    guard.diff_map.clone().expect("a diff map")
}

/// Tests that leave their own file for a new module of another file read as
/// moved in both files, and the module's own rows read as added.
#[test]
fn tests_that_move_into_another_files_module_read_as_moved() {
    let blocks = test_blocks();
    let lib = lib_with_tests(&blocks);
    let h = settled_stack(&[
        ("tests.rs", &blocks, ""),
        ("lib.rs", "pub fn scan() {}\n", &lib),
    ]);

    let tests_map = installed_map(&h, "/repo/tests.rs");
    let lib_map = installed_map(&h, "/repo/lib.rs");
    let mut lib_rows = vec![
        DiffStatus::Unchanged,
        DiffStatus::Added,
        DiffStatus::Added,
        DiffStatus::Added,
    ];
    lib_rows.extend([DiffStatus::Moved; 50]);
    lib_rows.push(DiffStatus::Added);
    assert_eq!(
        (
            tests_map
                .hunks()
                .map(|hunk| hunk.status)
                .collect::<Vec<_>>(),
            (0..55)
                .map(|row| lib_map.status_for_line(row))
                .collect::<Vec<_>>(),
        ),
        (vec![DiffHunkStatus::Moved], lib_rows),
        "the removed file moved whole, and only the module's own rows read as added",
    );
}

/// A copy of tests that stay in their own file is an addition, not a move.
#[test]
fn a_copy_in_another_file_stays_added() {
    let blocks = test_blocks();
    let h = settled_stack(&[
        ("tests.rs", &blocks, &format!("// A new note.\n{blocks}")),
        ("lib.rs", "pub fn scan() {}\n", &lib_with_tests(&blocks)),
    ]);

    let lib_map = installed_map(&h, "/repo/lib.rs");
    let mut lib_rows = vec![DiffStatus::Unchanged];
    lib_rows.extend([DiffStatus::Added; 54]);
    assert_eq!(
        (0..55)
            .map(|row| lib_map.status_for_line(row))
            .collect::<Vec<_>>(),
        lib_rows,
        "no row of the copy reads as moved",
    );
}

/// A stack over the changed a.txt with the clean d.txt open under it, and
/// d.txt's buffer.
fn stack_beside_clean_file(h: &mut TestHarness) -> BufferId {
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    repo(h).head_file("d.txt", "same\n");
    h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
    h.open_file(Path::new("/repo/d.txt"));
    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
    stacked(h);
    buffer_id
}

/// Write `text` at the start of buffer `id`, unsaved.
fn prepend(h: &TestHarness, id: BufferId, text: &str) {
    h.stoat
        .active_workspace()
        .buffers
        .get(id)
        .expect("buffer")
        .write()
        .expect("buffer poisoned")
        .edit(0..0, text);
}

/// An excerpt counts the conflict blocks its text holds at each cut. A
/// file whose working text holds two marker blocks counts 2, and an edit
/// that takes the first block out counts 1 once the next cut lands.
#[test]
fn an_excerpt_counts_the_conflict_blocks_of_its_text() {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    let block = |side: &str| format!("<<<<<<< HEAD\n{side}\n=======\ntheirs\n>>>>>>> theirs\n");
    let working = format!("a\n{}b\n{}c\n", block("one"), block("two"));
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\nc\n", &working)]);
    h.open_file(Path::new("/repo/a.txt"));
    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
    stacked(&mut h);
    h.settle_diff_jobs();
    let conflicts = |h: &mut TestHarness| with_stack(h, |state, _| state.excerpts[0].conflicts);
    let opened = conflicts(&mut h);

    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("buffer poisoned")
        .edit(2..2 + block("one").len(), "one\n");
    h.settle_diff_jobs();

    assert_eq!((opened, conflicts(&mut h)), (2, 1));
}

/// An excerpt's rows follow its editor's display rows, after a refold and
/// after an edit that adds a row before any diff map lands.
#[test]
fn excerpt_rows_follow_a_refold_and_an_edit() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    h.open_file(Path::new("/repo/a.txt"));
    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
    stacked(&mut h);
    let rows = |h: &mut TestHarness| {
        let ws = h.stoat.active_workspace_mut();
        let id = ws.focused_diff_changes_id().expect("a changes view");
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = ws;
        let excerpt = &diff_changes_views[id].excerpts[0];
        let shown = excerpt_rows(excerpt, editors);
        let display = editors[excerpt.editor_id].display_map.snapshot();
        (shown, display.line_count() as usize)
    };

    let folded = rows(&mut h);
    prepend(&h, buffer_id, "new\n");
    let edited = rows(&mut h);
    let count = folded.1;
    assert_eq!([folded, edited], [(count, count), (count + 1, count + 1)]);
}

/// The rows an expand opened follow their lines through an edit above them,
/// so the refold the edit brings keeps them open.
#[test]
fn a_reveal_follows_its_lines_through_an_edit() {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    let base = numbered(60);
    let working = base.replace("\n41\n", "\nFORTY-ONE\n");
    h.stage_review_scenario("/repo", &[("a.txt", &base, &working)]);
    h.open_file(Path::new("/repo/a.txt"));
    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
    stacked(&mut h);
    h.settle_diff_jobs();
    {
        let ws = h.stoat.active_workspace_mut();
        let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
        let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
        place_cursor(editor, Point::new(40, 0));
    }
    let opened = expand_from_cursor(&mut h.stoat, ExpandDir::Up);

    prepend(&h, buffer_id, "new\n");
    h.settle_diff_jobs();
    assert_eq!(
        (opened, shown_rows(&mut h)),
        (Ok(()), rows_of(&[0..=3, 4..=4, 28..=44, 45..=45])),
    );
}

/// The buffer row each display row of the focused excerpt starts with, so a
/// fold row reads as the first row it hides.
fn shown_rows(h: &mut TestHarness) -> Vec<u32> {
    let ws = h.stoat.active_workspace_mut();
    let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
    let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
    let snapshot = editor.display_map.snapshot();
    (0..snapshot.line_count())
        .filter_map(|row| snapshot.display_to_buffer(DisplayPoint::new(row, 0)))
        .map(|point| point.row)
        .collect()
}

/// The rows of `runs`, one after another.
fn rows_of(runs: &[RangeInclusive<u32>]) -> Vec<u32> {
    runs.iter().flat_map(|run| run.clone()).collect()
}

/// Excerpts over long files that paint before their hunks land show the
/// rows of excerpts that waited for the hunks to paint.
///
/// One scheduler pass lands the tally, which opens each excerpt unfolded
/// with its hunks still on the pool. A paint then gives each excerpt its
/// first width, which a file this long wraps on the pool, and the folds
/// that the hunks bring land while that rewrap runs.
#[test]
fn excerpts_folded_under_a_pending_wrap_show_their_folded_rows() {
    let stack_rows = |paint_before_hunks: bool| {
        let mut h = TestHarness::with_size(160, 48);
        let base = numbered(120);
        let working = base.replace("\n60\n", "\nSIXTY\n");
        h.stage_review_scenario(
            "/repo",
            &[("a.txt", &base, &working), ("b.txt", &base, &working)],
        );
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_diff_changes(false);
        h.run_until_parked();
        h.stoat.drive_pumps();
        if paint_before_hunks {
            h.stoat.render();
        }
        h.settle();
        h.stoat.render();
        h.stoat.render();

        let ws = h.stoat.active_workspace_mut();
        let id = ws.focused_diff_changes_id().expect("a changes view");
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = ws;
        diff_changes_views[id]
            .excerpts
            .iter()
            .map(|excerpt| excerpt_rows(excerpt, editors))
            .collect::<Vec<_>>()
    };
    let waited = stack_rows(false);
    let folded = waited.iter().all(|rows| (1..121).contains(rows));
    assert_eq!((stack_rows(true), folded), (waited, true));
}

/// An unsaved edit to a clean open file joins a stack that already
/// exists, once the file's diff map lands.
#[test]
fn a_landing_outside_the_stack_relists_it() {
    let mut h = TestHarness::with_size(100, 24);
    let clean = stack_beside_clean_file(&mut h);
    let before = excerpts(&h);

    prepend(&h, clean, "new ");
    h.settle_diff_map(clean);
    assert_eq!(
        (before, excerpts(&h)),
        (listed(&["a.txt"]), listed(&["a.txt", "d.txt"])),
    );
}

/// A relist reads the root the workspace resolved, so a moved stamp lists
/// the files again with no repository opened.
#[test]
fn a_relist_opens_no_repository() {
    let mut h = TestHarness::with_size(100, 24);
    let clean = stack_beside_clean_file(&mut h);
    let opened = h.fake_git().discover_calls();

    prepend(&h, clean, "new ");
    h.stoat
        .active_workspace_mut()
        .install_test_diff_map(clean, map_of("same\n", "new same\n"));
    h.settle();
    assert_eq!(
        (excerpts(&h), h.fake_git().discover_calls() - opened),
        (listed(&["a.txt", "d.txt"]), 0),
    );
}

/// A base switch moves every excerpt's diff map, so each excerpt cuts its
/// folds again.
#[test]
fn a_base_switch_refolds_the_excerpts() {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    h.stage_index_scenario("/repo", &[("e.txt", "head\n", "index\n", "work\n")]);
    // Set straight on the workspace, since the base actions also turn the
    // diff view on in the focused pane.
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    stacked(&mut h);
    let folded = |h: &TestHarness| {
        h.stoat
            .active_workspace()
            .focused_diff_changes()
            .and_then(|state| state.excerpts.first())
            .and_then(|excerpt| excerpt.folded_version)
    };
    let index = folded(&h);

    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Head);
    h.settle_diff_jobs();
    let head = folded(&h);
    assert_eq!(
        (index.is_some(), head.is_some(), index != head),
        (true, true, true)
    );
}

/// A base switch that takes the hunk around the cursor folds the cursor's
/// row, and the cursor moves to the first row the excerpt shows below it.
#[test]
fn a_base_switch_that_hides_the_cursor_row_moves_the_cursor_to_a_shown_row() {
    let mut h = index_stack(&[(0, "staged")], &[(0, "staged"), (25, "work")]);
    place_cursor(excerpt_editor(&mut h, "e.txt"), Point::new(2, 0));
    let before = focus(&mut h);

    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    settle_cut(&mut h);
    assert_eq!(
        (before, focus(&mut h), kept_of(&mut h, "e.txt")),
        (at("e.txt", 2), at("e.txt", 22), vec![(22, 29)]),
    );
}

/// A cursor that the reader put inside a fold keeps its row open through a
/// base switch.
#[test]
fn a_cursor_held_inside_a_fold_keeps_its_row_through_a_base_switch() {
    let mut h = index_stack(&[(0, "staged")], &[(0, "staged"), (25, "work")]);
    place_cursor(excerpt_editor(&mut h, "e.txt"), Point::new(12, 0));
    reveal_cursor(&mut h.stoat);

    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    settle_cut(&mut h);
    assert_eq!(focus(&mut h), at("e.txt", 12));
}

/// A reload that takes the hunk around the cursor is no edit of the
/// reader's, so the cursor moves to a shown row, as at a base switch.
#[test]
fn a_reload_that_hides_the_cursor_row_moves_the_cursor_to_a_shown_row() {
    let mut h = work_row_stack(&["e.txt"]);
    reload_file(&mut h, "e.txt", &lines(&[(25, "work")]));
    settle_cut(&mut h);
    assert_eq!(focus(&mut h), at("e.txt", 22));
}

/// The reader's own insert-mode edit that takes the hunk around the cursor
/// keeps the cursor's row open.
#[test]
fn an_insert_edit_that_hides_the_cursor_row_keeps_the_row_open() {
    let mut h = work_row_stack(&["e.txt"]);
    excerpt_editor(&mut h, "e.txt").mode = "insert".into();
    replace_first(&mut h, "e.txt", "work", "l0");
    settle_cut(&mut h);
    assert_eq!(focus(&mut h), at("e.txt", 2));
}

/// A later cut that no edit of the reader's moves keeps the hold that the
/// reader's insert-mode edit took, because the excerpt carries the hold.
#[test]
fn an_insert_edit_hold_lasts_through_a_base_switch() {
    let mut h = index_stack(&[(25, "staged")], &[(0, "work"), (25, "work")]);
    place_cursor(excerpt_editor(&mut h, "e.txt"), Point::new(2, 0));
    excerpt_editor(&mut h, "e.txt").mode = "insert".into();
    replace_first(&mut h, "e.txt", "work", "l0");
    settle_cut(&mut h);
    let edited = focus(&mut h);

    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    settle_cut(&mut h);
    assert_eq!((edited, focus(&mut h)), (at("e.txt", 2), at("e.txt", 2)));
}

/// An edit outside the focused excerpt is no edit of the reader's in the
/// stack, so the cursor of the excerpt it hid moves to a shown row, and
/// the focus stays where it was.
#[test]
fn an_edit_out_of_focus_that_hides_a_cursor_row_moves_that_cursor() {
    let mut h = work_row_stack(&["e.txt", "f.txt"]);
    replace_first(&mut h, "f.txt", "work", "l0");
    settle_cut(&mut h);
    assert_eq!(
        (
            focus(&mut h),
            cursor_point(excerpt_editor(&mut h, "f.txt")).row
        ),
        (at("e.txt", 2), 22),
    );
}

/// In a file with a grammar the parse of an edit lands before its diff
/// map, and the cut that the parse moves leaves the insert-mode edit for
/// the cut of the diff map to see.
#[test]
fn an_insert_edit_that_hides_the_cursor_row_keeps_it_open_past_the_parse() {
    let mut h = work_row_stack(&["e.rs"]);
    land_parse(&mut h);
    let before = cut_from(&h, "e.rs");
    excerpt_editor(&mut h, "e.rs").mode = "insert".into();
    replace_first(&mut h, "e.rs", "work", "l0");
    land_parse(&mut h);
    let parsed = cut_from(&h, "e.rs");

    settle_cut(&mut h);
    assert_eq!(
        (
            before.1.is_some(),
            parsed.0 == before.0,
            parsed.1 != before.1,
            focus(&mut h)
        ),
        (true, true, true, at("e.rs", 2)),
    );
}

/// Paint the stack, which asks for the parse of each excerpt, and let the
/// parses land, with no diff map spawned.
fn land_parse(h: &mut TestHarness) {
    h.render_composited();
    for _ in 0..2 {
        h.stoat.drive_background();
        h.settle();
    }
}

/// The text of a file of 30 rows, `l0` to `l29`, with each row that `edit`
/// names replaced.
fn lines(edit: &[(usize, &str)]) -> String {
    (0..30)
        .map(|row| match edit.iter().find(|(at, _)| *at == row) {
            Some((_, text)) => format!("{text}\n"),
            None => format!("l{row}\n"),
        })
        .collect()
}

/// A settled changes view against HEAD over e.txt, whose index and working
/// tree each change the rows that `index` and `work` name.
fn index_stack(index: &[(usize, &str)], work: &[(usize, &str)]) -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    h.stage_index_scenario(
        "/repo",
        &[("e.txt", &lines(&[]), &lines(index), &lines(work))],
    );
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Head);
    stacked(&mut h);
    h.settle_diff_jobs();
    h
}

/// A settled changes view over `rel_paths`, each changed to `work` on row
/// 0 and on row 25, with each cursor on row 2 in the context of the change
/// on row 0.
fn work_row_stack(rel_paths: &[&str]) -> TestHarness {
    let (head, work) = (lines(&[]), lines(&[(0, "work"), (25, "work")]));
    let files: Vec<(&str, &str, &str)> = rel_paths
        .iter()
        .map(|path| (*path, head.as_str(), work.as_str()))
        .collect();
    let mut h = settled_stack(&files);
    for path in rel_paths {
        place_cursor(excerpt_editor(&mut h, path), Point::new(2, 0));
    }
    h
}

/// Replace the first `from` in `rel_path` with `to`, as an edit with no
/// reload behind it.
fn replace_first(h: &mut TestHarness, rel_path: &str, from: &str, to: &str) {
    let buffer_id = excerpt_of(h.stoat.active_workspace(), rel_path).buffer_id;
    let start = h
        .buffer_text(buffer_id)
        .find(from)
        .expect("the replaced text");
    let buffer = h.stoat.active_workspace().buffers.get(buffer_id);
    buffer
        .expect("a buffer")
        .write()
        .expect("buffer poisoned")
        .edit(start..start + from.len(), to);
}

/// Let the stack cut its folds from the diff maps that follow a change.
fn settle_cut(h: &mut TestHarness) {
    h.settle_diff_jobs();
    h.settle();
}

/// The excerpt of `rel_path` in the focused changes view.
fn excerpt_of<'a>(ws: &'a Workspace, rel_path: &str) -> &'a Excerpt {
    ws.focused_diff_changes()
        .and_then(|state| {
            state
                .excerpts
                .iter()
                .find(|excerpt| excerpt.rel_path == rel_path)
        })
        .expect("an excerpt of the file")
}

/// The editor of the excerpt of `rel_path` in the focused changes view.
fn excerpt_editor<'a>(h: &'a mut TestHarness, rel_path: &str) -> &'a mut EditorState {
    let ws = h.stoat.active_workspace_mut();
    let editor_id = excerpt_of(ws, rel_path).editor_id;
    ws.editors.get_mut(editor_id).expect("an editor")
}

/// The rows the excerpt of `rel_path` shows with no cursor row held open,
/// as each run's first row and the row past its last.
fn kept_of(h: &mut TestHarness, rel_path: &str) -> Vec<(u32, u32)> {
    let revealed = excerpt_of(h.stoat.active_workspace(), rel_path)
        .revealed
        .clone();
    kept_rows(excerpt_editor(h, rel_path), &revealed)
        .into_iter()
        .map(|run| (run.start, run.end))
        .collect()
}

/// The diff map version and the syntax version that the folds of the
/// excerpt of `rel_path` were cut from.
fn cut_from(h: &TestHarness, rel_path: &str) -> (Option<usize>, Option<u64>) {
    let excerpt = excerpt_of(h.stoat.active_workspace(), rel_path);
    (excerpt.folded_version, excerpt.folded_syntax)
}

/// One excerpt's folds: the diff map version they were cut from, the fold
/// snapshot's version, and each fold's id with the buffer rows it hides.
#[derive(Debug, PartialEq)]
struct Cut {
    map: Option<usize>,
    version: usize,
    folds: Vec<(usize, Range<u32>)>,
}

/// A changes view over a file of 60 numbered lines with a hunk on rows 10,
/// 30, and 50, so its folds hide rows 0..7, 14..27, 34..47, and 54..61.
/// Diff maps stay warm, as in the app, so an edit refolds the excerpt.
fn three_hunk_stack() -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    let base = numbered(60);
    let text = base
        .replace("\n11\n", "\nELEVEN\n")
        .replace("\n31\n", "\nTHIRTY-ONE\n")
        .replace("\n51\n", "\nFIFTY-ONE\n");
    h.stage_review_scenario("/repo", &[("a.txt", &base, &text)]);
    h.open_file(Path::new("/repo/a.txt"));
    h.cover_with_diff_changes(false);
    h.settle_diff_jobs();
    h
}

/// Replace the first `from` in /repo/a.txt with `to`, and let the changes
/// view refold over the diff map that follows.
fn edit(h: &mut TestHarness, from: &str, to: &str) {
    let buffer_id = h
        .stoat
        .active_workspace()
        .buffers
        .id_for_path(Path::new("/repo/a.txt"))
        .expect("an open buffer");
    let start = h
        .buffer_text(buffer_id)
        .find(from)
        .expect("the replaced text");
    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("a buffer")
        .write()
        .expect("buffer poisoned")
        .edit(start..start + from.len(), to);
    h.settle_diff_jobs();
}

/// The folds of the focused changes view's first excerpt.
fn cut(h: &mut TestHarness) -> Cut {
    let ws = h.stoat.active_workspace_mut();
    let excerpt = &ws.focused_diff_changes().expect("a changes view").excerpts[0];
    let map = excerpt.folded_version;
    let editor = ws.editors.get_mut(excerpt.editor_id).expect("an editor");
    let snapshot = editor.display_map.snapshot();
    let folds = snapshot.fold_snapshot();
    let inlays = folds.inlay_snapshot();
    // A fold from a row's start hides that row under its fold row, and a
    // fold from a row's end hides the rows below it.
    let hidden = |range: &Range<InlayPoint>| {
        let start = inlays.to_buffer_point(range.start);
        let first = match start.column {
            0 => start.row,
            _ => start.row + 1,
        };
        first..inlays.to_buffer_point(range.end).row + 1
    };
    Cut {
        map,
        version: folds.version(),
        folds: folds
            .folds_in_range(InlayPoint::default()..folds.to_inlay_point(folds.max_point()))
            .into_iter()
            .map(|fold| (fold.id.0, hidden(&fold.range)))
            .collect(),
    }
}

/// The rows each fold of `after` hides, with whether `before` held it.
fn survivors(before: &Cut, after: &Cut) -> Vec<(bool, Range<u32>)> {
    after
        .folds
        .iter()
        .map(|(id, rows)| {
            (
                before.folds.iter().any(|(held, _)| held == id),
                rows.clone(),
            )
        })
        .collect()
}

/// A landing whose map shows the same rows leaves every fold, so the
/// display map does no work for it.
#[test]
fn a_landing_that_keeps_the_shown_rows_toggles_no_fold() {
    let mut h = three_hunk_stack();
    let before = cut(&mut h);
    edit(&mut h, "ELEVEN", "ELEVEN!");
    let after = cut(&mut h);
    assert_eq!(
        (after.map != before.map, after.version, after.folds),
        (true, before.version, before.folds),
    );
}

/// A hunk that lands in a gap refolds that gap, and the other folds stay.
#[test]
fn a_hunk_landing_in_a_gap_refolds_that_gap_only() {
    let mut h = three_hunk_stack();
    let before = cut(&mut h);
    // Row 36 lies three rows below the run around row 30, so the run grows
    // to row 40 and the gap below it shrinks.
    edit(&mut h, "\n37\n", "\nTHIRTY-SEVEN\n");
    assert_eq!(
        survivors(&before, &cut(&mut h)),
        [
            (true, 0..7),
            (true, 14..27),
            (false, 40..47),
            (true, 54..61)
        ],
    );
}

/// A run whose only hunk goes leaves one fold over both gaps around it,
/// and the other folds stay.
#[test]
fn a_run_whose_hunk_goes_joins_the_gaps_around_it() {
    let mut h = three_hunk_stack();
    let before = cut(&mut h);
    edit(&mut h, "THIRTY-ONE", "31");
    assert_eq!(
        survivors(&before, &cut(&mut h)),
        [(true, 0..7), (false, 14..47), (true, 54..61)],
    );
}

/// The focused excerpt's editor.
fn focused_editor(h: &mut TestHarness) -> &mut EditorState {
    let ws = h.stoat.active_workspace_mut();
    let editor_id = ws
        .focused_diff_changes()
        .and_then(|state| state.focused_excerpt())
        .expect("a focused excerpt")
        .editor_id;
    ws.editors.get_mut(editor_id).expect("an editor")
}

/// A normal-mode edit that takes the change under the cursor folds that
/// change's rows, and the cursor lands on the next change below.
#[test]
fn a_revert_under_the_cursor_lands_on_the_next_change() {
    let mut h = three_hunk_stack();
    let before = cut(&mut h);
    place_cursor(focused_editor(&mut h), Point::new(30, 0));

    edit(&mut h, "THIRTY-ONE", "31");
    assert_eq!(
        (focus(&mut h), survivors(&before, &cut(&mut h))),
        (
            at("a.txt", 50),
            vec![(true, 0..7), (false, 14..47), (true, 54..61)]
        ),
    );
}

#[test]
fn a_revert_of_the_last_change_lands_on_the_change_above() {
    let mut h = three_hunk_stack();
    place_cursor(focused_editor(&mut h), Point::new(50, 0));

    edit(&mut h, "FIFTY-ONE", "51");
    assert_eq!(focus(&mut h), at("a.txt", 30));
}

/// Insert mode keeps the row that its edit hid open, so the keys typed
/// next land where the cursor stands.
#[test]
fn an_insert_mode_revert_keeps_its_row_open() {
    let mut h = three_hunk_stack();
    focused_editor(&mut h).mode = "insert".into();
    place_cursor(focused_editor(&mut h), Point::new(30, 0));

    edit(&mut h, "THIRTY-ONE", "31");
    let open = cut(&mut h)
        .folds
        .iter()
        .all(|(_, rows)| !rows.contains(&30));
    assert_eq!((focus(&mut h), open), (at("a.txt", 30), true));
}

/// A row added inside a hunk moves the folds below it a row down, and every
/// fold stays.
#[test]
fn a_row_added_inside_a_hunk_keeps_every_fold() {
    let mut h = three_hunk_stack();
    let before = cut(&mut h);
    edit(&mut h, "ELEVEN", "ELEVEN\nMORE");
    assert_eq!(
        survivors(&before, &cut(&mut h)),
        [(true, 0..7), (true, 15..28), (true, 35..48), (true, 55..62)],
    );
}

/// An edit that takes the focused file's last hunk drops its excerpt, and
/// the cursor lands on the first change of the next excerpt.
#[test]
fn a_revert_of_a_files_last_hunk_drops_its_excerpt_and_lands_on_the_next_change() {
    let mut h = TestHarness::with_size(100, 24);
    stack_on_an_unsaved_edit(&mut h, &[("a.txt", "a\n", "A\n")]);

    h.edit_focused(0..4, "");
    h.settle_diff_jobs();
    assert_eq!(
        (excerpts(&h), focus(&mut h)),
        (listed(&["a.txt"]), at("a.txt", 0)),
    );
}

/// Insert mode keeps the excerpt that its edit took off the list, since a
/// drop sends the keys typed after it to another file. The excerpt goes
/// once the mode ends.
#[test]
fn an_insert_keeps_a_hunkless_excerpt_until_the_mode_ends() {
    let mut h = TestHarness::with_size(100, 24);
    stack_on_an_unsaved_edit(&mut h, &[("a.txt", "a\n", "A\n")]);

    h.type_keys("i");
    h.edit_focused(0..4, "");
    h.settle_diff_jobs();
    let inserting = (excerpts(&h), focus(&mut h));

    h.type_keys("Escape");
    h.settle();
    assert_eq!(
        (inserting, excerpts(&h), focus(&mut h)),
        (
            (listed(&["a.txt", "d.txt"]), at("d.txt", 0)),
            listed(&["a.txt"]),
            at("a.txt", 0),
        ),
    );
}

#[test]
fn a_dropped_excerpt_lands_on_the_file_below_it() {
    let mut h = TestHarness::with_size(100, 24);
    stack_on_an_unsaved_edit(
        &mut h,
        &[
            ("a.txt", "a\n", "A\n"),
            ("z.txt", "1\n2\n3\n", "1\n2\nTHREE\n"),
        ],
    );

    h.edit_focused(0..4, "");
    h.settle_diff_jobs();
    assert_eq!(
        (excerpts(&h), focus(&mut h)),
        (listed(&["a.txt", "z.txt"]), at("z.txt", 2)),
    );
}

#[test]
fn a_dropped_last_excerpt_leaves_an_empty_stack() {
    let mut h = TestHarness::with_size(100, 24);
    stack_on_an_unsaved_edit(&mut h, &[]);

    h.edit_focused(0..4, "");
    h.settle_diff_jobs();
    let state = h
        .stoat
        .active_workspace()
        .focused_diff_changes()
        .expect("a changes view");
    assert_eq!(
        (excerpts(&h), state.focused_excerpt().is_none()),
        (Vec::new(), true),
    );
}

/// A changes view over the `changed` files and the clean d.txt, which an
/// unsaved edit puts in the list, with the cursor on d.txt's edit.
///
/// Each of `changed` is a path with its text at HEAD and on disk. The tally
/// reads the disk, so the list holds d.txt through its buffer's diff map
/// alone, and an edit that takes the map's last hunk takes the file off the
/// list.
fn stack_on_an_unsaved_edit(h: &mut TestHarness, changed: &[(&str, &str, &str)]) {
    h.stoat.set_diff_warm_auto(true);
    h.stage_review_scenario("/repo", changed);
    repo(h).head_file("d.txt", "same\n");
    h.fake_fs().insert_file(Path::new("/repo/d.txt"), b"same\n");
    h.open_file(Path::new("/repo/d.txt"));
    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
    prepend(h, buffer_id, "new ");
    h.settle_diff_jobs();
    dispatch(&mut h.stoat, &DiffChanges);
    h.settle();
}

/// Remove d.txt's change with real keys, so the edit is an undo step, and
/// land the drop. Returns d.txt's text after the edit.
///
/// The cursor starts past the prepended text, so `0` moves it to the line
/// start before the select.
fn drop_d_by_keys(h: &mut TestHarness) -> String {
    h.type_keys("0 v l l l d");
    h.settle_diff_jobs();
    h.buffer_text_at(Path::new("/repo/d.txt"))
}

/// The undo acts on the dropped file though the focus moved to another
/// excerpt, and the change it restores brings the excerpt back with the
/// cursor on it.
#[test]
fn an_undo_after_a_drop_restores_the_excerpt_under_the_cursor() {
    let mut h = TestHarness::with_size(100, 24);
    stack_on_an_unsaved_edit(&mut h, &[("a.txt", "a\n", "A\n")]);
    let edited = drop_d_by_keys(&mut h);
    let dropped = excerpts(&h);
    let editor_id = dropped_editor(&h);

    h.type_keys("u");
    h.settle_diff_jobs();
    let restored = h
        .stoat
        .active_workspace()
        .focused_diff_changes()
        .and_then(|state| state.focused_excerpt())
        .is_some_and(|excerpt| excerpt.editor_id == editor_id);
    assert_eq!(
        (
            edited,
            dropped,
            h.buffer_text_at(Path::new("/repo/d.txt")),
            excerpts(&h),
            h.buffer_text_at(Path::new("/repo/a.txt")),
            focus(&mut h),
            restored,
        ),
        (
            "same\n".to_string(),
            listed(&["a.txt"]),
            "new same\n".to_string(),
            listed(&["a.txt", "d.txt"]),
            "A\n".to_string(),
            at("d.txt", 0),
            true,
        ),
    );
}

/// The editor of the focused changes view's dropped excerpt.
fn dropped_editor(h: &TestHarness) -> EditorId {
    h.stoat
        .active_workspace()
        .focused_diff_changes()
        .and_then(|state| state.dropped.as_ref())
        .expect("a dropped excerpt")
        .excerpt
        .editor_id
}

#[test]
fn a_redo_after_that_undo_drops_the_excerpt_again() {
    let mut h = TestHarness::with_size(100, 24);
    stack_on_an_unsaved_edit(&mut h, &[("a.txt", "a\n", "A\n")]);
    let edited = drop_d_by_keys(&mut h);
    h.type_keys("u");
    h.settle_diff_jobs();

    h.type_keys("U");
    h.settle_diff_jobs();
    assert_eq!(
        (
            edited,
            excerpts(&h),
            h.buffer_text_at(Path::new("/repo/d.txt")),
        ),
        (
            "same\n".to_string(),
            listed(&["a.txt"]),
            "same\n".to_string(),
        ),
    );
}

/// An edit in another file of the stack ends the dropped excerpt's turn at
/// once, so the undo after it acts on the focused excerpt. The two dispatch
/// with no pump between them, as keys read in one batch do.
#[test]
fn an_edit_elsewhere_ends_the_history_target() {
    let mut h = TestHarness::with_size(100, 24);
    stack_on_an_unsaved_edit(&mut h, &[("a.txt", "a\n", "A\n")]);
    let edited = drop_d_by_keys(&mut h);

    dispatch(&mut h.stoat, &DeleteSelection);
    dispatch(&mut h.stoat, &Undo);
    h.settle();
    let dropped = h
        .stoat
        .active_workspace()
        .focused_diff_changes()
        .expect("a changes view")
        .dropped
        .is_some();
    assert_eq!(
        (
            edited,
            h.buffer_text_at(Path::new("/repo/a.txt")),
            h.buffer_text_at(Path::new("/repo/d.txt")),
            dropped,
        ),
        (
            "same\n".to_string(),
            "A\n".to_string(),
            "same\n".to_string(),
            false,
        ),
    );
}

#[test]
fn a_closed_view_releases_the_dropped_excerpt() {
    let mut h = TestHarness::with_size(100, 24);
    stack_on_an_unsaved_edit(&mut h, &[("a.txt", "a\n", "A\n")]);
    let edited = drop_d_by_keys(&mut h);
    let editor_id = dropped_editor(&h);

    dispatch(&mut h.stoat, &CloseDiffChanges);
    h.settle();
    assert_eq!(
        (
            edited,
            h.stoat.active_workspace().editors.contains_key(editor_id),
        ),
        ("same\n".to_string(), false),
    );
}

/// An unsaved edit that takes a file's last change takes the file off the
/// stack though the disk still differs, and the cursor lands on the next
/// change.
#[test]
fn an_unsaved_revert_of_a_listed_file_drops_its_excerpt() {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
    h.open_file(Path::new("/repo/a.txt"));
    dispatch(&mut h.stoat, &DiffChanges);
    h.settle();

    h.edit_focused(0..1, "a");
    h.settle_diff_jobs();
    assert_eq!(
        (excerpts(&h), focus(&mut h)),
        (listed(&["b.txt"]), at("b.txt", 0)),
    );
}

/// A file the tally names for a move stays listed through an unsaved edit
/// that takes its buffer back to the base, since its place needs no hunk.
///
/// The changes view's pump drives the tally, so the file stacks first.
#[test]
fn an_unsaved_edit_of_a_moved_file_keeps_it_listed() {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    h.stage_rename_scenario("/repo", "old.txt", "moved.txt", "same\n", "same\nmore\n");
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Head);
    h.open_file(Path::new("/repo/moved.txt"));
    stacked(&mut h);

    h.edit_focused(5..10, "");
    h.settle_diff_jobs();
    assert_eq!(
        (
            h.buffer_text_at(Path::new("/repo/moved.txt")),
            listed_rel_paths(&h)
        ),
        ("same\n".to_string(), vec!["moved.txt".to_string()]),
    );
}

/// The focused excerpt's file and its cursor's row.
fn focus(h: &mut TestHarness) -> (String, u32) {
    let ws = h.stoat.active_workspace_mut();
    let id = ws.focused_diff_changes_id().expect("a changes view");
    let excerpt = ws.diff_changes_views[id]
        .focused_excerpt()
        .expect("a focused excerpt");
    let (rel_path, editor_id) = (excerpt.rel_path.clone(), excerpt.editor_id);
    let editor = ws.editors.get_mut(editor_id).expect("an editor");
    (rel_path, cursor_point(editor).row)
}

fn at(rel_path: &str, row: u32) -> (String, u32) {
    (rel_path.to_string(), row)
}

/// A stage and a `.git` event move the base where no diff map moved yet,
/// so each moves the stamp, and the list follows.
#[test]
fn a_stage_and_a_git_event_each_move_the_stamp() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    h.open_file(Path::new("/repo/a.txt"));
    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
    let stamp = |h: &TestHarness| StackStamp::read(h.stoat.active_workspace());

    let first = stamp(&h);
    h.stoat
        .active_workspace_mut()
        .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
    let staged = stamp(&h);
    h.stoat.active_workspace_mut().invalidate_all_diffs();
    assert_eq!((first != staged, staged != stamp(&h)), (true, true));
}

/// A clean buffer's content is the disk's, so after a commit cleans its
/// file, the map a hidden buffer keeps from before the commit lists nothing.
#[test]
fn a_clean_buffer_with_a_stale_map_lists_from_the_tally_alone() {
    let h = commit_behind_a_hidden_buffer(|_| {});
    assert_eq!(listed_rel_paths(&h), ["b.txt"]);
}

/// The tally reads the disk, which an unsaved edit has not reached, so the
/// edited buffer stays listed from its map whatever the map's age.
#[test]
fn an_unsaved_buffer_with_a_stale_map_stays_listed() {
    let h = commit_behind_a_hidden_buffer(|h| h.type_keys("i x escape"));
    assert_eq!(listed_rel_paths(&h), ["a.txt", "b.txt"]);
}

/// A repo where `a.txt` and `b.txt` differ from HEAD, and a commit of
/// `a.txt` lands while `b.txt` hides it, with the diff jobs and the tally
/// settled after it.
///
/// The hidden `a.txt` keeps the map from before the commit. `before_hide`
/// runs on `a.txt` while it has the focus.
fn commit_behind_a_hidden_buffer(before_hide: impl FnOnce(&mut TestHarness)) -> TestHarness {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
    h.stoat.sync_repo_root();
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();
    before_hide(&mut h);
    h.open_file(Path::new("/repo/b.txt"));

    h.fake_git()
        .add_repo("/repo")
        .clear_changes()
        .head_file("a.txt", "A\n")
        .modified("b.txt", "b\n", "B\n");
    h.stoat.active_workspace_mut().invalidate_all_diffs();
    h.settle_diff_jobs();
    h
}

/// The files the changes view lists, by their path in the repository.
fn listed_rel_paths(h: &TestHarness) -> Vec<String> {
    listed_files(&h.stoat).into_keys().collect()
}

/// An open and a close move the stamp's path part, and a diff landing its
/// map part, since the list reads the open buffers' diff maps.
#[test]
fn an_open_a_landing_and_a_close_move_their_stamp_parts() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
    let parts = |h: &TestHarness| {
        let stamp = StackStamp::read(h.stoat.active_workspace());
        (stamp.maps, stamp.paths)
    };
    let mut seen = vec![parts(&h)];

    h.open_file(Path::new("/repo/a.txt"));
    seen.push(parts(&h));
    let (_, buffer_id) = h.stoat.focused_editor_ids().expect("editor");
    h.settle_diff_map(buffer_id);
    seen.push(parts(&h));
    h.stoat.active_workspace_mut().buffers.remove(buffer_id);
    seen.push(parts(&h));
    let moved: Vec<(bool, bool)> = seen
        .windows(2)
        .map(|pair| (pair[0].0 != pair[1].0, pair[0].1 != pair[1].1))
        .collect();
    assert_eq!(moved, [(false, true), (true, false), (false, true)]);
}

/// Under a revision base the stack lists what changed since the revision,
/// a file committed since and clean in the tree included.
#[test]
fn a_revision_base_lists_what_changed_since_the_revision() {
    let mut h = TestHarness::with_size(100, 24);
    repo(&mut h)
        .head_file("committed.txt", "after\n")
        .commit("base0", &[("committed.txt", "before\n")]);
    h.fake_fs()
        .insert_file(Path::new("/repo/committed.txt"), b"after\n");
    h.stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::named("base0", "base0".to_string())));

    stacked(&mut h);
    assert_eq!(excerpts(&h), listed(&["committed.txt"]));
}

/// A close drops the buffers only the stack opened, and keeps the one a
/// pane opened before.
#[test]
fn a_close_drops_the_buffers_only_the_stack_opened() {
    let mut h = TestHarness::with_size(100, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
    h.open_file(Path::new("/repo/a.txt"));
    stacked(&mut h);
    let open = |h: &TestHarness, path: &str| {
        h.stoat
            .active_workspace()
            .buffers
            .id_for_path(Path::new(path))
            .is_some()
    };
    let before = (open(&h, "/repo/a.txt"), open(&h, "/repo/b.txt"));

    dispatch(&mut h.stoat, &CloseDiffChanges);
    assert_eq!(
        (before, open(&h, "/repo/a.txt"), open(&h, "/repo/b.txt")),
        ((true, true), true, false),
    );
}

/// Lines `line 0` to `line 38`, and with `edit` the lines from 4 on edited
/// in every other block of five, so a stack shows each line but the first.
/// `appended` new lines follow them.
fn blocks(edit: bool, appended: usize) -> String {
    let line = |n: usize| match edit && n >= 4 && ((n - 4) / 5).is_multiple_of(2) {
        true => format!("line {n} edit\n"),
        false => format!("line {n}\n"),
    };
    let new: String = (0..appended).map(|n| format!("new {n}\n")).collect();
    (0..39).map(line).collect::<String>() + &new
}

/// A painted changes view over `a.txt` and `b.txt`, two [`blocks`] files,
/// with its view inside `b.txt`'s excerpt `offset` rows past its header.
///
/// With `cursor_there`, the cursor sits on screen five rows under the
/// view's top in `b.txt`, else on `a.txt`'s tenth stack row above the
/// view. A paint follows the cursor before the scroll moves, so the scroll
/// stays where it is set.
fn seated_stack(cursor_there: bool, offset: usize) -> TestHarness {
    let mut h = TestHarness::with_size(120, 24);
    let (base, work) = (blocks(false, 0), blocks(true, 0));
    h.stage_review_scenario("/repo", &[("a.txt", &base, &work), ("b.txt", &base, &work)]);
    stacked(&mut h);
    let _ = h.stoat.render();
    with_stack(&mut h, |state, editors| {
        let row = match cursor_there {
            true => state.excerpts[1].top + offset + 5,
            false => state.excerpts[0].top + 10,
        };
        select_row(state, editors, row);
    });
    let _ = h.stoat.render();
    with_stack(&mut h, |state, _| {
        state.scroll = state.excerpts[1].top + offset;
    });
    let _ = h.stoat.render();
    h
}

/// The edited [`blocks`] text with three new lines after line `after`.
fn with_mid_lines(after: usize) -> String {
    let work = blocks(true, 0);
    let lines: Vec<&str> = work.split_inclusive('\n').collect();
    lines[..=after].concat() + "mid 0\nmid 1\nmid 2\n" + &lines[after + 1..].concat()
}

/// Write `text` to `/repo/<file>`, reload every buffer, settle, and return
/// the first paint.
fn reload_file(h: &mut TestHarness, file: &str, text: &str) -> Buffer {
    h.fake_fs()
        .insert_file(Path::new(&format!("/repo/{file}")), text.as_bytes());
    auto_reload::reload_all(&mut h.stoat, true);
    h.settle();
    h.stoat.render()
}

/// Run `f` over the focused changes view and the workspace's editors.
fn with_stack<T>(
    h: &mut TestHarness,
    f: impl FnOnce(&mut DiffChangesState, &mut SlotMap<EditorId, EditorState>) -> T,
) -> T {
    let ws = h.stoat.active_workspace_mut();
    let id = ws
        .focused_diff_changes_id()
        .expect("a focused changes view");
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = ws;
    f(&mut diff_changes_views[id], editors)
}

/// The text on each row of the focused stack's view, a header row as its
/// file's path.
fn stack_screen_text(h: &mut TestHarness) -> Vec<String> {
    with_stack(h, |state, editors| {
        layout_stack(state, editors);
        (state.scroll..state.scroll + state.viewport_rows)
            .map(|row| {
                let Some(excerpt) = state.excerpts.iter().rev().find(|e| e.top <= row) else {
                    return String::new();
                };
                match row.checked_sub(excerpt.top + header_rows(excerpt)) {
                    None => excerpt.rel_path.clone(),
                    Some(display_row) => editors
                        .get_mut(excerpt.editor_id)
                        .map(|editor| {
                            editor
                                .display_map
                                .snapshot()
                                .display_line(display_row as u32)
                        })
                        .unwrap_or_default(),
                }
            })
            .collect()
    })
}

/// The focused stack's scroll and whether a reload holds it.
fn stack_view(h: &mut TestHarness) -> (usize, bool) {
    with_stack(h, |state, _| (state.scroll, state.view_hold.is_some()))
}

/// Append twenty lines to `a.txt` on disk, reload every buffer, settle,
/// and paint.
fn grow_first_file(h: &mut TestHarness) {
    reload_file(h, "a.txt", &blocks(true, 20));
}

/// Twenty lines that arrive at the end of the first file leave the second
/// file's text on its screen rows. The cursor there keeps its screen row,
/// so the stack moves down twenty rows.
#[test]
fn a_reload_holds_the_stack_when_lines_arrive_above_the_view() {
    let mut h = seated_stack(true, 5);
    let (before, (scroll, _)) = (stack_screen_text(&mut h), stack_view(&mut h));

    grow_first_file(&mut h);

    assert_eq!(
        (stack_screen_text(&mut h), stack_view(&mut h)),
        (before, (scroll + 20, true)),
    );
}

/// A stack whose cursor sits above the view holds the first text row under
/// its pinned header, so the text stays put though the cursor does not
/// move. A view that starts on the file header holds the text row under
/// the header, and the header stays on the view's top row.
#[test]
fn a_reload_holds_the_stack_with_the_cursor_off_screen() {
    for offset in [5, 0] {
        let mut h = seated_stack(false, offset);
        let (before, (scroll, _)) = (stack_screen_text(&mut h), stack_view(&mut h));

        grow_first_file(&mut h);

        assert_eq!(
            (stack_screen_text(&mut h), stack_view(&mut h).0),
            (before, scroll + 20),
            "with the view {offset} rows past the second header",
        );
    }
}

/// The cursor on screen keeps its screen row though three lines arrive in
/// its own file between the view's top and the cursor.
#[test]
fn a_reload_keeps_the_stack_cursor_on_its_screen_row() {
    let mut h = seated_stack(true, 5);
    let cursor_screen_row = |h: &mut TestHarness| {
        with_stack(h, |state, editors| {
            layout_stack(state, editors);
            cursor_stack_row(state, editors).map(|row| row - state.scroll)
        })
    };
    let before = cursor_screen_row(&mut h);

    reload_file(&mut h, "b.txt", &with_mid_lines(7));

    assert_eq!((before, cursor_screen_row(&mut h)), (Some(5), Some(5)));
}

/// Lines that arrive in an off-screen cursor's own file above it move the
/// cursor, and the stack holds its text with no follow to the cursor.
#[test]
fn a_reload_that_moves_an_off_screen_cursor_holds_the_stack() {
    let mut h = seated_stack(false, 5);
    let before = stack_screen_text(&mut h);

    reload_file(&mut h, "a.txt", &with_mid_lines(2));

    assert_eq!(stack_screen_text(&mut h), before);
}

/// An auto-reload poll holds the stack as a reload does.
#[test]
fn a_poll_reload_holds_the_stack() {
    let mut h = seated_stack(false, 5);
    let before = stack_screen_text(&mut h);
    let id = h
        .stoat
        .active_workspace()
        .buffers
        .id_for_path(Path::new("/repo/a.txt"))
        .expect("a.txt is open");
    h.stoat
        .active_workspace_mut()
        .buffers
        .set_auto_reload(id, AutoReloadMode::Tail);
    auto_reload::ensure_auto_reload_poll(&mut h.stoat);

    h.fake_fs()
        .insert_file(Path::new("/repo/a.txt"), blocks(true, 20).as_bytes());
    auto_reload::pump_auto_reload(&mut h.stoat);
    h.run_until_parked();
    auto_reload::pump_auto_reload_install(&mut h.stoat);
    h.settle();
    let _ = h.stoat.render();

    assert_eq!(stack_screen_text(&mut h), before);
}

/// A deliberate move after the hold's first paint releases the stack.
/// Another writer's scroll keeps its row, and a held screen row puts the
/// cursor on screen row 3.
#[test]
fn a_deliberate_move_releases_the_stack() {
    let after = |deliberate: fn(&mut DiffChangesState)| {
        let mut h = seated_stack(true, 5);
        grow_first_file(&mut h);
        with_stack(&mut h, |state, _| deliberate(state));
        let _ = h.stoat.render();
        with_stack(&mut h, |state, editors| {
            let cursor = cursor_stack_row(state, editors).map(|row| row - state.scroll);
            (state.scroll, cursor, state.view_hold.is_some())
        })
    };
    let (scrolled, _, scrolled_held) = after(|state| state.scroll = 10);
    let (_, cursor, held_row_held) = after(|state| state.held_screen_row = Some(3));

    assert_eq!(
        ((scrolled, scrolled_held), (cursor, held_row_held)),
        ((10, false), (Some(3), false)),
    );
}

/// A diff map that lands after the reload folds most of the first file
/// away, and the second file's text stays on its screen rows.
#[test]
fn a_refold_after_a_reload_keeps_the_stack_rows() {
    let mut h = seated_stack(true, 5);
    grow_first_file(&mut h);
    let before = stack_screen_text(&mut h);
    let id = h
        .stoat
        .active_workspace()
        .buffers
        .id_for_path(Path::new("/repo/a.txt"))
        .expect("a.txt is open");

    h.stoat
        .active_workspace_mut()
        .install_test_diff_map(id, map_of(&blocks(true, 0), &blocks(true, 20)));
    pump_diff_changes_stack(&mut h.stoat);
    let _ = h.stoat.render();

    assert_eq!(stack_screen_text(&mut h), before);
}

/// The reader's next key releases the stack, so a reload never fights a
/// move the reader makes.
#[test]
fn a_key_after_a_reload_releases_the_stack() {
    let mut h = seated_stack(true, 5);
    grow_first_file(&mut h);
    let held = stack_view(&mut h).1;

    h.type_keys("j");

    assert_eq!((held, stack_view(&mut h).1), (true, false));
}

/// The first paint after a reload shows the stack as a steady paint does,
/// since the stack alone places the rows of its excerpts. Here the second
/// file's header sits four rows down the view, and three lines arrive in
/// that file between its header and its cursor.
#[test]
fn the_first_paint_after_a_reload_matches_a_steady_paint() {
    let mut h = seated_stack(true, 0);
    with_stack(&mut h, |state, _| state.scroll -= 4);
    let _ = h.stoat.render();

    let first = reload_file(&mut h, "b.txt", &with_mid_lines(1));
    h.stoat.pane_cache.clear();

    assert_eq!(first, h.stoat.render());
}

/// The paint that applies a reload's hold moves the stack, so it caches no
/// pane under the scroll it leaves. A move back to that scroll paints the
/// rows there, as a fresh paint does.
///
/// A steady paint before the reload lets the reload's paint capture, since
/// a capture skips the second of two frames whose keys moved. The first
/// file grows by five lines, so it stays out of the view at that scroll
/// and no reload flash there denies the paint a key.
#[test]
fn a_move_back_after_a_reload_paints_the_rows_there() {
    let mut h = seated_stack(true, 10);
    let scroll = stack_view(&mut h).0;
    let _ = h.stoat.render();
    reload_file(&mut h, "a.txt", &blocks(true, 5));

    with_stack(&mut h, |state, _| state.scroll = scroll);
    let moved_back = h.stoat.render();
    h.stoat.pane_cache.clear();

    assert_eq!(moved_back, h.stoat.render());
}
