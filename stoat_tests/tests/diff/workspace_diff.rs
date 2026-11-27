use futures::{future, FutureExt};
use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    action_handlers,
    action_handlers::movement,
    buffer::BufferId,
    diff_map::{
        DiffHunk, DiffHunkStatus, DiffMap, DiffStatus, GutterMark, MarkState, ProseChange,
        StagedMark,
    },
    pane::View,
    test_fixture::workspace_diff::span_prose,
    test_harness::TestHarness,
    workspace::{
        diff::{
            diff_sides, repo_hunk_position, resolve_base, scan_changed_ranges, DiffBase, DiffJob,
            DiffLanding, DiffState, HunkPosition, WorktreeBase, DIFF_SETTLE,
        },
        Workspace,
    },
};
use stoat_action::{DiffAgainstIndex, ToggleStageHunk};

/// The bar reads the totals and a file list reads the per-file counts, so
/// both come off one stored tally rather than two walks that could disagree.
#[test]
fn the_stored_tally_answers_both_the_totals_and_the_per_file_counts() {
    let mut h = TestHarness::with_size(40, 12);
    let workdir = PathBuf::from("/tally");
    h.stage_review_scenario(
        &workdir,
        &[("a.rs", "one\n", "ONE\n"), ("b.rs", "two\n", "TWO\n")],
    );
    h.fake_git().add_repo(&workdir).hunks("a.rs", 3);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.rs"));
    h.settle_diff_jobs();

    let ws = h.stoat.active_workspace();
    assert_eq!(
        ws.repo_status()
            .map(|status| (status.staged, status.unstaged)),
        Some((0, 4)),
        "the totals are the hunks each side carries",
    );
    assert_eq!(
        ws.repo_hunk_totals(),
        Some([(PathBuf::from("a.rs"), 3), (PathBuf::from("b.rs"), 1)].as_slice()),
        "and the per-file counts come off the same tally",
    );
}

/// An untracked file has no HEAD blob, so its base resolves through a
/// rename lookup, which the local backend answers with a working-tree stat
/// walk and two rename passes. Only a git write turns a path into a rename
/// target, so that answer holds until one does.
#[test]
fn an_untracked_buffer_asks_for_its_base_once() {
    let mut h = TestHarness::with_size(40, 12);
    let workdir = PathBuf::from("/untracked");
    h.stage_review_scenario(&workdir, &[("tracked.rs", "one\n", "ONE\n")]);
    // Written straight into the fake fs, so it exists on disk with no
    // registration in the repo, which is what untracked means.
    let fresh = workdir.join("fresh.rs");
    h.fake_fs().insert_file(&fresh, b"new\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&fresh);
    h.settle_diff_jobs();

    let after_open = h.fake_git().rename_source_calls(&workdir);
    assert!(after_open > 0, "the first settle asks once");

    h.type_keys("i x <esc>");
    h.settle_diff_jobs();

    assert_eq!(
        h.fake_git().rename_source_calls(&workdir),
        after_open,
        "and the recorded base answers every settle after it",
    );
}

/// A range review reads its merge base on both sides, so no hunk of the
/// range reads staged. A commit review keeps HEAD on the staged side.
#[test]
fn a_range_review_reads_its_base_on_both_sides() {
    let h = TestHarness::with_size(40, 12);
    h.fake_git()
        .add_repo("/repo")
        .commit("b1", &[("a.rs", "base\n")])
        .head_file("a.rs", "tip\n");
    let texts = |base: DiffBase| {
        let texts = resolve_base(
            &**h.fake_git(),
            Path::new("/repo"),
            Path::new("/repo/a.rs"),
            Some(&base),
        )
        .expect("a file in the repo");
        (texts.base.to_string(), texts.staged_text.to_string())
    };
    assert_eq!(
        [
            texts(DiffBase::review("b1".to_string(), "feature", "main")),
            texts(DiffBase::parent_of("t1", Some("b1".to_string()))),
        ],
        [
            ("base\n".to_string(), "base\n".to_string()),
            ("base\n".to_string(), "tip\n".to_string()),
        ],
    );
}

#[test]
fn a_pair_base_answers_for_its_file_alone() {
    let mut h = TestHarness::with_size(40, 12);
    h.stage_review_scenario("/repo", &[("t.txt", "old\n", "new\n")]);
    let pair = DiffBase::Pair {
        path: "/pair/b.txt".into(),
        base_path: "/pair/a.txt".into(),
        text: Arc::new("one\n".to_string()),
    };
    let resolved = |root: &str, path: &str| {
        resolve_base(
            &**h.fake_git(),
            Path::new(root),
            Path::new(path),
            Some(&pair),
        )
        .map(|texts| texts.base.to_string())
    };
    assert_eq!(
        [
            resolved("/pair", "/pair/b.txt"),
            resolved("/pair", "/pair/a.txt"),
            resolved("/repo", "/repo/t.txt"),
        ],
        [Some("one\n".to_string()), None, Some("old\n".to_string())],
        "the pair's file reads the other file with no repository, and every other path \
         keeps its own base",
    );
}

/// A repo-wide tally is a walk of every diff in the repository, and an
/// unsaved edit has not touched the disk it reads, so a settle must not buy
/// one. A landed save has touched it, so that one must.
#[test]
fn only_a_disk_write_buys_another_repo_tally() {
    let mut h = TestHarness::with_size(40, 12);
    let workdir = PathBuf::from("/tally");
    h.stage_review_scenario(&workdir, &[("a.rs", "one\n", "ONE\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.rs"));
    h.settle_diff_jobs();

    let after_open = h.fake_git().tally_calls(&workdir);
    assert!(after_open > 0, "the first open reads one");

    h.type_keys("i x <esc>");
    h.settle_diff_jobs();
    assert_eq!(
        h.fake_git().tally_calls(&workdir),
        after_open,
        "an unsaved edit cannot move the tally, so it buys no walk",
    );

    action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveBuffer);
    h.settle_diff_jobs();
    assert_eq!(
        h.fake_git().tally_calls(&workdir),
        after_open + 1,
        "and a landed save buys exactly one",
    );
}

/// The reader walks the focused file's own map, so the position counts
/// through it while the stored tally answers for the files ahead of it.
/// The two disagree for a beat after an edit, and the map is the half that
/// matches what is on screen.
#[test]
fn the_hunk_position_counts_the_focused_file_through_its_own_map() {
    let mut h = TestHarness::with_size(40, 16);
    let workdir = PathBuf::from("/position");
    let working: String = (0..12).map(|n| format!("line {n}\n")).collect();
    let head = working.replace("line 2\n", "old 2\n");
    h.stage_review_scenario(
        &workdir,
        &[("a.rs", "one\n", "ONE\n"), ("b.rs", &head, &working)],
    );
    h.fake_git().add_repo(&workdir).hunks("a.rs", 3);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("b.rs"));
    h.settle_diff_jobs();

    let editor_id = {
        let ws = h.stoat.active_workspace();
        let View::Editor(id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the opened file focuses an editor pane");
        };
        id
    };
    let buffer_id = {
        let ws = h.stoat.active_workspace_mut();
        ws.editors[editor_id].set_diff_view(true);
        ws.editors[editor_id].buffer_id
    };
    let install = |h: &mut TestHarness| {
        let map = DiffMap::from_hunks([added_hunk(2..3), added_hunk(8..9)], None);
        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(buffer_id, map);
    };

    let at = |hunk_at| HunkPosition {
        file_at: 2,
        file_total: 2,
        hunk_at,
        hunk_total: 5,
    };

    install(&mut h);
    assert_eq!(
        repo_hunk_position(h.stoat.active_workspace()),
        Some(at(None)),
        "b.rs is the second of the two changed files, the cursor opens above \
         its first hunk, and the total counts a.rs through the tally and b.rs \
         through its map",
    );

    h.type_keys("j j j j j j j j");
    install(&mut h);
    assert_eq!(
        repo_hunk_position(h.stoat.active_workspace()),
        Some(at(Some(5))),
        "the second of b.rs's own hunks, offset by the three a.rs carries",
    );
}

/// A removal of a file's last lines, in a file with no trailing newline,
/// leaves no row after it. The walk lands on the last row, and the position
/// names that removal rather than the hunk before it.
#[test]
fn a_landing_on_a_trailing_removal_has_a_position() {
    let mut h = TestHarness::with_size(40, 16);
    let workdir = PathBuf::from("/position");
    h.stage_review_scenario(&workdir, &[("b.rs", "a\nb\nc\nd\n", "a\nb\nc")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("b.rs"));
    h.settle_diff_jobs();

    let buffer_id = {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        editor.set_diff_view(true);
        editor.buffer_id
    };
    h.stoat
        .active_workspace_mut()
        .install_test_diff_map(buffer_id, DiffMap::from_hunks([deleted_hunk(3)], None));

    movement::goto_change_impl(
        &mut h.stoat,
        movement::ChangeDir::Next,
        1,
        movement::ChangeUnit::Tree,
    );
    assert_eq!(
        repo_hunk_position(h.stoat.active_workspace()),
        Some(HunkPosition {
            file_at: 1,
            file_total: 1,
            hunk_at: Some(1),
            hunk_total: 1,
        }),
        "the walk lands the removal on row 2, and the position counts it",
    );
}

/// Two edits that only a blank line separates are one change, the one the
/// walk crosses in one press, so the position counts them once.
#[test]
fn two_edits_around_a_blank_line_count_as_one_change() {
    let mut h = TestHarness::with_size(40, 16);
    let workdir = PathBuf::from("/position");
    h.stage_review_scenario(&workdir, &[("b.rs", "a\nb\n\nc\nd\n", "a\nB\n\nC\nd\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("b.rs"));
    h.settle_diff_jobs();

    let buffer_id = {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        editor.set_diff_view(true);
        editor.buffer_id
    };
    h.type_keys("j j j");
    h.stoat.active_workspace_mut().install_test_diff_map(
        buffer_id,
        DiffMap::from_hunks([added_hunk(1..2), added_hunk(3..4)], None),
    );

    assert_eq!(
        repo_hunk_position(h.stoat.active_workspace()),
        Some(HunkPosition {
            file_at: 1,
            file_total: 1,
            hunk_at: Some(1),
            hunk_total: 1,
        }),
        "the cursor on the second edit stands on change one of one",
    );
}

/// Under the index base the position counts the stops the diff view's walk
/// makes, and a file whose changes are all staged offers none.
#[test]
fn the_hunk_position_under_the_index_base_leaves_staged_changes_out() {
    let mut h = TestHarness::with_size(40, 16);
    let workdir = PathBuf::from("/position");
    h.stage_review_scenario(&workdir, &[("b.rs", "one\n", "ONE\n")]);
    h.fake_git().add_repo(&workdir).staged_file("a.rs", "x\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("b.rs"));
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    h.settle_diff_jobs();
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);

    let index = repo_hunk_position(h.stoat.active_workspace());
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Head);
    h.settle_diff_jobs();
    let head = repo_hunk_position(h.stoat.active_workspace());

    let only = |file_at, at_and_total| HunkPosition {
        file_at,
        file_total: file_at,
        hunk_at: Some(at_and_total),
        hunk_total: at_and_total,
    };
    assert_eq!(
        [index, head],
        [Some(only(1, 1)), Some(only(2, 2))],
        "the staged a.rs counts as a changed file and a hunk under HEAD alone",
    );
}

/// The tally lists a pure move with no hunks. Such a file is no stop for
/// the walk, so it neither moves the focused file's place nor joins the
/// count.
#[test]
fn the_file_place_counts_only_files_with_hunks() {
    let mut h = TestHarness::with_size(40, 16);
    let workdir = PathBuf::from("/position");
    h.stage_review_scenario(
        &workdir,
        &[
            ("a.rs", "one\n", "ONE\n"),
            ("a_moved.rs", "two\n", "TWO\n"),
            ("b.rs", "three\n", "THREE\n"),
            ("c.rs", "four\n", "FOUR\n"),
        ],
    );
    h.fake_git().add_repo(&workdir).hunks("a_moved.rs", 0);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("b.rs"));
    h.settle_diff_jobs();
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);

    let position =
        repo_hunk_position(h.stoat.active_workspace()).expect("the diff view has a position");
    assert_eq!(
        (position.file_at, position.file_total),
        (2, 3),
        "a.rs stands before b.rs and c.rs after it",
    );
}

fn added_hunk(line_range: Range<u32>) -> DiffHunk {
    DiffHunk {
        status: DiffHunkStatus::Added,
        unstaged_lines: vec![line_range.clone()],
        marked_rows: Vec::new(),
        relocated_rows: Vec::new(),
        buffer_start_line: line_range.start,
        buffer_line_range: line_range,
        base_byte_range: 0..0,
        anchor_range: None,
        token_detail: None,
    }
}

/// A removal that marks buffer `row`, the row after the removed lines.
fn deleted_hunk(row: u32) -> DiffHunk {
    let rows = row..row;
    DiffHunk {
        status: DiffHunkStatus::Deleted,
        unstaged_lines: vec![rows.clone()],
        marked_rows: Vec::new(),
        relocated_rows: Vec::new(),
        buffer_start_line: row,
        buffer_line_range: rows,
        base_byte_range: 0..0,
        anchor_range: None,
        token_detail: None,
    }
}

/// A file whose code moved to another open file reads as moved until that
/// file's buffer closes, and the next drive re-pairs without it.
#[test]
fn a_closed_buffer_takes_its_moves_out_of_the_other_files() {
    let mut h = TestHarness::with_size(100, 24);
    h.stoat.set_diff_warm_auto(true);
    h.stage_review_scenario(
        "/repo",
        &[
            ("a.rs", "fn moved() { work(1, 2); }\n", ""),
            (
                "b.rs",
                "fn keep() {}\n",
                "fn keep() {}\nfn moved() { work(1, 2); }\n",
            ),
        ],
    );
    h.cover_with_diff_changes(false);
    h.settle();
    h.settle_diff_jobs();
    let statuses = |h: &TestHarness| {
        let ws = h.stoat.active_workspace();
        let id = ws
            .buffers
            .id_for_path(Path::new("/repo/a.rs"))
            .expect("a.rs open");
        let buffer = ws.buffers.get(id).expect("a buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map installed");
        dm.hunks().map(|hunk| hunk.status).collect::<Vec<_>>()
    };
    let paired = statuses(&h);

    let b = h
        .stoat
        .active_workspace()
        .buffers
        .id_for_path(Path::new("/repo/b.rs"))
        .expect("b.rs open");
    h.stoat
        .active_workspace_mut()
        .diff
        .release(b, Some(Path::new("/repo/b.rs")));
    h.stoat.drive_background();
    assert_eq!(
        (paired, statuses(&h)),
        (vec![DiffHunkStatus::Moved], vec![DiffHunkStatus::Deleted]),
        "the removal reads as moved while b.rs shares, and as removed once it closes",
    );
}

#[test]
fn changed_ranges_scan_memoizes_across_unchanged_scans() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario(
        "/repo",
        &[("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n")],
    );

    let git = h.stoat.git_host.clone();
    let fs = h.stoat.fs_host.clone();
    let langs = h.stoat.language_registry.clone();
    let scan_and_install = |ws: &mut Workspace| {
        let scan = scan_changed_ranges(
            git.as_ref(),
            fs.as_ref(),
            &langs,
            &ws.git_root.clone(),
            &ws.changed_ranges_memo_snapshot(),
        );
        ws.install_changed_ranges(scan);
    };

    let ws = h.stoat.active_workspace_mut();

    scan_and_install(ws);
    assert_eq!(
        ws.changed_ranges_recomputes, 1,
        "the first scan diffs the changed file once"
    );
    assert!(
        !ws.changed_ranges.is_empty(),
        "the working-tree change is recorded"
    );

    scan_and_install(ws);
    assert_eq!(
        ws.changed_ranges_recomputes, 1,
        "a second scan over the unchanged tree reuses the memo, no re-diff"
    );
    assert!(
        !ws.changed_ranges.is_empty(),
        "the recorded change survives the memo hit"
    );
}

#[test]
fn diff_job_populates_tracked_buffer_diff_map() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard
        .diff_map
        .as_ref()
        .expect("the tracked buffer's diff map is populated");

    assert_eq!(
        dm.status_for_line(1),
        DiffStatus::Modified,
        "the edited second line reads modified"
    );
    assert_eq!(
        dm.status_for_line(0),
        DiffStatus::Unchanged,
        "the unchanged first line reads unchanged"
    );
}

/// A moved file carries its content across, so it owes no hunks. Its base
/// lives under the path it came from. A read of the new path alone shows
/// the whole file as freshly written.
#[test]
fn a_moved_buffer_diffs_against_the_old_path_and_shows_no_hunks() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_rename_scenario("/repo", "old.txt", "new.txt", "a\nb\n", "a\nb\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/new.txt"));
    h.settle_diff_jobs();

    assert_eq!(
        focused_diff_statuses(&h, 2),
        vec![DiffStatus::Unchanged, DiffStatus::Unchanged],
        "a move edits no line, so the moved buffer owes no hunks",
    );
}

/// `git mv` also puts the new path in the index. The move still decides
/// the base, so the index's copy under the new path adds nothing staged.
#[test]
fn a_staged_rename_diffs_against_the_old_path() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_rename_scenario("/repo", "old.txt", "new.txt", "a\nb\n", "a\nb\n");
    h.fake_git()
        .add_repo("/repo")
        .index_file("new.txt", "a\nb\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/new.txt"));
    h.settle_diff_jobs();

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    let buffer = h
        .stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    assert_eq!(
        (hunk_flags(dm), dm.staged_counts()),
        (Vec::new(), (0, 0)),
        "a move edits no line, staged or not",
    );
}

#[test]
fn a_moved_and_edited_buffer_shows_only_the_edit() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_rename_scenario("/repo", "old.txt", "new.txt", "a\nb\n", "a\nc\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/new.txt"));
    h.settle_diff_jobs();

    assert_eq!(
        focused_diff_statuses(&h, 2),
        vec![DiffStatus::Unchanged, DiffStatus::Modified],
        "only the edited line is work the move did not carry across",
    );
}

/// Per-line diff status of the focused editor's buffer, for the first
/// `lines` lines. Panics when the buffer carries no diff map, which is the
/// failure a test asserting statuses wants named.
fn focused_diff_statuses(h: &TestHarness, lines: u32) -> Vec<DiffStatus> {
    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer = ws
        .buffers
        .get(ws.editors[editor_id].buffer_id)
        .expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("buffer has a diff map");
    (0..lines).map(|line| dm.status_for_line(line)).collect()
}

/// Neither blob can change without a write under `.git`, which invalidates
/// the cache, so a recompute driven by an edit has no reason to read them
/// again. Each read is a repo-mutex acquisition and a decompression.
#[test]
fn a_second_diff_of_one_file_reads_no_blobs() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    let repo = PathBuf::from("/repo");
    let first = h.fake_git().blob_reads(&repo);
    assert!(first > 0, "the first diff has to read the blobs");

    // Move the buffer so the next drive finds the diff stale and recomputes.
    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    ws.buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .edit(0..0, "x");
    h.settle_diff_jobs();

    assert_eq!(
        h.fake_git().blob_reads(&repo),
        first,
        "the second diff reuses the blobs the first one read",
    );
}

/// A diff walks the whole file on a blocking thread and the next keystroke
/// invalidates it, so a burst is worth one diff at the end rather than one
/// per edit.
#[test]
fn a_burst_of_edits_diffs_once_after_it_settles() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    let repo = PathBuf::from("/repo");
    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;

    // Typing, with a frame between keystrokes and none of them settling.
    for _ in 0..5 {
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "x");
        h.stoat.drive_background();
        h.settle();
        assert!(
            h.stoat.active_workspace().diff.jobs.is_empty(),
            "a keystroke inside the settle window spawns no diff",
        );
    }

    let before = h.fake_git().blob_reads(&repo);
    h.advance_clock(DIFF_SETTLE + std::time::Duration::from_millis(1));
    h.stoat.drive_background();
    assert_eq!(
        h.stoat.active_workspace().diff.jobs.len(),
        1,
        "and the settle spawns one job for the whole burst",
    );

    h.settle();
    h.stoat.drive_background();
    assert_eq!(
        h.fake_git().blob_reads(&repo),
        before,
        "which reuses the cached blobs rather than rereading them",
    );
}

/// A diff job's result that lands after a drive polled its task asks for
/// the frame that installs it.
///
/// The job asks for a frame before its result reaches its task, so the
/// drive in that frame sometimes polls the task too early. The test
/// scheduler runs the job's closure at the spawn and its task only when
/// the scheduler runs. So the drive after the spawn always polls too early.
#[test]
fn a_diff_job_polled_before_it_lands_asks_for_the_frame_that_installs_it() {
    let (mut h, buffer_id) = wanted_harness();
    h.settle_diff_jobs();

    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .edit(0..0, "x");
    h.stoat.drive_background();
    h.advance_clock(DIFF_SETTLE + std::time::Duration::from_millis(1));

    h.stoat.drive_background();
    h.stoat.drive_background();
    assert_eq!(
        h.stoat.active_workspace().diff.jobs.len(),
        1,
        "the drive after the spawn finds the job still in flight",
    );

    let _ = h.stoat.redraw_notify.notified().now_or_never();
    h.run_until_parked();
    let asked = h.stoat.redraw_notify.notified().now_or_never().is_some();
    h.stoat.drive_background();
    assert_eq!(
        (asked, h.stoat.active_workspace().diff.jobs.len()),
        (true, 0),
        "the landed result asks for a frame, and that frame's drive installs it",
    );
}

/// A repository tally that lands after a drive polled its task asks for
/// the frame that installs it, as a diff job does.
#[test]
fn a_tally_polled_before_it_lands_asks_for_the_frame_that_installs_it() {
    let (mut h, _) = wanted_harness();
    h.settle_diff_jobs();

    h.stoat.active_workspace_mut().diff.stale_tally();
    h.stoat.drive_background();
    h.stoat.drive_background();
    assert!(
        h.stoat.active_workspace().diff.tally_job.is_some(),
        "the drive after the spawn finds the tally still in flight",
    );

    let asked = h.wakes_when_parked(h.stoat.redraw_notify.clone());
    h.stoat.drive_background();
    assert_eq!(
        (asked, h.stoat.active_workspace().diff.tally_pending()),
        (true, false),
        "the landed tally asks for a frame, and that frame's drive installs it",
    );
}

/// A want's hunks that land after a pump polled its task ask for the frame
/// that installs them, as a diff job's do.
#[test]
fn a_want_polled_before_it_lands_asks_for_the_frame_that_installs_it() {
    let (mut h, buffer_id) = wanted_harness();

    action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, None);
    let early = action_handlers::review::pump_diff_wants(&mut h.stoat);
    assert!(
        !early && h.stoat.active_workspace().diff.want_pending(buffer_id),
        "the pump after the spawn finds the want still in flight",
    );

    let asked = h.wakes_when_parked(h.stoat.redraw_notify.clone());
    assert_eq!(
        (
            asked,
            action_handlers::review::pump_diff_wants(&mut h.stoat)
        ),
        (true, true),
        "the landed want asks for a frame, and that frame's pump installs it",
    );
}

/// A repo where `a.rs` gains a line over HEAD, open with the background
/// diff on, and the buffer's id. The open leaves the map to the caller.
fn wanted_harness() -> (TestHarness, BufferId) {
    wanted_harness_of("fn foo() {}\n", "fn foo() {}\nfn bar() {}\n")
}

/// [`wanted_harness`] with `a.rs` at `head` in HEAD and at `working` in the
/// working tree.
fn wanted_harness_of(head: &str, working: &str) -> (TestHarness, BufferId) {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.rs", head, working)]);
    h.stoat.set_diff_warm_auto(true);
    h.stoat.open_file(Path::new("/repo/a.rs"));
    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    (h, buffer_id)
}

/// The buffer's diff as `(hunk count, base colors in, hunks-only map in,
/// whole map current)`.
fn wanted_map(h: &TestHarness, buffer_id: BufferId) -> (usize, bool, bool, bool) {
    let ws = h.stoat.active_workspace();
    let (hunks, colors) = {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map installed");
        (
            dm.hunks_in_range(0..u32::MAX).len(),
            dm.base_highlights_for_line(0).is_some(),
        )
    };
    (
        hunks,
        colors,
        ws.hunks_installed(buffer_id),
        ws.diff_map_current(buffer_id),
    )
}

/// A buffer that was opened and not typed in has no burst to wait out, so
/// the drive starts its colors job the frame its hunks land.
#[test]
fn a_wanted_map_lands_hunks_without_colors_and_the_drive_adds_them_with_no_settle() {
    let (mut h, buffer_id) = wanted_harness();

    h.settle_diff_map(buffer_id);
    assert_eq!(
        wanted_map(&h, buffer_id),
        (1, false, true, false),
        "the want lands the hunk with its colors owed",
    );

    h.stoat.drive_background();
    h.settle();
    h.stoat.drive_background();
    assert_eq!(
        wanted_map(&h, buffer_id),
        (1, true, false, true),
        "the drive lands the colors with no clock advance",
    );
}

/// A want's hunks-only map runs no tree pass, so the line pass's spans
/// stand until the drive's map lands. In a file with a grammar they still
/// read as code.
#[test]
fn a_wanted_map_of_a_file_with_a_grammar_marks_no_prose() {
    let (mut h, buffer_id) = wanted_harness_of(
        "let total = alpha + beta;\n",
        "let total = alpha + gamma;\n",
    );
    h.settle_diff_map(buffer_id);

    let ws = h.stoat.active_workspace();
    let prose = {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        span_prose(guard.diff_map.as_ref().expect("diff map installed"))
    };
    assert_eq!(
        (prose, ws.hunks_installed(buffer_id)),
        (vec![ProseChange::None], true),
        "the hunks-only map of a rust file marks no span as prose",
    );
}

/// The blobs a want read predate a `.git` write that lands before it, so
/// its map paints and the next diff reads the blobs again.
#[test]
fn a_git_write_during_a_want_files_no_base() {
    let (mut h, buffer_id) = wanted_harness();
    let repo = PathBuf::from("/repo");
    let before = h.fake_git().blob_reads(&repo);

    action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, None);
    let want_reads = h.fake_git().blob_reads(&repo) - before;
    h.stoat.active_workspace_mut().invalidate_all_diffs();
    h.settle();
    assert_eq!(
        wanted_map(&h, buffer_id),
        (1, false, false, false),
        "the map installs and records neither a marker nor a version",
    );

    let reads = h.fake_git().blob_reads(&repo);
    h.settle_diff_jobs();
    assert_eq!(
        (h.fake_git().blob_reads(&repo) - reads, want_reads > 0),
        (want_reads, true),
        "the next diff reads the blobs the want read",
    );
}

/// A drive job in flight when a diff open wants the same version lands
/// first, and the want's hunks-only map then has nothing to add.
#[test]
fn a_want_that_lands_after_a_drive_job_keeps_the_jobs_colors() {
    let (mut h, buffer_id) = wanted_harness();
    h.stoat.drive_background();
    h.advance_clock(DIFF_SETTLE + std::time::Duration::from_millis(1));
    h.stoat.drive_background();
    assert_eq!(
        h.stoat.active_workspace().diff.jobs.len(),
        1,
        "the drive has a job in flight",
    );

    action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, None);
    h.run_until_parked();
    h.stoat.drive_background();
    assert_eq!(
        wanted_map(&h, buffer_id),
        (1, true, false, true),
        "the job's whole map stays on the buffer",
    );
}

#[test]
fn a_second_want_at_the_same_version_adds_its_landing_and_no_job() {
    let (mut h, buffer_id) = wanted_harness();
    let hops = h.blocking_calls();

    for last in [false, true] {
        action_handlers::review::ensure_diff_map(
            &mut h.stoat,
            buffer_id,
            Some(DiffLanding::EdgeChange { last }),
        );
    }

    let lasts: Vec<bool> = h.stoat.active_workspace().diff.wants[&buffer_id]
        .landings
        .iter()
        .map(|landing| matches!(landing, DiffLanding::EdgeChange { last: true }))
        .collect();
    assert_eq!(
        (h.blocking_calls() - hops, lasts),
        (1, vec![false, true]),
        "one job carries both landings in the order they came",
    );
}

fn file_hop() -> DiffLanding {
    DiffLanding::FileHop {
        dir: movement::ChangeDir::Next,
        unit: movement::ChangeUnit::Tree,
        origin: None,
        wrapped: false,
        fallback: (0, 0..1),
    }
}

/// A hop lands on the runs the tree pass makes, which installed hunks alone
/// do not hold, so it wants the whole map and lands it current.
#[test]
fn a_hop_over_installed_hunks_wants_the_whole_map() {
    let (mut h, buffer_id) = wanted_harness();
    h.settle_diff_map(buffer_id);

    action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, Some(file_hop()));
    let whole = h
        .stoat
        .active_workspace()
        .diff
        .wants
        .get(&buffer_id)
        .map(|want| want.whole);
    assert_eq!(whole, Some(true), "the hop waits for the whole map");

    h.settle();
    assert_eq!(
        wanted_map(&h, buffer_id),
        (1, true, false, true),
        "the whole map lands with its colors and counts as current",
    );
}

#[test]
fn a_whole_want_takes_over_a_hunks_want_in_flight() {
    let (mut h, buffer_id) = wanted_harness();
    let hops = h.blocking_calls();

    action_handlers::review::ensure_diff_map(
        &mut h.stoat,
        buffer_id,
        Some(DiffLanding::EdgeChange { last: false }),
    );
    action_handlers::review::ensure_diff_map(&mut h.stoat, buffer_id, Some(file_hop()));

    let want = &h.stoat.active_workspace().diff.wants[&buffer_id];
    assert_eq!(
        (h.blocking_calls() - hops, want.whole, want.landings.len()),
        (2, true, 2),
        "the whole job replaces the hunks one and carries both landings",
    );
}

#[test]
fn a_pair_base_follows_an_edit_of_its_file() {
    let mut h = pair_harness();
    assert_eq!(
        focused_diff_statuses(&h, 2),
        [DiffStatus::Unchanged, DiffStatus::Modified]
    );

    rewrite_base(&h, "one\nTWO\n");
    h.settle_diff_jobs();
    assert_eq!(
        (pair_text(&h, "/pair/b.txt"), focused_diff_statuses(&h, 2)),
        (
            Some("one\nTWO\n".to_string()),
            vec![DiffStatus::Unchanged, DiffStatus::Unchanged]
        ),
        "the base took the edit and the hunk is gone",
    );
}

/// A file whose git diff is current when the pair opens re-diffs against
/// the pair, rather than keep the map the git diff left.
#[test]
fn a_pair_replaces_the_git_diff_its_file_had() {
    let mut h = TestHarness::with_size(120, 20);
    h.stage_review_scenario("/repo", &[("b.txt", "zero\n", "one\nTWO\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_side_by_side(("/repo/a.txt", "one\ntwo\n"), ("/repo/b.txt", "one\nTWO\n"));
    h.settle_diff_jobs();
    let git = focused_diff_statuses(&h, 2);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
    h.settle_diff_jobs();
    assert_eq!(
        (git, focused_diff_statuses(&h, 2)),
        (
            vec![DiffStatus::Modified, DiffStatus::Modified],
            vec![DiffStatus::Unchanged, DiffStatus::Modified],
        ),
    );
}

/// Closing a diff view with no base installed stales no other file, so a
/// pair in another tab keeps its current map.
#[test]
fn closing_a_git_diff_re_diffs_no_other_file() {
    let mut h = pair_harness();
    let (id, version) = {
        let ws = h.stoat.active_workspace();
        let id = ws
            .buffers
            .id_for_path(Path::new("/pair/b.txt"))
            .expect("the measured file is open");
        let buffer = ws.buffers.get(id).expect("buffer");
        let version = buffer.read().expect("poisoned").snapshot.version;
        (id, version)
    };

    action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
    h.open_file(Path::new("/pair/a.txt"));
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    assert!(
        h.stoat.active_workspace().diff.current(id, version),
        "the pair's map stays current"
    );
}

#[test]
fn each_pair_follows_its_own_base() {
    let mut h = pair_harness();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
    h.open_side_by_side(("/pair/c.txt", "x\ny\n"), ("/pair/d.txt", "x\nY\n"));
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
    h.settle_diff_jobs();

    rewrite_base(&h, "one\nTWO\n");
    h.settle_diff_jobs();
    assert_eq!(
        (pair_text(&h, "/pair/b.txt"), pair_text(&h, "/pair/d.txt")),
        (Some("one\nTWO\n".to_string()), Some("x\ny\n".to_string())),
        "the edit of a.txt moved b.txt's base alone",
    );
}

#[test]
fn a_pair_base_waits_out_the_settle_window() {
    let mut h = pair_harness();
    rewrite_base(&h, "one\nTWO\n");
    h.stoat.drive_background();
    assert_eq!(
        pair_text(&h, "/pair/b.txt"),
        Some("one\ntwo\n".to_string()),
        "a moved base waits for the window like any edit",
    );
}

/// `/pair/b.txt` diffed against `/pair/a.txt`, side by side and settled.
fn pair_harness() -> TestHarness {
    let mut h = TestHarness::with_size(120, 20);
    h.stoat.set_diff_warm_auto(true);
    h.open_side_by_side(("/pair/a.txt", "one\ntwo\n"), ("/pair/b.txt", "one\nTWO\n"));
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
    h.settle_diff_jobs();
    h
}

/// The text of the pair `path` measures against.
fn pair_text(h: &TestHarness, path: &str) -> Option<String> {
    let pair = h.stoat.active_workspace().pair_for(Path::new(path))?;
    Some(pair.text.to_string())
}

/// Replace the whole text of the `/pair/a.txt` buffer, as an edit in its
/// pane does.
fn rewrite_base(h: &TestHarness, text: &str) {
    let ws = h.stoat.active_workspace();
    let id = ws
        .buffers
        .id_for_path(Path::new("/pair/a.txt"))
        .expect("the base file is open");
    let buffer = ws.buffers.get(id).expect("buffer");
    let mut guard = buffer.write().expect("poisoned");
    let len = guard.snapshot.visible_text.len();
    guard.edit(0..len, text);
}

/// Every decoration consumer keys off the diff map's version, and the
/// minimap re-derives the whole file from it, so a version that moves for a
/// map nobody can tell apart is work spent on nothing. A write under `.git`
/// re-diffs every visible buffer whether or not its content moved.
#[test]
fn rediffing_untouched_content_keeps_the_installed_map() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let map_version = |h: &TestHarness| {
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard.diff_map.as_ref().expect("diffed").version()
    };
    let first = map_version(&h);

    // A git write re-diffs every visible buffer, content or no.
    h.stoat.active_workspace_mut().invalidate_all_diffs();
    h.settle_diff_jobs();
    assert_eq!(
        map_version(&h),
        first,
        "a rediff of untouched content keeps the map it already had",
    );

    // A real change to the hunks has to land.
    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .edit(0..0, "new\n");
    h.settle_diff_jobs();
    assert_ne!(
        map_version(&h),
        first,
        "and an added line is a different diff",
    );
}

#[test]
fn drive_diff_jobs_skips_an_already_current_buffer() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    // The buffer's diff is now current. Another event-loop turn must not
    // respawn a job for the unchanged version.
    h.stoat.drive_background();
    assert!(
        h.stoat.active_workspace().diff.jobs.is_empty(),
        "a drive over an already-diffed buffer spawns no new job",
    );
}

#[test]
fn invalidate_all_diffs_redrives_a_visible_buffer_against_the_new_head() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    assert!(
        h.stoat.active_workspace().diff_map_current(buffer_id),
        "the settled job leaves the buffer's diff current",
    );
    assert_eq!(
        base_text_of(&h, buffer_id),
        "a\nb\n",
        "the first diff is computed against the original HEAD",
    );

    // An external rebase moves HEAD under the editor. The buffer is
    // untouched, so only the invalidation can force a re-diff.
    h.fake_git().add_repo("/repo").head_file("a.txt", "a\nZ\n");
    h.stoat.active_workspace_mut().invalidate_all_diffs();

    assert!(
        !h.stoat.active_workspace().diff_map_current(buffer_id),
        "invalidation drops the recorded version",
    );
    h.settle_diff_jobs();
    assert_eq!(
        base_text_of(&h, buffer_id),
        "a\nZ\n",
        "the redrive diffs the buffer against the moved HEAD",
    );
}

/// A job still running against the old base never installs once the base
/// changes, so the map the view opened on stays.
#[test]
fn a_base_change_drops_the_diff_running_against_the_old_base() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_index_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n", "a\nd\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();
    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;

    h.stoat
        .active_workspace_mut()
        .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
    h.stoat.drive_background();
    action_handlers::dispatch(&mut h.stoat, &DiffAgainstIndex);
    h.settle();
    h.stoat.drive_background();

    assert_eq!(
        base_text_of(&h, buffer_id),
        "a\nc\n",
        "the index base stands, and the HEAD job never lands",
    );
}

/// A stage stales the diff of a buffer whose job still runs. The job keeps
/// its slot and loses only its standing, so no second job starts beside it.
#[test]
fn invalidate_marks_the_job_in_flight_superseded() {
    let h = TestHarness::with_size(80, 24);
    let id = BufferId::new(1);
    let mut state = DiffState::default();
    state.jobs.insert(id, pending_job(&h));

    state.invalidate(id, None);

    assert_eq!(
        state.jobs.get(&id).map(|job| job.superseded),
        Some(true),
        "the job stays in its slot, marked superseded",
    );
}

/// A buffer edited while its diff job runs waits for that job rather than
/// starting a second one beside it. Its settle window runs meanwhile, so
/// the next diff lands with no further wait once the job in flight is gone.
#[test]
fn a_newer_version_waits_for_the_job_in_flight() {
    let (mut h, buffer_id) = open_unstaged_change();
    let job = pending_job(&h);
    h.stoat
        .active_workspace_mut()
        .diff
        .jobs
        .insert(buffer_id, job);

    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .edit(0..0, "x");
    h.settle_diff_jobs();
    let current_while_busy = h.stoat.active_workspace().diff_map_current(buffer_id);

    // The clock does not advance from here, so the diff lands only on a
    // settle window that ran while the job was in flight.
    h.stoat.active_workspace_mut().diff.jobs.remove(&buffer_id);
    h.stoat.drive_background();
    h.settle();
    h.stoat.drive_background();

    assert_eq!(
        (
            current_while_busy,
            h.stoat.active_workspace().diff_map_current(buffer_id)
        ),
        (false, true),
        "the settled edit waits for the job in flight, then diffs at once",
    );
}

/// A diff job that never lands, so a drive finds it in flight.
fn pending_job(h: &TestHarness) -> DiffJob {
    DiffJob {
        task: h.stoat.executor.spawn(future::pending()),
        superseded: false,
        base_moved: false,
        started: h.stoat.executor.now(),
    }
}

/// A stage moves the index blob and leaves the buffer where it was, so the
/// mark has to flip on the next drive, with no clock advance and no `.git`
/// event behind it.
#[test]
fn a_git_write_restages_an_unedited_buffer_on_the_next_drive() {
    let (mut h, buffer_id) = open_unstaged_change();
    assert_eq!(
        staged_on_row_one(&h, buffer_id),
        Some(MarkState::Unstaged),
        "the change starts unstaged"
    );

    // The fake's patch apply records the patch and writes no index blob, so
    // the test writes the blob a real stage leaves.
    h.fake_git().add_repo("/repo").index_file("a.txt", "a\nc\n");
    h.stoat
        .active_workspace_mut()
        .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
    h.stoat.drive_background();
    h.settle();
    h.stoat.drive_background();

    assert_eq!(
        staged_on_row_one(&h, buffer_id),
        Some(MarkState::Staged),
        "the next drive reads the staged blob"
    );
}

/// The test scheduler runs the press's git work inline, so the landing
/// waits in the queue. The one drive below is the frame that lands it, and
/// that frame owes the diff the landing staled.
#[test]
fn a_landed_stage_starts_its_diff_in_the_same_frame() {
    let (mut h, buffer_id) = open_unstaged_change();
    assert_eq!(
        staged_on_row_one(&h, buffer_id),
        Some(MarkState::Unstaged),
        "the change starts unstaged"
    );

    movement::set_cursor_row(
        action_handlers::focused_editor_mut(&mut h.stoat).expect("editor"),
        1,
    );
    action_handlers::dispatch(&mut h.stoat, &ToggleStageHunk);
    // The fake's patch apply records the patch and writes no index blob, so
    // the test writes the blob a real stage leaves.
    h.fake_git().add_repo("/repo").index_file("a.txt", "a\nc\n");

    h.stoat.drive_background();
    assert_eq!(
        h.stoat.active_workspace().diff.jobs.len(),
        1,
        "the frame that lands the stage starts its diff"
    );

    h.settle();
    h.stoat.drive_background();
    assert_eq!(
        staged_on_row_one(&h, buffer_id),
        Some(MarkState::Staged),
        "the diff the landing started reads the staged blob"
    );
}

/// Only a job whose base moved under unchanged text skips the settle window
/// and logs its duration. A keystroke's job waits the window out.
#[test]
fn only_a_diff_job_after_a_write_reads_its_base_as_moved() {
    let (mut h, buffer_id) = open_unstaged_change();
    let base_moved = |h: &TestHarness| -> Vec<bool> {
        h.stoat
            .active_workspace()
            .diff
            .jobs
            .values()
            .map(|job| job.base_moved)
            .collect()
    };

    movement::set_cursor_row(
        action_handlers::focused_editor_mut(&mut h.stoat).expect("editor"),
        1,
    );
    action_handlers::dispatch(&mut h.stoat, &ToggleStageHunk);
    h.fake_git().add_repo("/repo").index_file("a.txt", "a\nc\n");
    h.stoat.drive_background();
    let after_write = base_moved(&h);

    h.settle();
    h.stoat.drive_background();
    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .edit(0..0, "x");
    h.stoat.drive_background();
    h.advance_clock(DIFF_SETTLE + std::time::Duration::from_millis(1));
    h.stoat.drive_background();

    assert_eq!(
        (after_write, base_moved(&h)),
        (vec![true], vec![false]),
        "the write's job skips the settle window, and the keystroke's waits it out",
    );
}

/// The watcher's echo of a stage arrives while the job the stage started is
/// in flight. That job already read the staged blob.
#[test]
fn a_git_write_lets_the_job_in_flight_paint() {
    let (mut h, buffer_id) = open_unstaged_change();
    assert_eq!(
        staged_on_row_one(&h, buffer_id),
        Some(MarkState::Unstaged),
        "the change starts unstaged"
    );

    h.fake_git().add_repo("/repo").index_file("a.txt", "a\nc\n");
    h.stoat
        .active_workspace_mut()
        .invalidate_diff(buffer_id, Path::new("/repo/a.txt"));
    h.stoat.drive_background();
    h.stoat.active_workspace_mut().invalidate_all_diffs();

    h.settle();
    h.stoat.drive_background();
    assert_eq!(
        staged_on_row_one(&h, buffer_id),
        Some(MarkState::Staged),
        "the job in flight at the .git write still paints"
    );
    assert!(
        !h.stoat.active_workspace().diff_map_current(buffer_id),
        "a re-run against the fresh blobs is owed"
    );

    h.settle();
    h.stoat.drive_background();
    assert!(
        h.stoat.active_workspace().diff_map_current(buffer_id),
        "the re-run lands and the map is current"
    );
}

/// Open `a.txt` with one unstaged change on row 1, and settle its first
/// diff.
fn open_unstaged_change() -> (TestHarness, BufferId) {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    (h, buffer_id)
}

/// The staged state the installed map gives row 1's change.
fn staged_on_row_one(h: &TestHarness, buffer_id: BufferId) -> Option<MarkState> {
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    guard
        .diff_map
        .as_ref()
        .expect("diff map")
        .staged_for_line(1)
}

/// Every hunk's buffer spans, keyed by the first buffer row it covers.
fn detail_spans_of(h: &TestHarness, buffer_id: BufferId) -> Vec<(u32, usize)> {
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    dm.hunks()
        .map(|hunk| {
            let spans = hunk
                .token_detail
                .as_ref()
                .map_or(0, |detail| detail.buffer_spans.len());
            (hunk.buffer_start_line, spans)
        })
        .collect()
}

/// Open `contents` as a rust file diffed against `base`, and settle the
/// background job that carries the structural refinement.
fn settled_rust_diff(base: &str, buffer: &str) -> (TestHarness, BufferId) {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.rs", base, buffer)]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.rs"));
    h.settle_diff_jobs();
    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    (h, buffer_id)
}

/// Open `buffer` as a file with no language diffed against `base`, and
/// settle the line pass that diffs it.
fn settled_plain_diff(base: &str, buffer: &str) -> (TestHarness, BufferId) {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.unknownext", base, buffer)]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.unknownext"));
    h.settle_diff_jobs();
    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    (h, buffer_id)
}

/// A reindent under a new block is what the structural pass exists for. The
/// line differ sees every moved line's prefix rewritten and washes it; the
/// tree differ sees unchanged statements and marks only the braces.
#[test]
fn a_reindent_under_a_new_block_washes_only_the_braces() {
    const BASE: &str = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";
    const BUFFER: &str =
        "fn f() {\n    if x {\n        let a = 1;\n        let b = 2;\n        let c = 3;\n    }\n}\n";

    let (h, buffer_id) = settled_rust_diff(BASE, BUFFER);
    let spans = detail_spans_of(&h, buffer_id);
    // The block adds four tokens, `if`, `x`, and its two braces. The line
    // differ washes nine spans here, one per rewritten indent.
    assert_eq!(
        spans,
        [(1, 4)],
        "the reindented statements wash nothing, and only `if x` and its braces mark"
    );
}

/// The statuses and extents a settled rust diff of `base` against `buffer`
/// produced, in hunk order.
fn hunk_shapes(base: &str, buffer: &str) -> Vec<(DiffHunkStatus, Range<u32>)> {
    let (h, buffer_id) = settled_rust_diff(base, buffer);
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    dm.hunks()
        .map(|hunk| (hunk.status, hunk.buffer_line_range.clone()))
        .collect()
}

/// Three functions with blank lines between them, one of which relocates.
const THREE_FNS: &str =
    "fn a() {\n    let x = 1;\n}\n\nfn b() {\n    let y = 2;\n}\n\nfn c() {\n    let z = 3;\n}\n";

/// The tree differ marks tokens, so it can never reach a line holding none.
/// A relocated block carrying a blank line has to read as a move anyway, or
/// the separator between two functions decides that neither of them moved.
#[test]
fn a_move_carrying_a_blank_line_is_still_a_move() {
    const MOVED: &str =
        "fn c() {\n    let z = 3;\n}\n\nfn a() {\n    let x = 1;\n}\n\nfn b() {\n    let y = 2;\n}\n";

    assert_eq!(
        MOVED.lines().nth(3),
        Some(""),
        "the fixture needs the blank inside the relocated hunk",
    );
    assert_eq!(
        hunk_shapes(THREE_FNS, MOVED),
        [
            (DiffHunkStatus::Moved, 0..4),
            (DiffHunkStatus::Moved, 11..11),
        ],
        "the four-line block reads as a move, blank line and all, and so does \
         the run it left at the end",
    );
}

/// Half a relocated block is still an addition. Staging reads the extents,
/// so calling a hunk moved when only part of it came from somewhere else
/// would put a move chip on lines that were written here.
#[test]
fn a_hunk_only_partly_relocated_stays_an_addition() {
    const PARTIAL: &str = "fn b() {\n    let y = 2;\n}\n\nfn c() {\n    let z = 3;\n}\n\nfn \
                           a() {\n    let x = 1;\n}\n\nfn d() {\n    let w = 4;\n}\n";

    assert_eq!(
        hunk_shapes(THREE_FNS, PARTIAL),
        [
            (DiffHunkStatus::Moved, 0..0),
            (DiffHunkStatus::Added, 7..15),
        ],
        "the added run holds one relocated function and one written here, so it is an addition",
    );
}

/// A function relocated within a file is the one thing only the tree pass
/// sees. The line differ reads the old site as a deletion and the new one
/// as an addition, and nothing about either says they are halves of the
/// same thing -- which is what left the origin chips and the m/M jumps with
/// no data outside their own tests.
#[test]
fn a_function_moved_within_a_file_reads_as_a_move() {
    const BASE: &str = "fn a() {\n    let x = 1;\n}\n\nfn b() {\n    let y = 2;\n}\n";
    const BUFFER: &str = "fn b() {\n    let y = 2;\n}\n\nfn a() {\n    let x = 1;\n}\n";

    let (h, buffer_id) = settled_rust_diff(BASE, BUFFER);
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");

    let moved: Vec<_> = dm
        .hunks()
        .filter(|hunk| hunk.status == DiffHunkStatus::Moved)
        .collect();
    assert!(
        !moved.is_empty(),
        "a relocated function reads as a move: {:?}",
        dm.hunks()
            .map(|h| (h.status, h.buffer_start_line))
            .collect::<Vec<_>>(),
    );
    assert!(
        moved.iter().any(
            |hunk| hunk.token_detail.as_ref().is_some_and(|detail| detail
                .buffer_spans
                .iter()
                .chain(&detail.base_spans)
                .any(|span| span.move_metadata.is_some()))
        ),
        "and carries the metadata naming where it came from",
    );
}

/// A move is a relabelling, not a re-extent. The staging siblings under this
/// parent read the line ranges, so an `s` on a hunk that just became a move
/// has to stage exactly what it staged as an addition.
#[test]
fn flipping_a_hunk_to_moved_leaves_its_extents_alone() {
    const BASE: &str = "fn a() {\n    let x = 1;\n}\n\nfn b() {\n    let y = 2;\n}\n";
    const BUFFER: &str = "fn b() {\n    let y = 2;\n}\n\nfn a() {\n    let x = 1;\n}\n";

    let extents = |language: bool| {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[(
                match language {
                    true => "a.rs",
                    // No grammar, so the tree pass never runs and the line
                    // pass's reading stands untouched.
                    false => "a.txt",
                },
                BASE,
                BUFFER,
            )],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new(match language {
            true => "/repo/a.rs",
            false => "/repo/a.txt",
        }));
        h.settle_diff_jobs();
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map populated");
        dm.hunks()
            .map(|hunk| (hunk.buffer_line_range.clone(), hunk.unstaged_lines.clone()))
            .collect::<Vec<_>>()
    };

    assert_eq!(
        extents(true),
        extents(false),
        "the tree pass renames the hunks and moves none of their lines",
    );
}

/// A whitespace-only edit marks the row in the gutter but washes nothing,
/// which is an empty detail rather than an absent one.
#[test]
fn a_whitespace_only_edit_carries_empty_detail() {
    const BASE: &str = "fn f() {\n    let a = 1;\n}\n";
    const BUFFER: &str = "fn f() {\n        let a = 1;\n}\n";

    let (h, buffer_id) = settled_rust_diff(BASE, BUFFER);
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    let modified: Vec<_> = dm
        .hunks()
        .filter(|hunk| hunk.status == DiffHunkStatus::Modified)
        .collect();
    assert_eq!(modified.len(), 1, "one modified hunk: {modified:?}");
    let detail = modified[0]
        .token_detail
        .as_ref()
        .expect("a modified hunk carries detail, empty or not");
    assert_eq!(
        (detail.buffer_spans.len(), detail.base_spans.len()),
        (0, 0),
        "the reindented row washes nothing"
    );
}

/// A file with no language never reaches the tree differ, so its washes
/// stay the line pass's char-refined spans.
#[test]
fn a_file_with_no_language_keeps_the_char_refined_spans() {
    let (h, buffer_id) = settled_plain_diff("alpha\n", "alPHa\n");
    let spans = detail_spans_of(&h, buffer_id);
    assert!(
        spans.iter().any(|(_, count)| *count > 0),
        "the line pass still refines the changed characters: {spans:?}"
    );
}

/// A newline added after the last line pairs that line with itself, so the
/// row keeps its gutter mark and neither side washes any of its text.
#[test]
fn a_newline_added_at_the_end_of_a_file_with_no_language_washes_nothing() {
    let (h, buffer_id) = settled_plain_diff("alpha", "alpha\n");
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    let modified: Vec<_> = dm
        .hunks()
        .filter(|hunk| hunk.status == DiffHunkStatus::Modified)
        .collect();
    assert_eq!(modified.len(), 1, "one modified hunk: {modified:?}");

    let detail = modified[0]
        .token_detail
        .as_ref()
        .expect("the line pass details its modified hunk");
    assert_eq!(
        (
            detail.buffer_spans.len(),
            detail.base_spans.len(),
            dm.base_line_refined(0)
        ),
        (0, 0, true),
        "a terminator-only change names no changed chars, and the base row keeps its text"
    );
}

/// The base text of `buffer_id`'s installed diff map.
fn base_text_of(h: &TestHarness, buffer_id: BufferId) -> String {
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    dm.base_text().expect("base text").to_string()
}

/// The settled diff map of a file whose HEAD holds a/b/c/d, whose index
/// holds b->B, and whose working tree adds d->D, under `base`.
///
/// The working tree's pick is the index, so `None` reads the index.
fn half_staged_map(base: Option<DiffBase>) -> DiffMap {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_index_scenario(
        "/repo",
        &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
    );
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    h.stoat.active_workspace_mut().set_diff_base(base);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/f.txt"));
    h.settle_diff_jobs();

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    guard.diff_map.clone().expect("diff map populated")
}

/// Each hunk as `(buffer_start_line, staged)`.
fn hunk_flags(dm: &DiffMap) -> Vec<(u32, bool)> {
    dm.hunks()
        .map(|hunk| (hunk.buffer_start_line, hunk.staged()))
        .collect()
}

/// Under the index pick, the index's own change is a staged mark beside its
/// base line, and the one hunk is the working tree's edit.
#[test]
fn an_index_base_reads_the_index_hunk_as_a_staged_mark() {
    let dm = half_staged_map(None);

    assert_eq!(
        hunk_flags(&dm),
        [(3, false)],
        "the unstaged d->D hunk alone"
    );
    assert_eq!(
        dm.staged_mark_at_base_line(1),
        Some(GutterMark {
            status: DiffHunkStatus::Modified,
            state: MarkState::Staged,
        }),
        "b->B is a staged mark on base line 1",
    );
    assert_eq!(dm.staged_counts(), (1, 1), "the mark and the hunk");
}

/// The HEAD base shows every change since the commit as a hunk, and the
/// index marks the one it already holds.
#[test]
fn a_head_base_reads_index_hunks_staged_and_worktree_edits_unstaged() {
    let dm = half_staged_map(Some(DiffBase::Head));

    assert_eq!(
        hunk_flags(&dm),
        [(1, true), (3, false)],
        "the index-staged line-1 hunk is staged, the line-3 hunk is not",
    );
    assert_eq!(
        *dm.staged_marks,
        Vec::<StagedMark>::new(),
        "and no staged marks"
    );
}

/// A file added to the index has no HEAD blob. It diffs against its index
/// text, and the whole of that text is a staged addition.
#[test]
fn a_file_added_to_the_index_diffs_against_it() {
    let mut h = TestHarness::with_size(80, 24);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    {
        let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        builder.index_file("new.rs", "x\n");
        builder.unstaged_file("new.rs", "x\ny\n");
    }
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/new.rs"));
    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    h.settle_diff_jobs();

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    assert_eq!(
        base_text_of(&h, buffer_id),
        "x\n",
        "the index text is the base"
    );
    let buffer = h
        .stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    assert_eq!(hunk_flags(dm), [(1, false)], "the working tree's added y");
    assert_eq!(
        *dm.staged_marks,
        [StagedMark {
            status: DiffHunkStatus::Added,
            base_lines: 0..1,
        }],
        "the index's x is a staged addition",
    );
}

/// A diff toggle's hunks land without the base's colors, which cost a parse
/// of the whole base file, and the background pass adds the colors later.
#[test]
fn the_diff_toggle_opens_on_hunks_and_takes_its_colors_later() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario(
        "/repo",
        &[("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n")],
    );
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.rs"));
    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    let map = |h: &TestHarness| {
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let dm = guard.diff_map.as_ref().expect("diff map installed");
        (
            dm.hunks_in_range(0..u32::MAX).len(),
            dm.base_highlights_for_line(0).is_some(),
        )
    };

    h.stoat.toggle_diff_view();
    h.settle();
    assert_eq!(
        map(&h),
        (1, false),
        "the toggle opens on the hunk, with the base left uncolored",
    );
    assert!(
        !h.stoat.active_workspace().diff_map_current(buffer_id),
        "and leaves the version unrecorded, so the background pass takes it up",
    );

    h.settle_diff_jobs();
    assert_eq!(
        map(&h),
        (1, true),
        "which lands the same hunk with its colors",
    );
    assert!(
        h.stoat.active_workspace().diff_map_current(buffer_id),
        "and the whole map settles",
    );
}

/// A two-file repo where `a.rs` and `b.rs` differ from HEAD and `clean.rs`
/// does not, opened on `a.rs` with the diff latch armed.
fn latched_harness() -> TestHarness {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario(
        "/repo",
        &[
            ("a.rs", "fn foo() {}\n", "fn foo() {}\nfn bar() {}\n"),
            ("b.rs", "fn baz() {}\n", "fn baz() {}\nfn qux() {}\n"),
        ],
    );
    // Matching HEAD and working text, which stage_review_scenario refuses,
    // so the latch has a genuinely clean file to fall back to plain on.
    h.fake_fs()
        .insert_file(Path::new("/repo/clean.rs"), b"fn same() {}\n");
    {
        let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        builder.head_file("clean.rs", "fn same() {}\n");
    }
    h.open_file(Path::new("/repo/a.rs"));
    h.stoat.toggle_diff_view();
    h
}

fn diff_view_on(h: &mut TestHarness) -> bool {
    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .diff_view
}

fn latched(h: &TestHarness) -> bool {
    let panes = &h.stoat.active_workspace().panes;
    panes.pane(panes.focus()).diff_mode
}

#[test]
fn a_latched_pane_opens_the_next_modified_file_as_a_diff() {
    let mut h = latched_harness();
    h.open_file(Path::new("/repo/b.rs"));

    assert!(
        diff_view_on(&mut h),
        "navigation while latched carries review mode to the new editor"
    );
    assert_eq!(
        h.stoat
            .active_workspace()
            .editors
            .iter()
            .filter(|(_, e)| e.diff_view)
            .count(),
        1,
        "and only the one editor the pane now shows"
    );
}

/// The latch decides review mode per file, so a file with nothing against
/// HEAD reads as a plain editor without disarming the session.
#[test]
fn a_clean_file_shows_plain_while_the_latch_stays_armed() {
    let mut h = latched_harness();
    h.open_file(Path::new("/repo/clean.rs"));

    assert_eq!(
        (diff_view_on(&mut h), latched(&h)),
        (false, true),
        "a clean file opens plain with the latch still on"
    );

    h.open_file(Path::new("/repo/b.rs"));
    assert!(
        diff_view_on(&mut h),
        "so hopping back to a modified file re-enters the diff"
    );
}

/// Under the index base a file whose every change is staged has marks and
/// no hunks, and its left column still has those marks to show.
#[test]
fn a_latched_pane_opens_a_file_with_only_staged_changes_as_a_diff() {
    let mut h = latched_harness();
    h.fake_fs()
        .insert_file(Path::new("/repo/staged.rs"), b"fn new() {}\n");
    {
        let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        builder.head_file("staged.rs", "fn old() {}\n");
        builder.index_file("staged.rs", "fn new() {}\n");
    }
    h.open_file(Path::new("/repo/staged.rs"));

    assert!(diff_view_on(&mut h), "the staged change opens the diff");
}

#[test]
fn toggling_off_over_a_clean_file_disarms_the_latch() {
    let mut h = latched_harness();
    h.open_file(Path::new("/repo/clean.rs"));
    h.stoat.toggle_diff_view();

    assert!(
        !latched(&h),
        "the toggle exits from a latched pane showing a plain file"
    );

    h.open_file(Path::new("/repo/b.rs"));
    assert!(
        !diff_view_on(&mut h),
        "and a later modified file stays plain"
    );
}

/// A file present in HEAD but gone from the working tree diffed into one
/// hunk per run of removed lines, so a file whose content came in two
/// blocks tallied as two removals in the statusline. A removed file is one
/// removal.
///
/// The file is seeded into HEAD alone, with nothing written to the fake fs,
/// because that absence is what makes the open substitute a lone newline
/// and the diff read as a removal.
#[test]
fn a_removed_file_is_one_deleted_hunk() {
    let head = "fn one() {}\n\nfn two() {}\n";
    let (h, buffer_id) = removed_file_harness(head, false);

    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");

    // A removal occupies no buffer rows, so the zero-width hunk never
    // answers a range query. Read the tree itself instead.
    assert_eq!(
        dm.hunks()
            .map(|h| (h.status, h.base_byte_range.clone()))
            .collect::<Vec<_>>(),
        vec![(DiffHunkStatus::Deleted, 0..head.len())],
        "the removal is one hunk spanning the whole base"
    );
    assert_eq!(
        dm.staged_counts(),
        (0, 1),
        "and the statusline tallies it as a single unstaged change"
    );
}

#[test]
fn a_staged_removal_reads_staged() {
    let (h, buffer_id) = removed_file_harness("fn one() {}\n\nfn two() {}\n", true);

    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");

    assert_eq!(
        dm.staged_counts(),
        (1, 0),
        "git rm already put the removal in the index"
    );
}

/// A harness holding one repo file that HEAD has and the working tree does
/// not, with its diff map installed.
///
/// `staged` also drops the index entry, which is what `git rm` leaves
/// behind. Without it the index keeps HEAD's content, as a plain `rm`
/// leaves it.
fn removed_file_harness(head: &str, staged: bool) -> (TestHarness, BufferId) {
    let mut h = TestHarness::with_size(80, 24);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    {
        let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        builder.head_file("gone.rs", head);
        if staged {
            builder.remove_index_file("gone.rs");
        }
    }
    h.open_file(Path::new("/repo/gone.rs"));

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    h.settle_diff_map(buffer_id);

    (h, buffer_id)
}

/// Stand up `/repo/a.rs` with the three texts a checked-out review has:
/// the base commit's content, the checked-out commit's content in HEAD,
/// and the working tree's own. Returns the harness with `base` installed
/// and the buffer's diff computed against it.
fn based_harness(base: Option<DiffBase>, rev: &str, head: &str, working: &str) -> DiffMap {
    let mut h = TestHarness::with_size(80, 24);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    {
        let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        builder.commit("base1", &[("a.rs", rev)]);
        builder.head_file("a.rs", head);
        builder.unstaged_file("a.rs", working);
    }
    h.open_file(Path::new("/repo/a.rs"));
    h.stoat.active_workspace_mut().set_diff_base(base);

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    h.settle_diff_map(buffer_id);

    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    guard.diff_map.clone().expect("diff map populated")
}

#[test]
fn a_rev_base_diffs_against_the_named_commit() {
    let dm = based_harness(
        Some(DiffBase::parent_of("c1", Some("base1".into()))),
        "old\n",
        "mid\n",
        "new\n",
    );
    assert_eq!(
        dm.base_text().map(|t| t.as_str()),
        Some("old\n"),
        "the base is the named commit, not HEAD"
    );
}

/// The rev is what the review is against and HEAD is the commit checked
/// out over it, so the same staged machinery splits the two apart.
#[test]
fn a_rev_base_reads_in_commit_hunks_staged_and_worktree_edits_unstaged() {
    let dm = based_harness(
        Some(DiffBase::parent_of("c1", Some("base1".into()))),
        "a\nx\nb\n",
        "A\nx\nb\n",
        "A\nx\nB\n",
    );
    assert_eq!(
        dm.staged_counts(),
        (1, 1),
        "the commit's own hunk reads staged and the worktree's reads unstaged"
    );
}

#[test]
fn a_rev_base_with_no_sha_diffs_against_nothing() {
    let dm = based_harness(
        Some(DiffBase::parent_of("c1", None)),
        "old\n",
        "mid\n",
        "new\n",
    );
    assert_eq!(
        dm.base_text().map(|t| t.as_str()),
        Some(""),
        "a root commit's parent is the empty tree"
    );
}

#[test]
fn a_memory_base_diffs_against_the_supplied_text() {
    let files = std::iter::once((
        PathBuf::from("/repo/a.rs"),
        Arc::new("proposed\n".to_string()),
    ))
    .collect();
    let dm = based_harness(Some(DiffBase::Memory { files }), "old\n", "mid\n", "new\n");
    assert_eq!(
        dm.base_text().map(|t| t.as_str()),
        Some("proposed\n"),
        "the supplied text is the base"
    );
    assert_eq!(
        dm.staged_counts(),
        (0, 1),
        "and nothing has applied an agent's proposal, so the hunk is unstaged"
    );
}

#[test]
fn a_memory_base_falls_back_for_a_path_it_does_not_carry() {
    let dm = based_harness(
        Some(DiffBase::Memory {
            files: HashMap::new(),
        }),
        "old\n",
        "mid\n",
        "new\n",
    );
    assert_eq!(
        dm.base_text().map(|t| t.as_str()),
        Some("mid\n"),
        "an untouched file keeps the working tree's own base"
    );
}

/// An amend renames the reviewed commit only where the base names the
/// commit that the amend rewrote.
#[test]
fn an_amend_renames_only_the_commit_it_rewrote() {
    let mut h = TestHarness::with_size(80, 24);
    let ws = h.stoat.active_workspace_mut();
    ws.set_diff_base(Some(DiffBase::parent_of("c1", Some("c0".into()))));

    let sides = ["c9", "c1"].map(|old_sha| {
        ws.follow_amended_commit(old_sha, "c2");
        diff_sides(ws.diff_base()).1
    });

    assert_eq!(
        sides,
        ["commit c1", "commit c2"],
        "an amend of another commit leaves the name"
    );
}

#[test]
fn setting_a_diff_base_drops_the_cached_blobs() {
    let mut h = TestHarness::with_size(80, 24);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    {
        let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        builder.head_file("a.rs", "old\n");
        builder.unstaged_file("a.rs", "new\n");
    }
    h.open_file(Path::new("/repo/a.rs"));

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    h.settle_diff_map(buffer_id);
    assert!(
        h.stoat
            .active_workspace()
            .diff
            .holds(buffer_id, Some(Path::new("/repo/a.rs"))),
        "the blobs are cached before the base moves"
    );

    h.stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::parent_of("c1", None)));
    assert!(
        !h.stoat
            .active_workspace()
            .diff
            .holds(buffer_id, Some(Path::new("/repo/a.rs"))),
        "a base change drops blobs read against the base it replaced"
    );
}

#[test]
fn a_crlf_head_leaves_an_lf_buffer_of_equal_content_unchanged() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.txt", "a\r\nb\r\n", "a\nb\n")]);
    h.open_file(Path::new("/repo/a.txt"));

    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    h.settle_diff_map(buffer_id);

    let ws = h.stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    let starts: Vec<u32> = dm
        .hunks_in_range(0..u32::MAX)
        .iter()
        .map(|hunk| hunk.buffer_start_line)
        .collect();
    assert_eq!(
        starts,
        Vec::<u32>::new(),
        "a HEAD blob differing from the buffer only in its line terminators \
         carries no change",
    );
}

#[test]
fn diff_job_highlights_the_base_text() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("a.rs", "fn main() {}\n", "fn other() {}\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.rs"));
    h.settle_diff_jobs();

    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().unwrap();
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    let spans = dm
        .base_highlights_for_line(0)
        .expect("the base's keyword line is highlighted");
    assert!(
        !spans.is_empty(),
        "base line 0 carries tree-sitter token spans"
    );
}

#[test]
fn diff_job_leaves_base_unhighlighted_without_a_language() {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("notes.unknownext", "a\nb\n", "a\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/notes.unknownext"));
    h.settle_diff_jobs();

    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().unwrap();
    let dm = guard.diff_map.as_ref().expect("diff map populated");
    assert!(
        dm.base_highlights_for_line(0).is_none(),
        "a file with no language leaves the base unhighlighted"
    );
}

#[test]
fn diff_job_leaves_a_buffer_outside_any_repo_without_a_diff_map() {
    let mut h = TestHarness::with_size(80, 24);
    h.stoat.set_diff_warm_auto(true);
    let path = h.write_file("loose.txt", "x\ny\n");
    h.open_file(&path);
    h.settle_diff_jobs();

    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    assert!(
        buffer.read().expect("poisoned").diff_map.is_none(),
        "a buffer outside any repo gets no diff map"
    );
}

/// A repo at `/repo` with one tracked change, and `fresh.rs` on disk that
/// no blob holds.
fn repo_with_fresh_file() -> TestHarness {
    let mut h = TestHarness::with_size(80, 24);
    h.stage_review_scenario("/repo", &[("tracked.rs", "one\n", "ONE\n")]);
    h.fake_fs().insert_file("/repo/fresh.rs", b"a\nb\n");
    h.stoat.set_diff_warm_auto(true);
    h
}

/// Opens `path`, settles its diff job, and reads the map the job installs.
fn opened_diff_map<T>(
    h: &mut TestHarness,
    path: &Path,
    read: impl FnOnce(Option<&DiffMap>) -> T,
) -> T {
    h.open_file(path);
    h.settle_diff_jobs();

    let ws = h.stoat.active_workspace();
    let editor_id = match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer = ws
        .buffers
        .get(ws.editors[editor_id].buffer_id)
        .expect("buffer");
    let guard = buffer.read().expect("poisoned");
    read(guard.diff_map.as_ref())
}

/// No blob holds an untracked file, so it diffs against empty text, and
/// every line is one addition that nothing has staged.
#[test]
fn diff_job_gives_an_untracked_buffer_one_added_hunk() {
    let mut h = repo_with_fresh_file();
    let read = opened_diff_map(&mut h, Path::new("/repo/fresh.rs"), |diff_map| {
        let diff_map = diff_map.expect("diff map populated");
        let hunks: Vec<_> = diff_map
            .hunks_in_range(0..u32::MAX)
            .into_iter()
            .map(|hunk| (hunk.buffer_line_range.clone(), hunk.status, hunk.staged()))
            .collect();
        (
            diff_map.base_text().map(|text| text.to_string()),
            hunks,
            diff_map.staged_marks.is_empty(),
        )
    });
    assert_eq!(
        read,
        (
            Some(String::new()),
            vec![(0..2, DiffHunkStatus::Added, false)],
            true
        ),
        "the file diffs against empty text as one unstaged addition with no staged mark",
    );
}

#[test]
fn diff_job_leaves_an_ignored_untracked_buffer_without_a_diff_map() {
    let mut h = repo_with_fresh_file();
    h.fake_git().add_repo("/repo").ignored("fresh.rs");
    assert!(
        opened_diff_map(&mut h, Path::new("/repo/fresh.rs"), |diff_map| diff_map
            .is_none()),
        "an ignored file is no work, so it gets no diff map",
    );
}

/// The workspace's repo answers for every buffer, so a file outside its
/// workdir resolves against it too.
#[test]
fn diff_job_leaves_a_buffer_outside_the_workdir_without_a_diff_map() {
    let mut h = repo_with_fresh_file();
    let path = h.write_file("loose.txt", "x\ny\n");
    assert!(
        opened_diff_map(&mut h, &path, |diff_map| diff_map.is_none()),
        "a file outside the workdir is no work of the repo, so it gets no diff map",
    );
}
