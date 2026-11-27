use std::{
    iter,
    ops::Range,
    path::{Path, PathBuf},
};
use stoat::{
    action_handlers::review::{editor_shows_changes, pressed_row},
    badge::BadgeSource,
    diff_map::{DiffHunk, DiffHunkStatus, DiffMap, MarkKind, MarkState},
    test_harness::TestHarness,
    workspace::{
        diff,
        diff::{DiffBase, WorktreeBase},
    },
};
use stoat_action::Action;
use stoat_text::Point;

/// The text of the buffer open at `path`, which lets a test read a proposal
/// that landed in a file the pane no longer shows.
fn buffer_text_at(h: &TestHarness, path: &str) -> Option<String> {
    let ws = h.stoat.active_workspace();
    let id = ws.buffers.id_for_path(Path::new(path))?;
    let shared = ws.buffers.get(id)?;
    let text = shared.read().expect("buffer poisoned").rope().to_string();
    Some(text)
}

/// The text the diff map of the buffer open at `path` measures against.
fn diff_base_at(h: &TestHarness, path: &str) -> Option<String> {
    let ws = h.stoat.active_workspace();
    let id = ws.buffers.id_for_path(Path::new(path))?;
    let shared = ws.buffers.get(id)?;
    let guard = shared.read().expect("buffer poisoned");
    Some(guard.diff_map.as_ref()?.base_text()?.to_string())
}

/// The base texts a [`DiffBase::Memory`] override carries, by path.
fn memory_base(h: &TestHarness) -> Vec<(String, String)> {
    let Some(DiffBase::Memory { files }) = h.stoat.active_workspace().diff_base() else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = files
        .iter()
        .map(|(path, text)| (path.display().to_string(), text.to_string()))
        .collect();
    out.sort();
    out
}

/// A proposal lands as an unsaved edit on the real file's buffer rather
/// than in a screen of its own, so the editor's own verbs accept and reject
/// it.
#[test]
fn agent_edits_land_as_unsaved_buffer_edits() {
    let mut h = TestHarness::with_size(80, 14);
    h.stoat.active_workspace_mut().git_root = "/work".into();
    h.fake_fs().insert_file("/work/a.rs", b"old\n");
    h.fake_fs().insert_file("/work/b.rs", b"stale\n");

    h.open_agent_edit_review(&[("a.rs", "old\n", "new\n"), ("b.rs", "stale\n", "fresh\n")]);

    assert_eq!(
        (
            buffer_text_at(&h, "/work/a.rs"),
            buffer_text_at(&h, "/work/b.rs")
        ),
        (Some("new\n".to_string()), Some("fresh\n".to_string())),
        "both proposals are sitting in their files, unsaved",
    );
}

/// The base travels with the proposals, since no revision holds what the
/// agent read. It is keyed by absolute path, which is what the diff reads
/// back.
#[test]
fn agent_edits_install_their_own_base() {
    let mut h = TestHarness::with_size(80, 14);
    h.stoat.active_workspace_mut().git_root = "/work".into();
    h.fake_fs().insert_file("/work/a.rs", b"old\n");

    h.open_agent_edit_review(&[("a.rs", "old\n", "new\n")]);

    assert_eq!(
        memory_base(&h),
        vec![("/work/a.rs".to_string(), "old\n".to_string())],
        "the agent's own base is what the proposal diffs against",
    );
}

/// The landing matches the commit openers. The first changed file opens
/// with the latch armed, and a badge names the two verbs.
#[test]
fn agent_edits_land_on_the_first_file_with_the_latch_armed() {
    let mut h = TestHarness::with_size(80, 14);
    h.stoat.active_workspace_mut().git_root = "/work".into();
    h.fake_fs().insert_file("/work/a.rs", b"old\n");
    h.fake_fs().insert_file("/work/b.rs", b"stale\n");

    h.open_agent_edit_review(&[("a.rs", "old\n", "new\n"), ("b.rs", "stale\n", "fresh\n")]);

    let panes = &h.stoat.active_workspace().panes;
    assert_eq!(
        (
            stoat::test_harness::editor::focused_buffer_path(&h.stoat),
            panes.pane(panes.focus()).diff_mode,
            h.stoat.pending_message.as_deref(),
        ),
        (
            PathBuf::from("/work/a.rs"),
            true,
            Some("agent edits: save accepts, :reload rejects"),
        ),
        "the first proposal is on screen, latched, with the verbs named",
    );
}

/// A repo where `a.rs` and `b.rs` differ from HEAD on their second row and
/// `clean.rs` matches it, with no file open.
fn repo_with_a_clean_file() -> TestHarness {
    let mut h = TestHarness::with_size(80, 14);
    h.stage_review_scenario(
        "/repo",
        &[("a.rs", "a\nb\n", "a\nc\n"), ("b.rs", "d\ne\n", "d\nf\n")],
    );
    // stage_review_scenario refuses a file that matches HEAD.
    h.fake_fs().insert_file("/repo/clean.rs", b"same\n");
    {
        let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        builder.head_file("clean.rs", "same\n");
    }
    h
}

/// [`repo_with_a_clean_file`] open on `a.rs` with the latch armed and its
/// hunks in.
fn latched_pair() -> TestHarness {
    let mut h = repo_with_a_clean_file();
    h.open_file(Path::new("/repo/a.rs"));
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();
    h
}

/// Where the diff of the buffer at `path` stands, as `(diff view on the
/// focused editor, hunks on the pool, hunks-only map in, whole map
/// current, hunk count)`.
fn want_state(h: &mut TestHarness, path: &str) -> (bool, bool, bool, bool, usize) {
    let diff_view = stoat::action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .diff_view;
    let ws = h.stoat.active_workspace();
    let id = ws
        .buffers
        .id_for_path(Path::new(path))
        .expect("open buffer");
    let hunks = ws
        .buffers
        .get(id)
        .expect("buffer")
        .read()
        .expect("buffer poisoned")
        .diff_map
        .as_ref()
        .map_or(0, |map| map.hunks_in_range(0..u32::MAX).len());
    (
        diff_view,
        ws.diff_map_pending(id),
        ws.hunks_installed(id),
        ws.diff_map_current(id),
        hunks,
    )
}

/// The test scheduler runs blocking work inline, so the hop count, not the
/// blob reads, is what shows the switch left its diff to the pool.
#[test]
fn a_latched_switch_keeps_the_panes_view_until_its_hunks_land() {
    let mut h = latched_pair();
    let hops = h.blocking_calls();

    h.stoat.open_file(Path::new("/repo/b.rs"));
    assert_eq!(
        (want_state(&mut h, "/repo/b.rs"), h.blocking_calls() - hops),
        ((true, true, false, false, 0), 1),
        "the switch keeps the diff layout and leaves the hunks to the pool",
    );

    h.settle();
    assert_eq!(
        want_state(&mut h, "/repo/b.rs"),
        (true, false, true, false, 1),
        "the hunk lands, and its colors stay owed",
    );
}

#[test]
fn a_latched_switch_to_a_clean_file_goes_plain_when_its_hunks_land() {
    let mut h = latched_pair();

    h.stoat.open_file(Path::new("/repo/clean.rs"));
    assert_eq!(
        want_state(&mut h, "/repo/clean.rs"),
        (true, true, false, false, 0),
        "the switch keeps the diff layout while the hunks are on the pool",
    );

    h.settle();
    assert_eq!(
        want_state(&mut h, "/repo/clean.rs"),
        (false, false, true, false, 0),
        "a file with no change shows plain once its hunks land",
    );
}

#[test]
fn leaving_the_diff_before_a_switch_lands_keeps_the_view_off() {
    let mut h = latched_pair();

    h.stoat.open_file(Path::new("/repo/b.rs"));
    stoat::action_handlers::review::exit_diff_view(&mut h.stoat);
    h.settle();
    assert_eq!(
        want_state(&mut h, "/repo/b.rs"),
        (false, false, true, false, 1),
        "the landing finds the latch gone and leaves the view off",
    );
}

#[test]
fn a_diff_open_after_a_latched_switch_keeps_the_view_on_a_clean_file() {
    let mut h = latched_pair();

    h.stoat.open_file(Path::new("/repo/clean.rs"));
    stoat::action_handlers::review::enter_diff_view(&mut h.stoat);
    h.settle();
    assert_eq!(
        want_state(&mut h, "/repo/clean.rs"),
        (true, false, true, false, 0),
        "the open outranks the switch that waited on the same hunks",
    );
}

/// The number of entries on the focused pane's jumplist.
fn jump_count(h: &TestHarness) -> usize {
    let panes = &h.stoat.active_workspace().panes;
    panes.pane(panes.focus()).jumplist.entries().len()
}

#[test]
fn a_diff_toggle_jumps_to_the_nearest_change_when_its_hunks_land() {
    let mut h = TestHarness::with_size(80, 14);
    h.stage_review_scenario("/repo", &[("a.rs", "a\nb\n", "a\nc\n")]);
    h.open_file(Path::new("/repo/a.rs"));
    let jumps_before = jump_count(&h);

    h.stoat.toggle_diff_view();
    assert_eq!(
        (
            review_cursor_row(&mut h),
            want_state(&mut h, "/repo/a.rs").1,
            jump_count(&h),
        ),
        (0, true, jumps_before),
        "the cursor waits for the hunks",
    );

    h.settle();
    assert_eq!(
        (review_cursor_row(&mut h), jump_count(&h)),
        (1, jumps_before + 1),
        "the cursor lands on the change, and the jump goes on the jumplist",
    );
}

/// The diff view lands a deletion on the live row above its seam. A toggle
/// from a row further down jumps there, and a toggle from that row holds
/// with no jump.
#[test]
fn a_diff_toggle_lands_a_deletion_on_the_row_above_its_seam() {
    let toggle_from = |row: u32| {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", "a\nb\nGONE\nc\nd\ne\n", "a\nb\nc\nd\ne\n")],
        );
        h.open_file(Path::new("/repo/a.rs"));
        {
            let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            stoat::action_handlers::movement::set_cursor_row(editor, row);
        }
        let jumps_before = jump_count(&h);

        h.stoat.toggle_diff_view();
        h.settle();
        (
            stoat::test_harness::editor::focused_cursor_point(&mut h.stoat).row,
            jump_count(&h) - jumps_before,
        )
    };
    assert_eq!(
        [toggle_from(1), toggle_from(4)],
        [(1, 0), (1, 1)],
        "the row above the seam holds, and a row below jumps to it",
    );
}

/// The crossing opens another file, so the status row names the file that
/// has nothing to show.
#[test]
fn a_diff_toggle_on_a_clean_file_says_so_and_crosses() {
    let mut h = TestHarness::with_size(80, 14);
    h.stage_review_scenario("/repo", &[("b.txt", "1\n", "2\n")]);
    h.fake_git().add_repo("/repo").head_file("a.txt", "same\n");
    h.fake_fs().insert_file("/repo/a.txt", b"same\n");
    h.open_file(Path::new("/repo/a.txt"));
    run(&mut h, &stoat_action::DiffFiles { rev: None });

    assert_eq!(
        (
            h.stoat.pending_message.as_deref(),
            stoat::test_harness::editor::focused_buffer_path(&h.stoat),
        ),
        (Some("a.txt has no changes"), PathBuf::from("/repo/b.txt")),
    );
}

/// A second toggle closes the view while the hunks of the first are still
/// on the pool, and their landing finds the view closed.
#[test]
fn a_diff_closed_before_its_hunks_land_leaves_the_cursor() {
    let mut h = TestHarness::with_size(80, 14);
    h.stage_review_scenario("/repo", &[("a.rs", "a\nb\n", "a\nc\n")]);
    h.open_file(Path::new("/repo/a.rs"));
    let jumps_before = jump_count(&h);

    h.stoat.toggle_diff_view();
    h.stoat.toggle_diff_view();
    let pending = want_state(&mut h, "/repo/a.rs").1;
    h.settle();

    assert_eq!(
        (
            pending,
            review_cursor_row(&mut h),
            jump_count(&h),
            want_state(&mut h, "/repo/a.rs").0,
        ),
        (true, 0, jumps_before, false),
        "the hunks land after the close, and the cursor and the jumplist stay",
    );
}

/// A layout key closes the view of a clean file while its hunks are on the
/// pool, so their landing does not cross into the first changed file.
#[test]
fn a_layout_key_before_the_hunks_land_leaves_the_clean_file_open() {
    let mut h = repo_with_a_clean_file();
    h.open_file(Path::new("/repo/clean.rs"));

    h.stoat.toggle_diff_view();
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::TogglePaneWiden);
    let pending = want_state(&mut h, "/repo/clean.rs").1;
    h.settle();

    assert_eq!(
        (
            pending,
            pair_view(&mut h),
            h.stoat.pending_message.as_deref()
        ),
        (
            true,
            (false, false, Some(PathBuf::from("/repo/clean.rs"))),
            Some("diff closed"),
        ),
    );
}

/// The hunk opens on a blank row, so the toggle lands on the row below it
/// that holds text, where a walk lands on the same stop.
#[test]
fn a_diff_toggle_lands_past_a_stop_s_blank_row() {
    let mut h = TestHarness::with_size(80, 14);
    h.stage_review_scenario("/repo", &[("a.rs", "a\nb\n", "a\n\nc\n")]);
    h.open_file(Path::new("/repo/a.rs"));

    h.stoat.toggle_diff_view();
    h.settle();

    assert_eq!(
        stoat::test_harness::editor::focused_cursor_point(&mut h.stoat),
        Point::new(2, 0)
    );
}

/// Each proposal is one edit, so one undo backs one file out. That is the
/// fine-grained reject, and it costs no verb of its own.
#[test]
fn one_undo_rejects_one_file() {
    let mut h = TestHarness::with_size(80, 14);
    h.stoat.active_workspace_mut().git_root = "/work".into();
    h.fake_fs().insert_file("/work/a.rs", b"old\n");

    h.open_agent_edit_review(&[("a.rs", "old\n", "new\n")]);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);

    assert_eq!(
        buffer_text_at(&h, "/work/a.rs"),
        Some("old\n".to_string()),
        "the file is back to what the agent read",
    );
}

/// A proposal for a file that does not exist yet opens empty and reads as a
/// whole-file addition, which is what its empty base says it is.
#[test]
fn a_proposed_new_file_opens_against_an_empty_base() {
    let mut h = TestHarness::with_size(80, 14);
    h.stoat.active_workspace_mut().git_root = "/work".into();

    h.open_agent_edit_review(&[("new.rs", "", "added\n")]);

    assert_eq!(
        (buffer_text_at(&h, "/work/new.rs"), memory_base(&h)),
        (
            Some("added\n".to_string()),
            vec![("/work/new.rs".to_string(), String::new())]
        ),
        "the proposal is in a buffer with nothing behind it",
    );
}

/// The 1-based new-side line and 0-based buffer row of the review
/// editor's text cursor.
fn review_cursor_row(h: &mut TestHarness) -> u32 {
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let bs = snapshot.buffer_snapshot();
    let head = editor.selections.newest_anchor().head();
    bs.rope().offset_to_point(bs.resolve_anchor(&head)).row
}

/// A repo where `b.rs` matches HEAD and differs from commit `base0`, so it
/// is changed against the revision and against nothing else.
fn diff_rev_harness() -> (TestHarness, PathBuf) {
    let mut h = TestHarness::with_size(80, 20);
    let workdir = PathBuf::from("/repo");
    h.stoat.active_workspace_mut().git_root = workdir.clone();
    {
        let mut builder = h.fake_git().add_repo(&workdir).with_fs(h.fake_fs());
        builder.head_file("b.rs", "fn b() {}\nfn b2() {}\n");
        builder.commit("base0", &[("b.rs", "fn b() {}\n")]);
    }
    h.fake_fs()
        .insert_file(workdir.join("b.rs"), b"fn b() {}\nfn b2() {}\n");
    h.stoat.set_diff_warm_auto(true);
    (h, workdir)
}

/// What the three pieces of `:diff-files <rev>` state currently read: the
/// base every buffer diffs against, whether the pane is latched into the
/// diff layout, and which file is open.
fn diff_state(h: &TestHarness) -> (Option<String>, bool, Option<PathBuf>) {
    let base = match h.stoat.active_workspace().diff_base() {
        Some(DiffBase::Rev { sha, .. }) => sha.clone(),
        _ => None,
    };
    let panes = &h.stoat.active_workspace().panes;
    let latched = panes.pane(panes.focus()).diff_mode;
    let path = h
        .stoat
        .active_workspace()
        .buffers
        .path_for(h.stoat.focused_editor_ids().expect("editor").1)
        .map(Path::to_path_buf);
    (base, latched, path)
}

/// A revision points the whole workspace at that commit and opens what
/// changed since it. Against HEAD `b.rs` is untouched, so nothing but the
/// revision could have brought the reader here.
#[test]
fn diff_with_a_rev_installs_the_base_and_opens_the_changed_file() {
    let (mut h, workdir) = diff_rev_harness();

    stoat::action_handlers::dispatch(
        &mut h.stoat,
        &stoat_action::DiffFiles {
            rev: Some("base0".to_string()),
        },
    );
    h.settle();

    assert_eq!(
        diff_state(&h),
        (Some("base0".to_string()), true, Some(workdir.join("b.rs")),),
    );
}

/// The status row names a revision as the reader typed it, beside the short
/// sha it resolved to. A typed sha needs no second spelling.
#[test]
fn diff_with_a_rev_names_the_base_as_typed() {
    let (mut h, workdir) = diff_rev_harness();
    h.fake_git().add_repo(&workdir).branch("main", "base0");

    let sides = ["main", "base0"].map(|rev| {
        stoat::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::DiffFiles {
                rev: Some(rev.to_string()),
            },
        );
        h.settle();
        diff::diff_sides(h.stoat.active_workspace().diff_base())
    });

    assert_eq!(
        sides,
        [("main base0", "working tree"), ("base0", "working tree")]
            .map(|(left, right)| (left.to_string(), right.to_string())),
        "a branch keeps its name, and a typed sha shows once",
    );
}

/// A repo whose `f.txt` has a staged change and an unstaged one, open in
/// the focused pane.
fn half_staged_harness() -> TestHarness {
    let mut h = TestHarness::with_size(80, 20);
    h.stage_index_scenario(
        "/repo",
        &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
    );
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/f.txt"));
    h
}

/// Whether the base is HEAD, and whether the focused editor shows the diff.
fn head_base_and_view(h: &mut TestHarness) -> (bool, bool) {
    let head = matches!(h.stoat.active_workspace().diff_base(), Some(DiffBase::Head));
    let view = stoat::action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .diff_view;
    (head, view)
}

fn run(h: &mut TestHarness, action: &dyn Action) {
    stoat::action_handlers::dispatch(&mut h.stoat, action);
    h.settle();
}

#[test]
fn diff_against_head_installs_the_head_base_and_opens_the_view() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffAgainstHead);
    assert_eq!(head_base_and_view(&mut h), (true, true));
}

/// The installed base with the view closed opens the view, which lands on
/// the first change. With the view already open it changes nothing, so a
/// second press leaves the cursor where the reader moved it.
#[test]
fn diff_against_the_installed_base_keeps_the_open_view() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffAgainstIndex);
    assert_eq!(
        (head_base_and_view(&mut h).1, review_cursor_row(&mut h)),
        (true, 3),
        "the view opens on the unstaged hunk, the first change against the index",
    );

    run(&mut h, &stoat_action::MoveUp);
    run(&mut h, &stoat_action::MoveUp);
    run(&mut h, &stoat_action::DiffAgainstIndex);
    assert_eq!(
        review_cursor_row(&mut h),
        1,
        "and a second press moves nothing"
    );
}

#[test]
fn diff_against_index_returns_from_head_to_the_index() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffAgainstHead);
    run(&mut h, &stoat_action::DiffAgainstIndex);

    assert_eq!(
        (
            h.stoat.active_workspace().diff_base().is_none(),
            head_base_and_view(&mut h).1
        ),
        (true, true),
        "the index base with the view still open",
    );
}

/// Each base pick on the changes view changes the base and leaves the
/// changes view in place, so the stack follows the base.
#[test]
fn a_base_pick_on_the_changes_view_keeps_the_changes_view() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffChanges);

    let picks: [&dyn Action; 5] = [
        &stoat_action::DiffAgainstIndex,
        &stoat_action::DiffAgainstHead,
        &stoat_action::DiffBaseToggle,
        &stoat_action::DiffFiles {
            rev: Some("HEAD".into()),
        },
        &stoat_action::DiffFiles {
            rev: Some("index".into()),
        },
    ];
    let seen: Vec<_> = picks
        .into_iter()
        .map(|action| {
            run(&mut h, action);
            let base = h.stoat.active_workspace().worktree_base();
            (h.stoat.current_view(), base)
        })
        .collect();
    let (index, head) = (WorktreeBase::Index, WorktreeBase::Head);
    assert_eq!(
        seen,
        [index, head, index, head, index].map(|base| (Some("diff_changes"), base)),
    );
}

/// A revision on the changes view points the stack at the revision, so the
/// stack lists what changed since it. `a.rs` changed against HEAD, so the
/// stack holds a file before the revision too.
#[test]
fn a_revision_on_the_changes_view_lists_the_changes_since_it() {
    let (mut h, workdir) = diff_rev_harness();
    h.fake_git()
        .add_repo(&workdir)
        .with_fs(h.fake_fs())
        .modified("a.rs", "a\n", "A\n")
        .commit("base0", &[("a.rs", "a\n"), ("b.rs", "fn b() {}\n")]);
    h.open_file(&workdir.join("b.rs"));
    run(&mut h, &stoat_action::DiffChanges);

    run(
        &mut h,
        &stoat_action::DiffFiles {
            rev: Some("base0".to_string()),
        },
    );
    let listed: Vec<String> = h
        .stoat
        .active_workspace()
        .focused_diff_changes()
        .map(|state| {
            state
                .excerpts
                .iter()
                .map(|excerpt| excerpt.rel_path.clone())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        (h.stoat.current_view(), diff_state(&h).0, listed),
        (
            Some("diff_changes"),
            Some("base0".to_string()),
            vec!["a.rs".to_string(), "b.rs".to_string()]
        ),
    );
}

/// `:git dc` picks HEAD and `:git df` returns to the index, each with the
/// diff view open.
#[test]
fn git_diff_cached_picks_head_and_git_diff_the_index() {
    let mut h = half_staged_harness();
    let mut picked = |command: &str| {
        h.type_text(command);
        h.type_keys("enter");
        h.settle();
        (
            h.stoat.active_workspace().worktree_base(),
            head_base_and_view(&mut h).1,
        )
    };
    assert_eq!(
        [picked(":git dc"), picked(":git df")],
        [(WorktreeBase::Head, true), (WorktreeBase::Index, true)],
    );
}

/// Closing the diff keeps the HEAD base the reader picked, so the gutter
/// of the plain pane goes on marking against HEAD.
#[test]
fn a_bare_diff_close_keeps_the_head_base() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffAgainstHead);
    run(&mut h, &stoat_action::DiffFiles { rev: None });

    assert_eq!(
        head_base_and_view(&mut h),
        (true, false),
        "the view closes onto the HEAD base",
    );
}

/// A revision displaces the HEAD pick only while it holds. Closing the
/// diff returns to HEAD, not to the index.
#[test]
fn a_displaced_head_base_returns_when_the_revision_closes() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffAgainstHead);
    h.stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::named("base0", "base0".into())));
    run(&mut h, &stoat_action::DiffFiles { rev: None });

    assert_eq!(head_base_and_view(&mut h), (true, false));
}

#[test]
fn diff_against_head_under_a_review_drops_the_review_base() {
    let mut h = half_staged_harness();
    h.stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::named("base0", "base0".into())));
    run(&mut h, &stoat_action::DiffAgainstHead);

    assert_eq!(
        head_base_and_view(&mut h),
        (true, true),
        "HEAD replaces the review base"
    );
}

#[test]
fn the_base_toggle_flips_between_head_and_the_index() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffBaseToggle);
    assert_eq!(
        (
            head_base_and_view(&mut h),
            h.stoat.pending_message.as_deref()
        ),
        ((false, false), Some("diff: index → working tree")),
        "the first press picks the index and opens nothing",
    );

    run(&mut h, &stoat_action::DiffBaseToggle);
    assert_eq!(
        (
            head_base_and_view(&mut h),
            h.stoat.pending_message.as_deref()
        ),
        ((true, false), Some("diff: HEAD → working tree")),
        "the second press returns to HEAD",
    );
}

#[test]
fn the_base_toggle_keeps_an_open_view_open() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffFiles { rev: None });
    run(&mut h, &stoat_action::DiffBaseToggle);
    assert_eq!(head_base_and_view(&mut h), (false, true));
}

/// The first press under a revision lands on the pick with no flip, so one
/// press always returns to the working tree.
#[test]
fn the_base_toggle_under_a_revision_returns_to_the_pick() {
    let mut h = half_staged_harness();
    h.stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::named("base0", "base0".into())));
    run(&mut h, &stoat_action::DiffBaseToggle);

    assert_eq!(
        (
            matches!(h.stoat.active_workspace().diff_base(), Some(DiffBase::Head)),
            h.stoat.pending_message.as_deref(),
        ),
        (true, Some("diff: HEAD → working tree")),
    );
}

#[test]
fn diff_head_as_a_word_installs_the_head_base() {
    let mut h = half_staged_harness();
    run(
        &mut h,
        &stoat_action::DiffFiles {
            rev: Some("HEAD".into()),
        },
    );
    assert_eq!(head_base_and_view(&mut h), (true, true));
}

#[test]
fn diff_index_as_a_word_returns_to_the_index() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffAgainstHead);
    run(
        &mut h,
        &stoat_action::DiffFiles {
            rev: Some("index".into()),
        },
    );

    assert_eq!(
        (
            h.stoat.active_workspace().diff_base().is_none(),
            head_base_and_view(&mut h).1
        ),
        (true, true),
    );
}

#[test]
fn the_base_edit_seeds_the_palette_with_the_current_base() {
    let mut h = half_staged_harness();
    let mut seeds = vec![base_edit_seed(&mut h)];
    run(&mut h, &stoat_action::DiffAgainstIndex);
    seeds.push(base_edit_seed(&mut h));
    for base in [
        DiffBase::parent_of("c1", Some("base0".into())),
        DiffBase::parent_of("c1", None),
        DiffBase::named("main", "base0".into()),
    ] {
        h.stoat.active_workspace_mut().set_diff_base(Some(base));
        seeds.push(base_edit_seed(&mut h));
    }

    assert_eq!(
        seeds,
        [
            "diff-files HEAD",
            "diff-files index",
            "diff-files base0",
            "diff-files ",
            "diff-files main"
        ]
        .map(|seed| Some(seed.to_string())),
    );
}

/// The text `DiffBaseEdit` opens the palette with. The palette closes again,
/// so the next press opens a fresh one.
fn base_edit_seed(h: &mut TestHarness) -> Option<String> {
    run(h, &stoat_action::DiffBaseEdit);
    let seed = h
        .stoat
        .command_palette
        .as_ref()
        .map(|palette| palette.input.text(h.stoat.active_workspace()));
    run(h, &stoat_action::CancelPromptInput);
    seed
}

#[test]
fn diff_pair_opens_the_second_file_against_the_first() {
    let mut h = pair_harness();
    run(&mut h, &stoat_action::DiffPair);
    assert_eq!(
        (
            pair_base(&h, "/pair/b.txt"),
            pair_view(&mut h),
            review_cursor_row(&mut h),
            h.stoat.pending_message.as_deref(),
        ),
        (
            Some(("/pair/a.txt".into(), "one\ntwo\nthree\n".into())),
            (true, true, Some(PathBuf::from("/pair/b.txt"))),
            1,
            None,
        ),
        "the right file diffs against the left, focused and widened, with the cursor on \
         the first difference and no word that the files match",
    );
}

#[test]
fn a_second_diff_pair_closes_the_view() {
    let mut h = pair_harness();
    run(&mut h, &stoat_action::DiffPair);
    run(&mut h, &stoat_action::DiffPair);
    assert_eq!(
        (pair_base(&h, "/pair/b.txt"), pair_view(&mut h)),
        (None, (false, false, Some(PathBuf::from("/pair/b.txt")))),
    );
}

/// A split pressed on the diff closes the diff and splits nothing, so the
/// view never ends up narrow beside a new pane.
#[test]
fn a_split_on_the_diff_closes_it_and_splits_nothing() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffFiles { rev: None });

    run(&mut h, &stoat_action::SplitRight);

    assert_eq!(layout(&h), (1, Some("file"), false, Some("diff closed")));
}

/// A focus move on a diff widened over a split closes the diff where it
/// stands, and the split it covered comes back.
#[test]
fn a_focus_move_on_the_diff_closes_it_in_place() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::SplitRight);
    run(&mut h, &stoat_action::DiffFiles { rev: None });
    let focus = h.stoat.active_workspace().panes.focus();

    run(&mut h, &stoat_action::FocusLeft);

    assert_eq!(
        (
            h.stoat.active_workspace().panes.focus() == focus,
            layout(&h)
        ),
        (true, (2, Some("file"), false, Some("diff closed"))),
    );
}

#[test]
fn a_widen_toggle_on_the_diff_closes_it() {
    let mut h = half_staged_harness();
    run(&mut h, &stoat_action::DiffFiles { rev: None });

    run(&mut h, &stoat_action::TogglePaneWiden);

    assert_eq!(layout(&h), (1, Some("file"), false, Some("diff closed")));
}

/// A focus move on a pair diff closes the view and drops the pair, as the
/// bare `:diff-files` does.
#[test]
fn a_focus_move_on_a_pair_diff_drops_the_pair() {
    let mut h = pair_harness();
    run(&mut h, &stoat_action::DiffPair);

    run(&mut h, &stoat_action::FocusLeft);

    assert_eq!(
        (
            pair_base(&h, "/pair/b.txt"),
            pair_view(&mut h),
            h.stoat.pending_message.as_deref(),
        ),
        (
            None,
            (false, false, Some(PathBuf::from("/pair/b.txt"))),
            Some("diff closed"),
        ),
    );
}

/// Opening a diff turns off a diff that another pane of the tab still
/// shows, whether the open toggles or latches, so the tab holds one diff.
#[test]
fn opening_a_diff_turns_off_the_diff_beside_it() {
    let toggle: fn(&mut TestHarness) = |h| run(h, &stoat_action::DiffFiles { rev: None });
    let latch: fn(&mut TestHarness) = |h| {
        stoat::action_handlers::review::latch_diff_view(&mut h.stoat);
        h.settle();
    };
    for (name, open) in [("toggle", toggle), ("latch", latch)] {
        let mut h = half_staged_harness();
        let left = h.stoat.active_workspace().panes.focus();
        let (left_editor, _) = h.stoat.focused_editor_ids().expect("editor");
        run(&mut h, &stoat_action::SplitRight);
        let (right_editor, _) = h.stoat.focused_editor_ids().expect("editor");

        {
            let ws = h.stoat.active_workspace_mut();
            ws.panes.pane_mut(left).diff_mode = true;
            ws.editors[left_editor].set_diff_view(true);
        }

        open(&mut h);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.panes.pane(left).diff_mode,
                ws.editors[left_editor].diff_view,
                ws.editors[right_editor].diff_view,
            ),
            (false, false, true),
            "after the {name}, only the right pane shows a diff",
        );
    }
}

/// The pane count, the screen in front, whether a pane is widened, and the
/// status message, which together show what a layout action did.
fn layout(h: &TestHarness) -> (usize, Option<&'static str>, bool, Option<&str>) {
    let panes = &h.stoat.active_workspace().panes;
    (
        panes.split_pane_ids().len(),
        h.stoat.current_view(),
        panes.widened().is_some(),
        h.stoat.pending_message.as_deref(),
    )
}

#[test]
fn diff_pair_over_equal_files_says_so_and_stays_put() {
    let mut h = TestHarness::with_size(120, 20);
    h.stage_review_scenario("/repo", &[("c.txt", "1\n", "2\n")]);
    h.open_side_by_side(("/repo/a.txt", "same\n"), ("/repo/b.txt", "same\n"));
    run(&mut h, &stoat_action::DiffPair);

    let view = pair_view(&mut h);
    assert_eq!(
        (
            h.stoat.pending_changed_file_jump.is_none(),
            h.stoat.pending_message.as_deref(),
            view,
        ),
        (
            true,
            Some("the two files do not differ"),
            (true, true, Some(PathBuf::from("/repo/b.txt"))),
        ),
        "no hop into the repo's changed file",
    );
}

/// A reader who leaves the measured file before its hunks land gets no
/// verdict about the file they left.
///
/// The focus moves as a click into the other pane moves it, because a focus
/// key on the diff closes the diff instead.
#[test]
fn the_verdict_drops_once_the_focus_moves_on() {
    let mut h = TestHarness::with_size(120, 20);
    h.open_side_by_side(("/pair/a.txt", "same\n"), ("/pair/b.txt", "same\n"));
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
    {
        let panes = &mut h.stoat.active_workspace_mut().panes;
        let measured = panes.focus();
        let left = panes
            .split_pane_ids()
            .into_iter()
            .find(|&id| id != measured)
            .expect("the left pane");
        panes.set_focus(left);
    }
    h.settle();

    assert_eq!(h.stoat.pending_message.as_deref(), None);
}

/// A focus key closes a pair diff while its hunks are on the pool, so no
/// verdict and no change walk reports on the closed pair.
#[test]
fn a_pair_diff_closed_before_its_hunks_land_gets_no_verdict() {
    let mut h = TestHarness::with_size(120, 20);
    h.open_side_by_side(("/pair/a.txt", "same\n"), ("/pair/b.txt", "same\n"));
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
    let pending = want_state(&mut h, "/pair/b.txt").1;
    h.settle();

    assert_eq!(
        (pending, h.stoat.pending_message.as_deref()),
        (true, Some("diff closed")),
    );
}

/// The git diff on the measured file before the press shows changes, and
/// the pair's own hunks show none, so the verdict waits for the pair's.
#[test]
fn equal_files_say_so_over_a_file_with_a_git_diff() {
    let mut h = TestHarness::with_size(120, 20);
    h.stage_review_scenario("/repo", &[("b.txt", "zero\n", "same\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_side_by_side(("/repo/a.txt", "same\n"), ("/repo/b.txt", "same\n"));
    h.settle_diff_jobs();
    run(&mut h, &stoat_action::DiffPair);

    let view = pair_view(&mut h);
    assert_eq!(
        (h.stoat.pending_message.as_deref(), view),
        (
            Some("the two files do not differ"),
            (true, true, Some(PathBuf::from("/repo/b.txt"))),
        ),
    );
}

#[test]
fn the_change_walk_ends_inside_a_pair() {
    let mut h = repo_pair_harness();
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);

    let path = pair_view(&mut h).2;
    assert_eq!(
        (
            h.stoat.pending_changed_file_jump.is_none(),
            h.stoat.pending_message.as_deref(),
            path,
        ),
        (
            true,
            Some("no more changes"),
            Some(PathBuf::from("/repo/b.txt"))
        ),
    );
}

/// A git diff in another tab is no part of the pair, so closing it leaves
/// the pair and its view as they were.
#[test]
fn a_git_diff_closed_in_another_tab_leaves_the_pair() {
    let mut h = repo_pair_harness();
    run(&mut h, &stoat_action::NewTab);
    h.open_file(Path::new("/repo/c.txt"));
    run(&mut h, &stoat_action::DiffFiles { rev: None });
    run(&mut h, &stoat_action::DiffFiles { rev: None });
    run(&mut h, &stoat_action::PrevTab);

    assert_eq!(
        (pair_base(&h, "/repo/b.txt"), pair_view(&mut h)),
        (
            Some(("/repo/a.txt".into(), "one\ntwo\n".into())),
            (true, true, Some(PathBuf::from("/repo/b.txt"))),
        ),
    );
}

#[test]
fn two_pairs_in_two_tabs_coexist() {
    let h = two_tab_pairs();
    let ws = h.stoat.active_workspace();
    assert_eq!(
        (
            pair_base(&h, "/repo/b.txt"),
            pair_base(&h, "/repo/d.txt"),
            [ws.tab_title(0), ws.tab_title(1)],
        ),
        (
            Some(("/repo/a.txt".into(), "one\ntwo\n".into())),
            Some(("/repo/c2.txt".into(), "x\ny\n".into())),
            ["\u{b1} b.txt".to_string(), "\u{b1} d.txt".to_string()],
        ),
        "each tab measures its own pair and names the file it edits",
    );
}

#[test]
fn closing_a_pair_view_drops_only_its_own_pair() {
    let mut h = two_tab_pairs();
    h.stoat.set_diff_warm_auto(true);
    run(&mut h, &stoat_action::DiffPair);
    h.settle_diff_jobs();

    let (editor, _) = h.stoat.focused_editor_ids().expect("editor");
    assert_eq!(
        (
            pair_base(&h, "/repo/b.txt"),
            pair_base(&h, "/repo/d.txt"),
            editor_shows_changes(&mut h.stoat, editor),
            diff_base_at(&h, "/repo/d.txt"),
        ),
        (
            Some(("/repo/a.txt".into(), "one\ntwo\n".into())),
            None,
            true,
            Some(String::new()),
        ),
        "d.txt diffs against the repo again, which does not track it, so every line reads \
         as added",
    );
}

/// Every diff command over a paired file returns the file to the base the
/// command names, or to the workspace's base for the bare close. The base
/// edit seeds a bare `diff-files `, since a pair has no revision to
/// retype.
#[test]
fn diff_commands_over_a_paired_file_drop_its_pair() {
    let paired = || {
        let (mut h, _) = diff_rev_harness();
        h.open_side_by_side(("/repo/a.txt", "one\n"), ("/repo/b.txt", "one\ntwo\n"));
        run(&mut h, &stoat_action::DiffPair);
        h
    };
    let after = |action: &dyn Action| {
        let mut h = paired();
        run(&mut h, action);
        pair_base(&h, "/repo/b.txt")
    };
    let named = |rev: &str| stoat_action::DiffFiles {
        rev: Some(rev.to_string()),
    };

    assert_eq!(
        (
            pair_base(&paired(), "/repo/b.txt"),
            base_edit_seed(&mut paired()),
            [
                after(&stoat_action::DiffFiles { rev: None }),
                after(&named("HEAD")),
                after(&named("index")),
                after(&named("base0")),
                after(&stoat_action::DiffBaseToggle),
            ],
        ),
        (
            Some(("/repo/a.txt".into(), "one\n".into())),
            Some("diff-files ".to_string()),
            [None, None, None, None, None],
        ),
    );
}

/// `/repo/b.txt` diffed against `/repo/a.txt`, in a repo whose own change
/// is `c.txt`.
fn repo_pair_harness() -> TestHarness {
    let mut h = TestHarness::with_size(120, 20);
    h.stage_review_scenario("/repo", &[("c.txt", "1\n", "2\n")]);
    h.open_side_by_side(("/repo/a.txt", "one\ntwo\n"), ("/repo/b.txt", "one\nTWO\n"));
    run(&mut h, &stoat_action::DiffPair);
    h
}

/// [`repo_pair_harness`] with a second tab, active, that diffs
/// `/repo/d.txt` against `/repo/c2.txt`.
fn two_tab_pairs() -> TestHarness {
    let mut h = repo_pair_harness();
    run(&mut h, &stoat_action::NewTab);
    h.open_side_by_side(("/repo/c2.txt", "x\ny\n"), ("/repo/d.txt", "x\nY\n"));
    run(&mut h, &stoat_action::DiffPair);
    h
}

#[test]
fn diff_pair_without_a_pair_badges_and_changes_nothing() {
    let mut h = TestHarness::with_size(80, 20);
    run(&mut h, &stoat_action::DiffPair);

    let badge = h
        .stoat
        .active_workspace()
        .badges
        .find_by_source(BadgeSource::Review)
        .and_then(|id| h.stoat.active_workspace().badges.get(id))
        .map(|badge| badge.label.clone());
    assert_eq!(
        (
            badge.as_deref(),
            matches!(h.stoat.active_workspace().diff_base(), Some(DiffBase::Head))
        ),
        (Some("diff pair needs two files in two panes"), true),
    );
}

/// Two files side by side with the focus on the first, the base.
fn pair_harness() -> TestHarness {
    let mut h = TestHarness::with_size(120, 20);
    h.open_side_by_side(
        ("/pair/a.txt", "one\ntwo\nthree\n"),
        ("/pair/b.txt", "one\nTWO\nthree\n"),
    );
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
    h
}

/// The `(base_path, text)` of the pair `path` measures against.
fn pair_base(h: &TestHarness, path: &str) -> Option<(String, String)> {
    let pair = h.stoat.active_workspace().pair_for(Path::new(path))?;
    Some((pair.base_path.display().to_string(), pair.text.to_string()))
}

/// The focused editor's diff flag, whether the focused pane is widened, and
/// the focused buffer's path.
fn pair_view(h: &mut TestHarness) -> (bool, bool, Option<PathBuf>) {
    let diff_view = stoat::action_handlers::focused_editor_mut(&mut h.stoat)
        .is_some_and(|editor| editor.diff_view);
    let ws = h.stoat.active_workspace();
    let widened = ws.panes.widened() == Some(ws.panes.focus());
    let path = h
        .stoat
        .focused_editor_ids()
        .and_then(|(_, buffer_id)| ws.buffers.path_for(buffer_id).map(Path::to_path_buf));
    (diff_view, widened, path)
}

/// The bare command closes the diff, and a base a revision installed goes
/// with it. Leaving it behind would have every later diff silently measured
/// against a commit the reader thought they had closed.
#[test]
fn a_bare_diff_clears_the_base_a_rev_installed() {
    let (mut h, _) = diff_rev_harness();
    stoat::action_handlers::dispatch(
        &mut h.stoat,
        &stoat_action::DiffFiles {
            rev: Some("base0".to_string()),
        },
    );
    h.settle();

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();

    let (base, latched, _) = diff_state(&h);
    assert_eq!((base, latched), (None, false));
}

/// Naming a revision asks to look at it, so running it again re-targets the
/// view rather than closing it. Only the bare command closes, which is what
/// keeps `:diff-files <rev>` from being a toggle whose meaning depends on
/// what the reader last did.
#[test]
fn a_repeated_diff_rev_keeps_the_view_open() {
    let (mut h, workdir) = diff_rev_harness();
    let open = |h: &mut TestHarness| {
        stoat::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::DiffFiles {
                rev: Some("base0".to_string()),
            },
        );
        h.settle();
    };

    open(&mut h);
    open(&mut h);

    assert_eq!(
        diff_state(&h),
        (Some("base0".to_string()), true, Some(workdir.join("b.rs")),),
        "the second run lands on the same view rather than closing it",
    );
}

/// The base holds the resolved commit, not the text the reader typed. A
/// revspec is a name for a commit at one moment: `main` moves, a prefix is
/// not a sha, and either would leave the base naming something that stops
/// meaning the commit it was chosen for.
#[test]
fn diff_stores_the_resolved_sha_rather_than_the_revspec() {
    let (mut h, _) = diff_rev_harness();

    stoat::action_handlers::dispatch(
        &mut h.stoat,
        &stoat_action::DiffFiles {
            rev: Some("base".to_string()),
        },
    );
    h.settle();

    let (base, _, _) = diff_state(&h);
    assert_eq!(
        base.as_deref(),
        Some("base0"),
        "the prefix resolved to the commit it names",
    );
}

/// A revision the repo cannot resolve changes nothing. Half-applying it
/// would leave the reader in a diff against a base they never named.
#[test]
fn an_unknown_rev_badges_and_changes_nothing() {
    let (mut h, _) = diff_rev_harness();
    let before = diff_state(&h);

    stoat::action_handlers::dispatch(
        &mut h.stoat,
        &stoat_action::DiffFiles {
            rev: Some("nosuch".to_string()),
        },
    );
    h.settle();

    assert_eq!(diff_state(&h), before, "no state moved");
    let badge = h
        .stoat
        .active_workspace()
        .badges
        .find_by_source(BadgeSource::Review)
        .and_then(|id| h.stoat.active_workspace().badges.get(id))
        .map(|badge| badge.label.clone());
    assert_eq!(
        badge.as_deref(),
        Some("unknown revision"),
        "the refusal is reported rather than silent",
    );
}

#[test]
fn diff_recomputes_a_map_staled_by_an_external_git_mutation() {
    let mut h = TestHarness::with_size(80, 14);
    // a.txt matches HEAD, so its first diff map holds no hunks at all --
    // exactly the state a buffer is left in when it was clean before an
    // external rebase moved HEAD out from under it.
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .head_file("a.txt", "a\nb\nc\n");
    h.fake_fs()
        .insert_file(PathBuf::from("/repo/a.txt"), b"a\nb\nc\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.txt"));
    h.settle_diff_jobs();

    h.fake_git()
        .add_repo("/repo")
        .head_file("a.txt", "a\nOLD\nc\n");
    h.stoat.active_workspace_mut().invalidate_all_diffs();

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();

    assert_eq!(
        review_cursor_row(&mut h),
        1,
        "the stale empty map is recomputed and the cursor lands on the new first hunk",
    );
}

fn open_git_file_at_cursor(h: &mut TestHarness, row: u32) -> PathBuf {
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, row);
    workdir
}

/// Open a file with two modified lines four unchanged lines apart, cursor
/// on `row`. The gap is under the old chunk extraction's 6-row merge
/// window, so this is exactly the shape that used to stage both.
fn open_two_hunk_file_at(h: &mut TestHarness, row: u32) -> PathBuf {
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(
        &workdir,
        &[(
            "a.rs",
            "a\nb\nc\nd\ne\nf\ng\nh\n",
            "a\nZ\nc\nd\ne\nf\nY\nh\n",
        )],
    );
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, row);
    workdir
}

/// The staged unit is the hunk the gutter draws. Two hunks close enough to
/// share a context window still stage one at a time.
#[test]
fn staging_one_hunk_leaves_a_nearby_hunk_alone() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_two_hunk_file_at(&mut h, 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    let patch = &patches[0];
    assert!(
        patch.contains("-b\n"),
        "removes the first base line: {patch}"
    );
    assert!(
        patch.contains("+Z\n"),
        "adds the first buffer line: {patch}"
    );
    assert!(
        !patch.contains("-g\n") && !patch.contains("+Y\n"),
        "and the second hunk's change is absent: {patch}"
    );
    // Context stops before the neighbor, so the patch never restates the
    // second hunk's row as unchanged base text and silently reverts it.
    assert!(
        patch.contains("@@ -1,5 +1,5 @@"),
        "and the context stops short of the second hunk: {patch}"
    );
}

/// Hunks closer together than the context width. Context has to stop at the
/// neighbor rather than restating its changed row as unchanged base text,
/// which would silently revert the second change on apply.
#[test]
fn context_stops_at_a_hunk_closer_than_the_context_width() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\ne\n", "a\nZ\nc\nd\nY\n")]);
    h.open_file(&workdir.join("a.rs"));
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 1);
    }

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    let patch = &patches[0];
    assert!(
        !patch.contains("\n e\n"),
        "the neighbor's base line is not carried as context: {patch}"
    );
    assert!(
        patch.contains("@@ -1,4 +1,4 @@"),
        "the patch stops one row short of the neighbor: {patch}"
    );
}

/// The other hunk of the same pair stages on its own too, so the split is
/// the gutter's and not an artifact of which one comes first.
#[test]
fn staging_the_second_hunk_leaves_the_first_alone() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_two_hunk_file_at(&mut h, 6);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    let patch = &patches[0];
    assert!(
        patch.contains("-g\n"),
        "removes the second base line: {patch}"
    );
    assert!(
        patch.contains("+Y\n"),
        "adds the second buffer line: {patch}"
    );
    assert!(
        !patch.contains("-b\n") && !patch.contains("+Z\n"),
        "and the first hunk's change is absent: {patch}"
    );
}

/// A block that the buffer wraps in a new `if`, which reindents every line.
const REINDENT_BASE: &str = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";

/// [`REINDENT_BASE`] wrapped in `if x { .. }`, with `c` set to 4. The line
/// pass sees rows 1 to 5 changed, and the tree pass marks two runs: the
/// `if x` on row 1, and the `4` with the closing brace on rows 4 and 5.
const REINDENT_BUFFER: &str =
    "fn f() {\n    if x {\n        let a = 1;\n        let b = 2;\n        let c = 4;\n    }\n}\n";

/// The `if x {` of [`REINDENT_BUFFER`] staged on its own, as an insertion
/// above the index's unchanged `let a = 1;`.
const REINDENT_IF_STAGE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,5 +1,6 @@\n fn f() {\n+    if x {\n     let a = 1;\n     let b = 2;\n     let c = 3;\n }\n";

/// The stop that the `if x {` of [`REINDENT_BUFFER`] opens, rows 1 to 3,
/// staged on its own. The stop reaches the second run, so the reindented
/// `let` rows move with the `if x {`.
const REINDENT_IF_STOP_STAGE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,5 +1,6 @@\n fn f() {\n+    if x {\n-    let a = 1;\n+        let a = 1;\n-    let b = 2;\n+        let b = 2;\n     let c = 3;\n }\n";

/// The second run of [`REINDENT_BUFFER`], rows 4 and 5, staged on its own.
/// Each row carries its new indent, since a staged row is the whole buffer
/// line.
const REINDENT_TAIL_STAGE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,5 +1,6 @@\n fn f() {\n     let a = 1;\n     let b = 2;\n-    let c = 3;\n+        let c = 4;\n+    }\n }\n";

/// Open `file` holding `buffer` over a HEAD of `base`, with its diff jobs
/// settled so the tree pass has marked its runs, cursor on `row`.
fn open_settled_at(h: &mut TestHarness, file: &str, base: &str, buffer: &str, row: u32) -> PathBuf {
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[(file, base, buffer)]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join(file));
    h.settle_diff_jobs();
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, row);
    workdir
}

/// Open [`REINDENT_BUFFER`] over [`REINDENT_BASE`], cursor on `row`.
fn open_reindent_at(h: &mut TestHarness, row: u32) -> PathBuf {
    open_settled_at(h, "a.rs", REINDENT_BASE, REINDENT_BUFFER, row)
}

/// A reindent pairs each moved line with its old self, so the `if x {` it
/// added stages as an insertion rather than over `let a = 1;`.
#[test]
fn stage_line_on_a_line_a_reindent_added_inserts_it() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_reindent_at(&mut h, 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    assert_eq!(h.fake_git().applied_patches(&workdir), [REINDENT_IF_STAGE]);
}

/// The gutter marks only the runs that hold an edit of their own, but a
/// stop reaches from its run to the next. So `s` on the first run moves
/// the reindented lines below it too, and leaves the second run unstaged.
#[test]
fn staging_a_narrowed_hunk_moves_the_stop_under_the_cursor() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_reindent_at(&mut h, 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    h.settle();

    assert_eq!(
        (
            h.fake_git().applied_patches(&workdir),
            h.stoat.pending_message.as_deref(),
        ),
        (
            vec![REINDENT_IF_STOP_STAGE.to_string()],
            Some("staged hunk")
        ),
        "the stop moves alone, and the status names the hunk keys pressed",
    );
}

/// A failed index write reports why, and claims no stage.
#[test]
fn a_run_whose_index_write_fails_reports_why() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_reindent_at(&mut h, 1);
    h.fake_git()
        .add_repo(&workdir)
        .fail_apply_with("index locked");

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    h.settle();

    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("could not update staging: git backend failure: index locked"),
    );
}

/// The second run spans two rows, and a cursor on its last row moves the
/// whole run.
#[test]
fn staging_the_other_run_leaves_the_first_alone() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_reindent_at(&mut h, 5);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    assert_eq!(
        h.fake_git().applied_patches(&workdir),
        [REINDENT_TAIL_STAGE]
    );
}

/// The stop that the `if x {` of a staged [`REINDENT_BUFFER`] opens, taken
/// back out of the index. The reindented `let` rows go back to their old
/// indent with it, which leaves the second run staged.
const REINDENT_IF_STOP_UNSTAGE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,7 +1,6 @@\n fn f() {\n-    if x {\n-        let a = 1;\n+    let a = 1;\n-        let b = 2;\n+    let b = 2;\n         let c = 4;\n     }\n }\n";

/// Under the HEAD base a staged reindent shows as a narrowed hunk, so
/// `ToggleStageHunk` on its first run unstages that run's stop alone.
#[test]
fn unstaging_a_narrowed_hunk_moves_the_stop_under_the_cursor() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(
        &workdir,
        &[("a.rs", REINDENT_BASE, REINDENT_BUFFER, REINDENT_BUFFER)],
    );
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.rs"));
    run(&mut h, &stoat_action::DiffAgainstHead);
    h.settle_diff_jobs();
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 1);
    }

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    h.settle();

    assert_eq!(
        (
            h.fake_git().applied_patches(&workdir),
            h.stoat.pending_message.as_deref(),
        ),
        (
            vec![REINDENT_IF_STOP_UNSTAGE.to_string()],
            Some("unstaged hunk")
        ),
    );
}

/// Under the index base, the index's rename of `f` to `g` is a staged mark
/// on the row above the hunk. Deleting the first line moves the anchored
/// hunk up onto the mark's row before the next diff lands. A press there
/// still stages the hunk's first stop, not the one row the mark covers.
#[test]
fn a_stale_staged_mark_leaves_a_press_the_stop_under_it() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    let renamed = |text: &str| format!("// top\n{}", text.replace("fn f", "fn g"));
    h.stage_index_scenario(
        &workdir,
        &[(
            "a.rs",
            &format!("// top\n{REINDENT_BASE}"),
            &renamed(REINDENT_BASE),
            &renamed(REINDENT_BUFFER),
        )],
    );
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.rs"));
    run(&mut h, &stoat_action::DiffAgainstIndex);
    h.settle_diff_jobs();
    h.edit_focused(0.."// top\n".len(), "");
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 1);
    }

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    h.settle();

    assert_eq!(
        h.fake_git().applied_patches(&workdir),
        ["diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,5 +2,6 @@\n fn g() {\n+    if x {\n-    let a = 1;\n+        let a = 1;\n-    let b = 2;\n+        let b = 2;\n     let c = 3;\n }\n"],
    );
}

/// A text file gets no tree pass, so its hunk is never narrowed and `s`
/// moves all of it, the removed `c` included. A stage narrowed to the
/// cursor's row keeps `c` in the index.
#[test]
fn an_unnarrowed_hunk_still_stages_whole() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_settled_at(&mut h, "a.txt", "a\nb\nc\nd\n", "a\nX\nd\n", 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    assert_eq!(
        h.fake_git().applied_patches(&workdir),
        ["diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,4 +1,3 @@\n a\n-b\n+X\n-c\n d\n"],
    );
}

/// HEAD of a file whose first hunk turns `b` into two lines.
const SHIFT_HEAD: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";

/// The buffer over [`SHIFT_HEAD`]. Each row below the first hunk sits one
/// row lower than in HEAD, and row 7 holds the second hunk's `Y`.
const SHIFT_BUFFER: &str = "a\nZ1\nZ2\nc\nd\ne\nf\nY\nh\n";

/// The second hunk staged into an index that lacks the first. Both starts
/// name index row 4, where the context opens.
const SECOND_HUNK_STAGE: &str =
    "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -4,5 +4,5 @@\n d\n e\n f\n-g\n+Y\n h\n";

/// The second hunk unstaged from an index that holds both. The first hunk
/// moved it down a row, so both starts name index row 5.
const SECOND_HUNK_UNSTAGE: &str =
    "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -5,5 +5,5 @@\n d\n e\n f\n-Y\n+g\n h\n";

/// Press `action` on the second hunk of [`SHIFT_BUFFER`] over an index that
/// holds `index`, and return the one patch the press applied.
fn press_on_the_second_hunk(index: &str, action: &dyn Action) -> String {
    press_with_cursor(index, action, |h| {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 7);
    })
}

/// Press `action` in [`SHIFT_BUFFER`] over an index that holds `index`, with
/// the cursor `place` puts, and return the one patch the press applied.
fn press_with_cursor(
    index: &str,
    action: &dyn Action,
    place: impl FnOnce(&mut TestHarness),
) -> String {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(&workdir, &[("a.rs", SHIFT_HEAD, index, SHIFT_BUFFER)]);
    h.open_file(&workdir.join("a.rs"));
    place(&mut h);

    stoat::action_handlers::dispatch(&mut h.stoat, action);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    patches[0].clone()
}

/// libgit2 looks for a hunk exactly at its `+` start. With the first hunk
/// out of the index, the context opens at index row 4 but buffer row 5, and
/// the header names the index row on both sides.
#[test]
fn a_line_below_an_unstaged_hunk_stages_at_its_index_row() {
    assert_eq!(
        press_on_the_second_hunk(SHIFT_HEAD, &stoat_action::ToggleStageLine),
        SECOND_HUNK_STAGE
    );
}

/// The hunk key reads the index as the line key does, so it stages the same
/// patch.
#[test]
fn a_hunk_below_an_unstaged_hunk_stages_at_its_index_row() {
    assert_eq!(
        press_on_the_second_hunk(SHIFT_HEAD, &stoat_action::ToggleStageHunk),
        SECOND_HUNK_STAGE
    );
}

/// With the first hunk staged, the context opens at index row 5 as in the
/// buffer, one row lower than in HEAD. A patch diffed from HEAD names row 4
/// and misses it.
#[test]
fn a_hunk_below_a_staged_hunk_stages_at_its_index_row() {
    assert_eq!(
        press_on_the_second_hunk(
            "a\nZ1\nZ2\nc\nd\ne\nf\ng\nh\n",
            &stoat_action::ToggleStageHunk
        ),
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -5,5 +5,5 @@\n d\n e\n f\n-g\n+Y\n h\n",
    );
}

/// Escape after typing a line, `g l`, and `A` leave the block cursor on the
/// line's newline cell. Its head is the start of the next line, and the
/// press stages the line the cursor is drawn on.
#[test]
fn a_cursor_on_a_lines_last_cell_stages_that_line() {
    let patch = press_with_cursor(SHIFT_HEAD, &stoat_action::ToggleStageHunk, |h| {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let bs = snapshot.buffer_snapshot();
        let offset = bs.rope().point_to_offset(Point::new(7, 1));
        editor.selections.set_block_cursor(offset, bs);
    });
    assert_eq!(patch, SECOND_HUNK_STAGE);
}

/// A line selected with `x` heads at the start of the next line, and the
/// press stages the selected line.
#[test]
fn a_line_selection_stages_the_selected_line() {
    let patch = press_with_cursor(SHIFT_HEAD, &stoat_action::ToggleStageLine, |h| {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 7);
        h.type_keys("x");
    });
    assert_eq!(patch, SECOND_HUNK_STAGE);
}

#[test]
fn a_hunk_below_a_staged_hunk_unstages_at_its_index_row() {
    assert_eq!(
        press_on_the_second_hunk(SHIFT_BUFFER, &stoat_action::ToggleStageHunk),
        SECOND_HUNK_UNSTAGE
    );
}

#[test]
fn toggle_stage_hunk_unstages_a_staged_hunk() {
    assert_eq!(
        press_on_the_second_hunk(SHIFT_BUFFER, &stoat_action::ToggleStageHunk),
        SECOND_HUNK_UNSTAGE
    );
}

/// A cursor on a plain context row stages nothing. The gutter is empty
/// there, so reaching for the nearest hunk would stage a unit the user
/// cannot see under the cursor.
#[test]
fn a_cursor_off_every_hunk_stages_nothing() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_two_hunk_file_at(&mut h, 3);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    h.settle();

    assert_eq!(
        h.fake_git().applied_patches(&workdir),
        Vec::<String>::new(),
        "nothing was staged"
    );
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("no hunk under the cursor"),
        "and the message says why"
    );
}

/// A pure addition has no base lines of its own, so its base anchor comes
/// from the lines before it. The header has to name that line, or the
/// index apply places the hunk somewhere else.
#[test]
fn a_mid_file_addition_anchors_its_header_at_the_derived_base_line() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nb\nNEW\nc\nd\n")]);
    h.open_file(&workdir.join("a.rs"));
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 2);
    }

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    let patch = &patches[0];
    assert!(patch.contains("+NEW\n"), "adds the new line: {patch}");
    assert!(
        patch.contains("@@ -1,4 +1,5 @@"),
        "the header counts from the file start with no base line consumed: {patch}"
    );
}

/// A deletion's rows are zero-width in the buffer, so the gutter marks it
/// at its anchor row and the patch body carries only the removed lines.
#[test]
fn a_deletion_stages_from_its_anchor_row() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nGONE\nc\nd\n", "a\nb\nc\nd\n")]);
    h.open_file(&workdir.join("a.rs"));
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 2);
    }

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    let patch = &patches[0];
    assert!(patch.contains("-GONE\n"), "removes the base line: {patch}");
    assert!(
        !patch
            .lines()
            .any(|line| line.starts_with('+') && !line.starts_with("+++")),
        "and adds nothing: {patch}"
    );
}

/// The change walk lands a deletion on the row the staging keys read it
/// from, so a stage press right after the walk takes the deletion.
#[test]
fn walking_to_a_deletion_then_staging_removes_it() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nGONE\nc\nd\n", "a\nb\nc\nd\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.rs"));
    h.settle_diff_jobs();
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 0);
    }

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    assert_eq!(
        stoat::test_harness::editor::focused_cursor_point(&mut h.stoat).row,
        2,
        "the walk lands on the row after the removed line"
    );

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    assert!(
        patches[0].contains("-GONE\n"),
        "removes the base line: {}",
        patches[0]
    );
}

/// The diff view draws a deletion's removed block below the live row above
/// its seam, so a press on that row stages what a press on the seam row
/// stages. The plain pane marks the seam row itself, and a press above it
/// stages nothing.
#[test]
fn a_diff_view_press_on_the_row_above_a_deletion_stages_it() {
    let press = |diff_view: bool, row: u32| {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nGONE\nc\nd\n", "a\nb\nc\nd\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("a.rs"));
        h.settle_diff_jobs();
        if diff_view {
            stoat::action_handlers::review::enter_diff_view(&mut h.stoat);
            h.settle_diff_jobs();
        }
        {
            let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            stoat::action_handlers::movement::set_cursor_row(editor, row);
        }

        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();
        (
            h.fake_git().applied_patches(&workdir),
            h.stoat.pending_message.clone(),
        )
    };

    let at_seam = press(false, 2);
    assert_eq!(
        (
            at_seam.0.len(),
            at_seam.0.iter().all(|patch| patch.contains("-GONE\n")),
            at_seam.1.as_deref(),
        ),
        (1, true, Some("staged hunk")),
        "a press on the seam row removes the base line: {at_seam:?}",
    );
    assert_eq!(
        [press(true, 1), press(false, 1)],
        [
            at_seam,
            (Vec::new(), Some("no hunk under the cursor".to_string()))
        ],
        "the diff view press above the seam stages the deletion, the plain one nothing",
    );
}

/// The diff view moves a press onto a seam only from a row no hunk covers,
/// so the last row of a hunk above a move seam presses that hunk. A plain
/// pane keeps every row.
#[test]
fn a_press_moves_onto_a_seam_only_from_a_row_no_hunk_covers() {
    let mut h = TestHarness::with_size(80, 14);
    let path = h.write_file("a.rs", "a\nb\nc\nd\ne\nf\n");
    h.open_file(&path);
    let hunk = |status, rows: Range<u32>| DiffHunk {
        status,
        unstaged_lines: iter::once(rows.clone()).collect(),
        marked_rows: Vec::new(),
        relocated_rows: Vec::new(),
        buffer_start_line: rows.start,
        buffer_line_range: rows,
        base_byte_range: 0..0,
        anchor_range: None,
        token_detail: None,
    };
    let mut map = DiffMap::from_hunks(
        [
            hunk(DiffHunkStatus::Added, 1..3),
            hunk(DiffHunkStatus::Moved, 3..3),
            hunk(DiffHunkStatus::Deleted, 5..5),
        ],
        None,
    );
    let (_, buffer_id) = h
        .stoat
        .focused_editor_ids()
        .expect("an editor has the focus");
    {
        let ws = h.stoat.active_workspace();
        let buffer = ws.buffers.get(buffer_id).expect("the focused buffer");
        let mut guard = buffer.write().expect("buffer poisoned");
        map.anchor_hunks(&guard.snapshot);
        guard.diff_map = Some(map);
    }

    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let pressed = |diff_walk| {
        (0..6)
            .map(|row| pressed_row(&snapshot, snapshot.buffer_snapshot(), row, diff_walk))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        [pressed(true), pressed(false)],
        [vec![0, 1, 2, 3, 5, 5], vec![0, 1, 2, 3, 4, 5]],
    );
}

#[test]
fn stage_hunk_applies_the_forward_patch_for_the_cursor_hunk() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_git_file_at_cursor(&mut h, 2);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    let patch = &patches[0];
    assert!(
        patch.contains("--- a/a.rs\n+++ b/a.rs\n"),
        "targets a.rs: {patch}"
    );
    assert!(patch.contains("-c\n"), "removes the base line: {patch}");
    assert!(patch.contains("+X\n"), "adds the buffer line: {patch}");
}

/// A press on a hunk the index lacks stages it, and a press once the index
/// holds it unstages it, so two presses send the forward patch and then its
/// reverse.
#[test]
fn a_second_toggle_on_a_staged_hunk_sends_the_reverse_patch() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_git_file_at_cursor(&mut h, 2);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    h.settle();
    h.fake_git()
        .add_repo(&workdir)
        .index_file("a.rs", "a\nb\nX\nd\n");
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patch = |from: &str, to: &str| {
        format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n\
             @@ -1,4 +1,4 @@\n a\n b\n-{from}\n+{to}\n d\n"
        )
    };
    assert_eq!(
        h.fake_git().applied_patches(&workdir),
        [patch("c", "X"), patch("X", "c")],
    );
}

/// A line with a staged edit and an unstaged edit on top of it holds both
/// a forward and a reverse patch. Each toggle prefers the forward one, so a
/// partly staged hunk or line stages its rest.
#[test]
fn a_partly_staged_change_stages_its_rest() {
    const FORWARD: &str =
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n a\n-B\n+BB\n c\n";
    let pressed = |action: &dyn Action| {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_index_scenario("/repo", &[("a.rs", "a\nb\nc\n", "a\nB\nc\n", "a\nBB\nc\n")]);
        h.open_file(Path::new("/repo/a.rs"));
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 1);
        stoat::action_handlers::dispatch(&mut h.stoat, action);
        h.fake_git().applied_patches(Path::new("/repo"))
    };

    assert_eq!(
        [
            pressed(&stoat_action::ToggleStageHunk),
            pressed(&stoat_action::ToggleStageLine),
        ],
        [[FORWARD], [FORWARD]],
    );
}

/// The tree pass marks the changed call alone, so the blank row added below
/// it lies in no run. The stop the press stages covers it, so staging each
/// stop leaves nothing unstaged.
#[test]
fn stage_hunk_takes_the_blank_row_its_stop_covers() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_settled_at(
        &mut h,
        "a.rs",
        "fn a() {\n    x();\n}\n",
        "fn a() {\n    y();\n\n}\n",
        1,
    );

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    assert_eq!(
        h.fake_git().applied_patches(&workdir),
        ["diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,4 @@\n fn a() {\n-    x();\n+    y();\n+\n }\n"],
    );
}

/// A press hands the diff and the index write to the git queue. The test
/// scheduler runs that work inline, so the patch is in at the press, and
/// the status waits for the landing.
#[test]
fn a_stage_press_hands_the_index_write_to_a_worker() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_git_file_at_cursor(&mut h, 2);
    let before = h.blocking_calls();

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    assert_eq!(
        (
            h.blocking_calls() - before,
            h.fake_git().applied_patches(&workdir).len(),
            h.stoat.pending_message.as_deref()
        ),
        (1, 1, None),
        "the write ran as one blocking job, and nothing landed yet",
    );

    h.settle();
    assert_eq!(h.stoat.pending_message.as_deref(), Some("staged hunk"));
}

/// A patch that creates `rel` in the index with `hunk`.
fn creating_patch(rel: &str, hunk: &str) -> String {
    format!("diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n{hunk}")
}

/// Presses `action` on row 0 of untracked `name`, which holds `x` and `y`
/// beside a repo's one tracked change, and answers the patches the press
/// applied and the status it set.
fn press_on_untracked(name: &str, action: &dyn Action) -> (Vec<String>, Option<String>) {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/repo");
    h.stage_review_scenario(&workdir, &[("t.rs", "a\n", "b\n")]);
    h.fake_fs().insert_file(workdir.join(name), b"x\ny\n");
    h.fake_git().add_repo(&workdir).untracked(name);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join(name));
    h.settle_diff_jobs();

    run(&mut h, action);
    (
        h.fake_git().applied_patches(&workdir),
        h.stoat.pending_message.clone(),
    )
}

/// No blob holds an untracked file, so its hunk stages as a patch that
/// creates the file in the index. The tree pass narrows a Rust file's
/// added hunk to a run, which stages through the rows path, while a file
/// with no language stages its whole hunk.
#[test]
fn staging_an_untracked_file_s_hunk_adds_it_to_the_index() {
    let whole = "@@ -0,0 +1,2 @@\n+x\n+y\n";
    assert_eq!(
        [
            press_on_untracked("fresh.rs", &stoat_action::ToggleStageHunk),
            press_on_untracked("fresh.txt", &stoat_action::ToggleStageHunk),
        ],
        [
            (
                vec![creating_patch("fresh.rs", whole)],
                Some("staged hunk".to_string())
            ),
            (
                vec![creating_patch("fresh.txt", whole)],
                Some("staged hunk".to_string())
            ),
        ],
    );
}

/// A line press creates the index entry with that line alone, which leaves
/// the rest of the file an unstaged hunk.
#[test]
fn staging_an_untracked_file_s_line_adds_that_line_alone() {
    assert_eq!(
        press_on_untracked("fresh.rs", &stoat_action::ToggleStageLine),
        (
            vec![creating_patch("fresh.rs", "@@ -0,0 +1,1 @@\n+x\n")],
            Some("staged line".to_string())
        ),
    );
}

#[test]
fn unstage_hunk_reverts_the_staged_hunk_to_head() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(
        &workdir,
        &[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n", "a\nb\nX\nd\n")],
    );
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 2);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
    let patch = &patches[0];
    assert!(patch.contains("-X\n"), "removes the staged line: {patch}");
    assert!(patch.contains("+c\n"), "restores the HEAD line: {patch}");
}

/// Open `rel` under `/work`, press `action` with the cursor on `row`, and
/// return every patch the press applied.
fn press_in(h: &mut TestHarness, rel: &str, row: u32, action: &dyn Action) -> Vec<String> {
    let workdir = PathBuf::from("/work");
    h.open_file(&workdir.join(rel));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, row);

    stoat::action_handlers::dispatch(&mut h.stoat, action);
    h.fake_git().applied_patches(&workdir)
}

/// `git add` leaves a new file in the index with no HEAD blob. Its hunks
/// stage against the index text alone.
#[test]
fn a_file_only_the_index_holds_stages_its_hunk() {
    let mut h = TestHarness::with_size(80, 14);
    h.stage_review_scenario_with_staged("/work", &[], &[("new.rs", "a\nb\n")]);
    h.fake_git().add_repo("/work").index_file("new.rs", "a\n");

    assert_eq!(
        press_in(&mut h, "new.rs", 1, &stoat_action::ToggleStageHunk),
        ["diff --git a/new.rs b/new.rs\n--- a/new.rs\n+++ b/new.rs\n@@ -1,1 +1,2 @@\n a\n+b\n"],
    );
}

/// HEAD holds the file empty, so an unstage that empties the index text
/// writes an empty blob rather than removing the entry.
#[test]
fn unstaging_all_of_a_file_committed_empty_keeps_its_entry() {
    let mut h = TestHarness::with_size(80, 14);
    h.stage_index_scenario("/work", &[("a.rs", "", "x\n", "x\n")]);

    assert_eq!(
        press_in(&mut h, "a.rs", 0, &stoat_action::ToggleStageHunk),
        ["diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,0 @@\n-x\n"],
    );
}

/// A removed file opens as a lone newline. The press stages the removal
/// rather than a blob that holds one blank line.
#[test]
fn a_file_removed_from_disk_stages_its_deletion() {
    let mut h = TestHarness::with_size(80, 14);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
    h.fake_git()
        .add_repo("/work")
        .head_file("gone.rs", "a\nb\n");

    assert_eq!(
        press_in(&mut h, "gone.rs", 0, &stoat_action::ToggleStageHunk),
        ["diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +1,0 @@\n-a\n-b\n"],
    );
}

/// An unstage takes a file HEAD does not hold out of the index only when no
/// line of it stays staged, from the hunk key and the line key alike.
#[test]
fn unstaging_an_added_file_removes_it_only_when_no_line_stays() {
    let unstage = |index: &str, row: u32, action: &dyn Action| {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario_with_staged("/work", &[], &[("new.rs", index)]);
        h.fake_git().add_repo("/work").index_file("new.rs", index);
        press_in(&mut h, "new.rs", row, action)
    };
    let removal = "diff --git a/new.rs b/new.rs\ndeleted file mode 100644\n--- a/new.rs\n+++ /dev/null\n@@ -1,1 +1,0 @@\n-a\n";

    assert_eq!(
        (
            unstage("a\n", 0, &stoat_action::ToggleStageHunk),
            unstage("a\n", 0, &stoat_action::ToggleStageLine),
            unstage("a\nb\n", 1, &stoat_action::ToggleStageLine),
        ),
        (
            vec![removal.to_string()],
            vec![removal.to_string()],
            vec!["diff --git a/new.rs b/new.rs\n--- a/new.rs\n+++ b/new.rs\n@@ -1,2 +1,1 @@\n a\n-b\n".to_string()],
        ),
    );
}

/// A moved file has no HEAD blob at its own path. An unstage reverts its
/// index lines to the text HEAD holds at the path it moved from.
#[test]
fn unstaging_in_a_moved_file_reverts_to_the_text_it_moved_from() {
    let mut h = TestHarness::with_size(80, 14);
    h.stage_rename_scenario("/work", "old.rs", "new.rs", "a\nb\n", "a\nB\n");
    h.fake_git()
        .add_repo("/work")
        .index_file("new.rs", "a\nB\n");

    assert_eq!(
        press_in(&mut h, "new.rs", 1, &stoat_action::ToggleStageHunk),
        ["diff --git a/new.rs b/new.rs\n--- a/new.rs\n+++ b/new.rs\n@@ -1,2 +1,2 @@\n a\n-B\n+b\n"],
    );
}

#[test]
fn toggle_stage_hunk_stages_an_unstaged_hunk() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_git_file_at_cursor(&mut h, 2);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(
        patches.len(),
        1,
        "toggle stages the unstaged hunk: {patches:?}"
    );
    assert!(patches[0].contains("-c\n") && patches[0].contains("+X\n"));
}

#[test]
fn stage_hunk_off_a_hunk_is_a_message_only_noop() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_git_file_at_cursor(&mut h, 0);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    h.settle();

    assert!(
        h.fake_git().applied_patches(&workdir).is_empty(),
        "cursor off a hunk applies nothing"
    );
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("no hunk under the cursor")
    );
}

#[test]
fn stage_line_stages_only_the_cursor_line() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "one patch: {patches:?}");
    let patch = &patches[0];
    assert!(
        patch.contains("-b\n") && patch.contains("+B\n"),
        "stages the cursor line: {patch}"
    );
    assert!(
        !patch.contains("+C\n"),
        "leaves the other line unstaged: {patch}"
    );
    assert!(
        patch.contains(" c\n"),
        "the other line stays as context: {patch}"
    );
}

/// Staging moves the index, which is one of the two sides the gutter reads,
/// so the map computed before it no longer describes what is on screen.
#[test]
fn stage_hunk_stales_the_diff_map() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
    h.open_file(&workdir.join("a.rs"));
    h.seed_current_diff_map("a\nb\nc\nd\n");
    let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 1);
    }
    assert!(
        h.stoat.active_workspace().diff_map_current(buffer_id),
        "the map starts current, so staling it is the stage's doing",
    );

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
    h.settle();

    assert!(
        !h.stoat.active_workspace().diff_map_current(buffer_id),
        "the index moved, so the map is owed a recompute",
    );
}

/// The line funnel writes the index too, and it is a second call site, so
/// it stales the map on its own.
#[test]
fn stage_line_stales_the_diff_map() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
    h.open_file(&workdir.join("a.rs"));
    h.seed_current_diff_map("a\nb\nc\nd\n");
    let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 1);
    }
    assert!(
        h.stoat.active_workspace().diff_map_current(buffer_id),
        "the map starts current, so staling it is the stage's doing",
    );

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
    h.settle();

    assert!(
        !h.stoat.active_workspace().diff_map_current(buffer_id),
        "one staged line moves the index the same way a hunk does",
    );
}

/// A review base makes the checked-out commit the staged side, so the line
/// keys reach it through the amend transport. The index is not on screen
/// there, and an index write changes a thing the user never sees.
#[test]
fn stage_line_under_a_commit_base_amends_rather_than_writing_the_index() {
    let mut h = reviewing_a_commit("a\nP\nQ\nd\n");

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
    h.settle();

    assert_eq!(
        (
            h.fake_git().applied_patches(&PathBuf::from("/work")),
            h.stoat.pending_message.as_deref()
        ),
        (vec![], Some("amended line into the commit")),
        "the line went into the commit and the index was left alone",
    );
}

/// Unstaging takes the same route. Both keys read the base before they read
/// the cursor, so neither reaches the index while a commit is the staged
/// side.
#[test]
fn unstage_line_under_a_commit_base_amends_rather_than_writing_the_index() {
    let mut h = reviewing_a_commit("a\nX\nY\nd\n");

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
    h.settle();

    assert_eq!(
        (
            h.fake_git().applied_patches(&PathBuf::from("/work")),
            h.stoat.pending_message.as_deref()
        ),
        (vec![], Some("amended line out of the commit")),
        "the line came out of the commit and the index was left alone",
    );
}

/// A rebase stopped on an edit over commit `c1`, with `c0` as the review
/// base, and the cursor on row 1 of `a.rs`. `working` is what the buffer
/// holds, which is what separates a worktree edit from the commit's own
/// change.
fn reviewing_a_commit(working: &str) -> TestHarness {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stoat.active_workspace_mut().git_root = workdir.clone();
    h.fake_git()
        .add_repo(&workdir)
        .commit("c0", &[("a.rs", "a\nb\nc\nd\n")])
        .commit_with_parent("c1", "c0", &[("a.rs", "a\nX\nY\nd\n")])
        .head_file("a.rs", working);
    h.fake_fs().insert_file("/work/a.rs", working.as_bytes());
    {
        let ws = h.stoat.active_workspace_mut();
        ws.set_diff_base(Some(DiffBase::parent_of("c1", Some("c0".to_string()))));
        ws.rebase_active = Some(paused_rebase(&workdir));
    }
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 1);
    h
}

/// A range review over `base0`, whose `a.txt` the working file changes to
/// `working`, with the diff settled and the cursor on `row`.
///
/// A text file, so no tree pass narrows a hunk and `s` moves it whole.
fn reviewing_a_range(working: &str, row: u32) -> TestHarness {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stoat.active_workspace_mut().git_root = workdir.clone();
    h.fake_git()
        .add_repo(&workdir)
        .commit("base0", &[("a.txt", "a\nb\nc\nd\n")])
        .head_file("a.txt", working);
    h.fake_fs().insert_file("/work/a.txt", working.as_bytes());
    h.stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::review(
            "base0".to_string(),
            "feature",
            "main",
        )));
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.txt"));
    h.settle_diff_jobs();
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, row);
    h
}

/// The focused buffer's hunks as `(first row, staged)`, the staged state of
/// buffer row `row`, and the map's mark kind.
fn approval_marks(
    h: &TestHarness,
    row: u32,
) -> (Vec<(u32, bool)>, Option<MarkState>, Option<MarkKind>) {
    let id = h.stoat.focused_editor_ids().expect("editor").1;
    let buffer = h.stoat.active_workspace().buffers.get(id).expect("buffer");
    let guard = buffer.read().expect("buffer poisoned");
    let map = guard.diff_map.as_ref();
    (
        map.map(|map| {
            map.hunks()
                .map(|hunk| (hunk.buffer_start_line, hunk.staged()))
                .collect()
        })
        .unwrap_or_default(),
        map.and_then(|map| map.staged_for_line(row)),
        map.map(|map| map.mark_kind()),
    )
}

/// Under a range review the map reads approvals before the first press.
/// `s` marks the hunk under the cursor approved with no git write, a second
/// `s` takes the mark back, and a base change drops the review's rows.
#[test]
fn s_under_a_range_review_approves_the_hunk_with_no_git_write() {
    let mut h = reviewing_a_range("a\nP\nQ\nd\n", 1);
    let path = Path::new("/work/a.txt");
    let press = |h: &mut TestHarness| {
        stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle_diff_jobs();
        (h.stoat.pending_message.clone(), approval_marks(h, 1))
    };

    let before = approval_marks(&h, 1);
    let approved = press(&mut h);
    let rows = h
        .stoat
        .active_workspace()
        .approved_rows(path)
        .map(|rows| rows.covers(&(1..3)));
    let unapproved = press(&mut h);
    h.stoat.active_workspace_mut().set_diff_base(None);

    assert_eq!(
        (
            before,
            approved,
            rows,
            unapproved,
            h.fake_git().applied_patches(&PathBuf::from("/work")),
            h.stoat.active_workspace().approved_rows(path).is_none(),
        ),
        (
            (
                vec![(1, false)],
                Some(MarkState::Unapproved),
                Some(MarkKind::Approved)
            ),
            (
                Some("approved hunk".to_string()),
                (
                    vec![(1, true)],
                    Some(MarkState::Approved),
                    Some(MarkKind::Approved)
                )
            ),
            Some(true),
            (
                Some("unapproved hunk".to_string()),
                (
                    vec![(1, false)],
                    Some(MarkState::Unapproved),
                    Some(MarkKind::Approved)
                )
            ),
            Vec::<String>::new(),
            true,
        ),
    );
}

/// Under a range review `S` approves the cursor's row alone, so a two-row
/// hunk reads partly approved. `s` on a deletion approves it through the
/// row it sits above, and `s` off every hunk marks nothing.
#[test]
fn line_deletion_and_off_hunk_presses_under_a_range_review() {
    let pressed = |working: &str, row: u32, action: &dyn Action| {
        let mut h = reviewing_a_range(working, row);
        stoat::action_handlers::dispatch(&mut h.stoat, action);
        h.settle_diff_jobs();
        h
    };
    let line = pressed("a\nP\nQ\nd\n", 2, &stoat_action::ToggleStageLine);
    let deletion = pressed("a\nc\nd\n", 1, &stoat_action::ToggleStageHunk);
    let off = pressed("a\nP\nQ\nd\n", 0, &stoat_action::ToggleStageHunk);

    assert_eq!(
        (
            line.stoat.pending_message.as_deref(),
            approval_marks(&line, 2),
            approval_marks(&line, 1).1,
            approval_marks(&deletion, 1).0,
            off.stoat.pending_message.as_deref(),
            off.stoat
                .active_workspace()
                .approved_rows(Path::new("/work/a.txt"))
                .is_none(),
        ),
        (
            Some("approved line"),
            (
                vec![(1, false)],
                Some(MarkState::Approved),
                Some(MarkKind::Approved)
            ),
            Some(MarkState::Unapproved),
            vec![(1, true)],
            Some("no hunk under the cursor"),
            true,
        ),
    );
}

/// A base with nothing safe to rewrite under it gets the transport's own
/// refusal rather than the line-granularity one, since the commit is out of
/// reach for `s` as well.
#[test]
fn stage_line_reports_the_transport_refusal_below_the_tip() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
    h.fake_git().add_repo(&workdir).commit("c1", &[]);
    h.stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::parent_of("c2", Some("c1".to_string()))));
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
    h.settle();

    assert_eq!(
        (
            h.fake_git().applied_patches(&workdir),
            h.stoat.pending_message.as_deref()
        ),
        (vec![], Some(stoat::action_handlers::amend::REFUSED_BADGE)),
        "no walk and no edit stop, so the transport is closed to both key pairs",
    );
}

/// A rebase stopped on an edit, which is one of the two positions that make
/// rewriting the checked-out commit safe.
fn paused_rebase(workdir: &Path) -> stoat::rebase::ActiveRebase {
    stoat::rebase::ActiveRebase {
        workdir: workdir.to_path_buf(),
        onto: "c1".to_string(),
        remaining: std::collections::VecDeque::new(),
        total: 1,
        current_head: "c1".to_string(),
        last_pick_sha: Some("c1".to_string()),
        last_message: None,
        pause: Some(stoat::rebase::RebasePause::Edit {
            cherry_picked_commit: "c1".to_string(),
        }),
        branch: None,
    }
}

/// The line stage resolves its hunk from bare extents now, so a second hunk
/// close enough that the old chunk extraction would have merged the two is
/// absent from the patch rather than riding along as context.
#[test]
fn stage_line_ignores_a_hunk_the_old_chunking_would_have_merged() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(
        &workdir,
        &[(
            "a.rs",
            "a\nb\nc\nd\ne\nf\ng\nh\n",
            "a\nZ\nc\nd\ne\nf\nY\nh\n",
        )],
    );
    h.open_file(&workdir.join("a.rs"));
    {
        let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        stoat::action_handlers::movement::set_cursor_row(editor, 1);
    }

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "one patch: {patches:?}");
    let patch = &patches[0];
    assert!(
        patch.contains("-b\n") && patch.contains("+Z\n"),
        "stages the cursor line: {patch}"
    );
    // Context that ran into the far hunk would carry its buffer text as an
    // unchanged line, telling git the index already holds the change.
    assert!(
        !patch.contains("\n Y\n"),
        "the far hunk's text is not carried as context: {patch}"
    );
    assert!(
        patch.contains("@@ -1,5 +1,5 @@"),
        "and the patch stops well short of it: {patch}"
    );
}

#[test]
fn unstage_line_reverts_the_staged_line_to_head() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\n")]);
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "one patch: {patches:?}");
    let patch = &patches[0];
    assert!(patch.contains("-B\n"), "reverts the staged line: {patch}");
    assert!(patch.contains("+b\n"), "restores the HEAD line: {patch}");
}

/// The blank line the buffer adds above the cursor is not in the index, so
/// the cursor's index row sits one row above its buffer row.
#[test]
fn unstage_line_below_an_added_blank_line_reverts_its_own_row() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(
        &workdir,
        &[("a.rs", "a\nb\nc\nd\n", "a\nb\nc\nD\n", "a\n\nb\nc\nD\n")],
    );
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 4);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    assert_eq!(
        h.fake_git().applied_patches(&workdir),
        ["diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,4 +1,4 @@\n a\n b\n c\n-D\n+d\n"],
    );
}

#[test]
fn toggle_stage_line_stages_an_unstaged_line() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n")]);
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(
        patches.len(),
        1,
        "toggle stages the unstaged line: {patches:?}"
    );
    assert!(patches[0].contains("-b\n") && patches[0].contains("+B\n"));
}

#[test]
fn toggle_stage_line_unstages_a_staged_line() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\n")]);
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 1);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(
        patches.len(),
        1,
        "toggle unstages the staged line: {patches:?}"
    );
    assert!(patches[0].contains("-B\n") && patches[0].contains("+b\n"));
}

#[test]
fn stage_line_composes_against_the_updated_index() {
    // HEAD a/b/c/d; the index already stages line 1 (b -> B), and the
    // buffer additionally changes line 2 (c -> C). Staging line 2 diffs the
    // index (not HEAD), so the patch touches only line 2.
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(
        &workdir,
        &[("a.rs", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nC\nd\n")],
    );
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 2);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "one patch: {patches:?}");
    let patch = &patches[0];
    assert!(
        patch.contains("-c\n") && patch.contains("+C\n"),
        "stages line 2 against the index: {patch}"
    );
    assert!(
        !patch.contains("+B\n") && !patch.contains("-b\n"),
        "line 1 is already staged in the index and stays context, not re-staged: {patch}"
    );
}

#[test]
fn unstage_line_maps_the_cursor_past_an_unstaged_insertion() {
    // HEAD a/b; the index stages line 1 (b -> B); the buffer inserts NEW
    // above unstaged, so the staged line sits at buffer row 2 but index row
    // 1. Unstaging must map back and revert only the index line.
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\n", "a\nB\n", "NEW\na\nB\n")]);
    h.open_file(&workdir.join("a.rs"));
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 2);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "one patch: {patches:?}");
    let patch = &patches[0];
    assert!(patch.contains("-B\n"), "reverts the staged line: {patch}");
    assert!(patch.contains("+b\n"), "restores the HEAD line: {patch}");
    assert!(
        !patch.contains("NEW"),
        "the unstaged insertion is untouched: {patch}"
    );
}

#[test]
fn stage_line_off_a_change_is_a_message_only_noop() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_git_file_at_cursor(&mut h, 0);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
    h.settle();

    assert!(
        h.fake_git().applied_patches(&workdir).is_empty(),
        "cursor off a change applies nothing"
    );
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("no line change under the cursor")
    );
}

#[test]
fn space_h_s_stages_the_hunk_from_a_plain_editor() {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = open_git_file_at_cursor(&mut h, 2);
    // The stage keys bind once the diff holds a hunk, as it does a moment
    // after a file opens.
    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    h.settle_diff_map(buffer_id);

    h.type_keys("space h s");

    let patches = h.fake_git().applied_patches(&workdir);
    assert_eq!(patches.len(), 1, "space h s stages the hunk: {patches:?}");
    assert!(
        patches[0].contains("-c\n") && patches[0].contains("+X\n"),
        "stages the cursor hunk: {}",
        patches[0]
    );
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "the one-shot git mode returns to normal after acting"
    );
}

/// The patches `keys` applies with the cursor on the second hunk of
/// [`SHIFT_BUFFER`], over an index that holds `index`.
fn chord_on_the_second_hunk(index: &str, keys: &str) -> Vec<String> {
    let mut h = TestHarness::with_size(80, 14);
    let workdir = PathBuf::from("/work");
    h.stage_index_scenario(&workdir, &[("a.rs", SHIFT_HEAD, index, SHIFT_BUFFER)]);
    h.open_file(&workdir.join("a.rs"));
    let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
    h.settle_diff_map(buffer_id);
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 7);
    h.type_keys(keys);
    h.fake_git().applied_patches(&workdir)
}

#[test]
fn space_h_s_unstages_a_staged_hunk_from_a_plain_editor() {
    assert_eq!(
        chord_on_the_second_hunk(SHIFT_BUFFER, "space h s"),
        [SECOND_HUNK_UNSTAGE]
    );
}

#[test]
fn space_h_capital_s_toggles_the_line_from_a_plain_editor() {
    assert_eq!(
        (
            chord_on_the_second_hunk(SHIFT_HEAD, "space h S"),
            chord_on_the_second_hunk(SHIFT_BUFFER, "space h S")
        ),
        (
            vec![SECOND_HUNK_STAGE.to_string()],
            vec![SECOND_HUNK_UNSTAGE.to_string()]
        ),
        "the line key stages an unstaged line and unstages a staged one",
    );
}
