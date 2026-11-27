use std::path::{Path, PathBuf};
use stoat::{
    action_handlers::review_walk::{
        check_restored_walk, land_walk_check, queue_walk_landing, queue_walk_start, walk_landing,
        walk_navigate_as, walk_one_commit, WalkEntry, WalkFailure, WalkLandingKind,
    },
    app::Stoat,
    auto_reload,
    badge::BadgeSource,
    commit_list::Preview,
    commit_picker::CommitPickerRole,
    git_jobs,
    git_jobs::GitJobKey,
    host::FakeFsOp,
    rebase::{ActiveRebase, RebaseState},
    review_walk::{ReturnRef, ReviewWalk, WalkOrigin, WalkSpan},
    test_harness::TestHarness,
    workspace::{diff, diff::DiffBase},
};

/// The text a checkout writes into `/repo/a.rs`. The fake host moves HEAD
/// without writing the tree, so a test writes this text itself in place of
/// the checkout.
const CHECKED_OUT_A: &str = "fn a() {}\nfn a2() {}\nfn a3() {}\n";

/// A harness whose `/repo` carries three commits, `main` on the tip and
/// `feature` one commit back, with the workspace rooted there.
fn harness() -> TestHarness {
    let mut h = Stoat::test();
    h.seed_linear_history(
        "/repo",
        &[
            ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
            (
                "b2c3d4e5",
                "chore: tweak a",
                &[("a.rs", "fn a() {}\nfn a2() {}\n")],
            ),
            (
                "c3d4e5f6",
                "feat: add b.rs",
                &[("a.rs", "fn a() {}\nfn a2() {}\n"), ("b.rs", "fn b() {}\n")],
            ),
        ],
    );
    h.fake_git()
        .add_repo("/repo")
        .branch("main", "c3d4e5f6")
        .branch("feature", "b2c3d4e5")
        .set_head_branch("main");
    h.stoat.active_workspace_mut().git_root = "/repo".into();
    // The fake host moves HEAD on a checkout but does not write the tree to
    // disk, so the working tree is seeded once at the tip and stays there.
    // A walk still diffs correctly, because every base it installs is read
    // from the commit rather than from these files.
    h.fake_fs()
        .insert_file("/repo/a.rs", b"fn a() {}\nfn a2() {}\n");
    h.fake_fs().insert_file("/repo/b.rs", b"fn b() {}\n");
    h
}

/// `a.rs` as the fork leaves it.
const FORK_A: &str = "fn a() {}\nfn a2() {}\nfn fork() {}\n";

/// [`harness`]'s three commits, plus a two-commit `feature` forked from
/// `b2c3d4e5` with no `b.rs`. On it, `d4e5f6a7` changes `a.rs` and
/// `e5f6a7b8` adds `c.rs`. HEAD stays on `main` at `c3d4e5f6`, and an
/// untracked scratch file sits beside the work.
///
/// The fake's HEAD files and the tree on disk hold the tip of `feature`,
/// which is what a checkout of it leaves, since the fake moves HEAD
/// without writing the tree.
fn fork_harness() -> TestHarness {
    let h = harness();
    h.fake_git()
        .add_repo("/repo")
        .commit_with_parent_message("d4e5f6a7", "b2c3d4e5", "feat: fork", &[("a.rs", FORK_A)])
        .commit_with_parent_message(
            "e5f6a7b8",
            "d4e5f6a7",
            "feat: add c.rs",
            &[("a.rs", FORK_A), ("c.rs", "fn c() {}\n")],
        )
        .branch("feature", "e5f6a7b8")
        .set_head("c3d4e5f6")
        .head_file("a.rs", FORK_A)
        .head_file("c.rs", "fn c() {}\n")
        .untracked("0-scratch.txt");
    h.fake_fs().insert_file("/repo/a.rs", FORK_A.as_bytes());
    h.fake_fs().insert_file("/repo/c.rs", b"fn c() {}\n");
    h
}

fn diff_base(h: &TestHarness) -> Option<Option<String>> {
    match h.stoat.active_workspace().diff_base() {
        Some(DiffBase::Rev { sha, .. }) => Some(sha.clone()),
        _ => None,
    }
}

fn latched(h: &TestHarness) -> bool {
    let panes = &h.stoat.active_workspace().panes;
    panes.pane(panes.focus()).diff_mode
}

fn open_path(h: &TestHarness) -> Option<PathBuf> {
    h.stoat
        .active_workspace()
        .buffers
        .path_for(h.stoat.focused_editor_ids()?.1)
        .map(Path::to_path_buf)
}

/// The focused buffer's text, and whether it holds unsaved edits.
fn open_text(h: &TestHarness) -> (String, bool) {
    let id = h.stoat.focused_editor_ids().expect("an editor").1;
    let buffer = h.stoat.active_workspace().buffers.get(id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    (guard.snapshot.visible_text.to_string(), guard.dirty)
}

fn review_badge(h: &TestHarness) -> Option<String> {
    let ws = h.stoat.active_workspace();
    ws.badges
        .find_by_source(BadgeSource::Review)
        .and_then(|id| ws.badges.get(id))
        .map(|badge| badge.label.clone())
}

/// Opening does not walk the history on the keystroke that asked for it.
///
/// The walk reads and decodes up to a thousand commits under the repo lock,
/// which is why the picker comes up empty and fills in behind itself.
#[test]
fn opening_the_picker_leaves_the_walk_to_a_worker() {
    let mut h = harness();
    stoat::action_handlers::review_walk::git_ls(&mut h.stoat, None);

    let picker = h.stoat.commit_picker.as_ref().expect("picker is open");
    assert!(
        picker.pending_commits.is_some(),
        "the walk is out at a worker rather than already done",
    );
    assert!(picker.commits.is_empty(), "nothing has been walked yet");

    h.settle();
    let picker = h.stoat.commit_picker.as_ref().expect("picker is open");
    assert!(picker.pending_commits.is_none(), "the walk was installed");
    assert_eq!(picker.commits.len(), 3, "the seeded history arrived");
}

/// Nothing else drives the pumps after the keystroke that opened the picker.
/// Without a wake at completion, the picker stays empty until some unrelated
/// event drives them.
///
/// The query matches no row, so the walk lands on an empty selection and no
/// preview build follows it. That leaves the walk as the only thing that
/// could have woken the loop, which a preview build would otherwise mask.
#[test]
fn a_history_walk_wakes_the_run_loop_when_it_lands() {
    use futures::FutureExt;

    let mut h = harness();

    // Drain any permit standing before the open, against an Arc clone so
    // the observer never borrows `h` across settle. Notify holds at most
    // one permit, so a single drain clears it.
    let redraw = h.stoat.redraw_notify.clone();
    let _ = redraw.notified().now_or_never();

    stoat::action_handlers::review_walk::git_ls(&mut h.stoat, None);
    stoat::action_handlers::review_walk::set_picker_query(&mut h.stoat, "zzzznomatch");
    h.settle();

    assert!(
        h.stoat
            .commit_picker
            .as_ref()
            .expect("picker")
            .selected_commit()
            .is_none(),
        "nothing matched, so no preview build was spawned to wake the loop",
    );

    let notified = redraw.notified();
    tokio::pin!(notified);
    assert!(
        notified.enable(),
        "the walk should wake the loop so the list appears on its own",
    );
}

/// A walk that lands after a pump polled it asks for the frame that
/// installs it.
///
/// The walk asks for a frame before its result reaches its task, so the
/// pump in that frame sometimes finds it still running. The test scheduler
/// runs the walk at the spawn and its task only when it runs, so the pump
/// after the open always finds it running.
#[test]
fn a_walk_polled_before_it_lands_asks_for_the_frame_that_installs_it() {
    let mut h = harness();
    stoat::action_handlers::review_walk::git_ls(&mut h.stoat, None);
    stoat::action_handlers::review_walk::pump_commit_picker(&mut h.stoat);

    let asked = h.wakes_when_parked(h.stoat.redraw_notify.clone());
    stoat::action_handlers::review_walk::pump_commit_picker(&mut h.stoat);
    let walked = h
        .stoat
        .commit_picker
        .as_ref()
        .expect("picker")
        .commits
        .len();
    assert_eq!(
        (asked, walked),
        (true, 3),
        "the landed walk asks for a frame, and that frame's pump installs it",
    );
}

/// A preview that lands after a pump polled its build asks for the frame
/// that installs it, as a walk does.
#[test]
fn a_picker_preview_polled_before_it_lands_asks_for_the_frame_that_installs_it() {
    let mut h = harness();
    stoat::action_handlers::review_walk::git_ls(&mut h.stoat, None);
    h.settle();
    stoat::action_handlers::review_walk::commit_picker_step(&mut h.stoat, 1);
    let sha = h
        .stoat
        .commit_picker
        .as_ref()
        .and_then(|picker| picker.selected_commit())
        .expect("a selected commit")
        .sha
        .clone();
    stoat::action_handlers::review_walk::pump_commit_picker(&mut h.stoat);

    let asked = h.wakes_when_parked(h.stoat.redraw_notify.clone());
    stoat::action_handlers::review_walk::pump_commit_picker(&mut h.stoat);
    let picker = h.stoat.commit_picker.as_ref().expect("picker");
    assert_eq!(
        (
            asked,
            matches!(picker.preview_sessions.get(&sha), Preview::Built(_))
        ),
        (true, true),
        "the landed preview asks for a frame, and that frame's pump installs it",
    );
}

/// Drilling into a merge defers its range walk the same way, and the list
/// it brings replaces the one drilled from.
#[test]
fn drilling_into_a_merge_leaves_the_walk_to_a_worker() {
    let mut h = merged_harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();

    let before = h
        .stoat
        .commit_picker
        .as_ref()
        .expect("picker")
        .commits
        .len();
    stoat::action_handlers::review_walk::commit_picker_drill_in(&mut h.stoat);

    let picker = h.stoat.commit_picker.as_ref().expect("picker");
    assert!(
        picker.pending_commits.is_some(),
        "the range walk is out at a worker",
    );
    assert_eq!(
        picker.commits.len(),
        before,
        "the list drilled from is still showing until the walk lands",
    );

    h.settle();
    let picker = h.stoat.commit_picker.as_ref().expect("picker");
    assert!(picker.pending_commits.is_none(), "the walk was installed");
    assert_eq!(
        picker.commits.len(),
        2,
        "the merge's two branch commits replaced the list",
    );
}

/// A query typed while the walk is still running filters the list it
/// brings, rather than being dropped because it arrived first.
#[test]
fn a_query_typed_before_the_walk_lands_filters_what_arrives() {
    let mut h = harness();
    stoat::action_handlers::review_walk::git_ls(&mut h.stoat, None);
    stoat::action_handlers::review_walk::set_picker_query(&mut h.stoat, "tweak");
    h.settle();

    let picker = h.stoat.commit_picker.as_ref().expect("picker");
    assert_eq!(
        picker.filtered.len(),
        1,
        "only the commit the query names is listed",
    );
    assert_eq!(
        picker.selected_commit().map(|c| c.sha.clone()),
        Some("b2c3d4e5".to_string()),
        "and the selection sits on it",
    );
}

/// A commit that changed nothing yields no diff to build, and the picker
/// has to remember that rather than ask again.
///
/// The preview pump spawns a build when the cache has no answer for the
/// selected sha. An answer of "this commit has no diff" is still an answer:
/// dropping it leaves the pump asking for the same build on every pass,
/// which never converges.
#[test]
fn a_commit_with_no_diff_stops_the_picker_asking() {
    let mut h = Stoat::test();
    h.seed_linear_history(
        "/repo",
        &[
            ("aaaa1111", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
            (
                "bbbb2222",
                "chore: touch nothing",
                &[("a.rs", "fn a() {}\n")],
            ),
        ],
    );
    h.fake_git()
        .add_repo("/repo")
        .branch("main", "bbbb2222")
        .set_head_branch("main");
    h.stoat.active_workspace_mut().git_root = "/repo".into();

    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();

    {
        let picker = h.stoat.commit_picker.as_ref().expect("picker open");
        assert_eq!(
            picker.selected_commit().map(|c| c.sha.as_str()),
            Some("bbbb2222"),
            "the empty commit is the one selected",
        );
        assert!(
            picker.pending_preview.is_none(),
            "no build is left in flight for a commit that has no diff",
        );
        assert!(
            matches!(picker.preview_sessions.get("bbbb2222"), Preview::Empty),
            "the empty answer is cached, not left looking unbuilt",
        );
    }

    h.snapshot();
    assert!(
        h.rendered_text().contains("no changes"),
        "the preview says the commit is empty rather than that it is loading",
    );
}

#[test]
fn git_review_opens_the_picker_over_the_refs_history() {
    let mut h = harness();
    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();

    let picker = h.stoat.commit_picker.as_ref().expect("picker open");
    assert_eq!(
        picker
            .commits
            .iter()
            .map(|c| c.sha.as_str())
            .collect::<Vec<_>>(),
        ["c3d4e5f6", "b2c3d4e5", "a1b2c3d4"],
        "the walk runs newest-first from the ref tip"
    );
    assert_eq!(
        picker.selected_commit().map(|c| c.sha.as_str()),
        Some("b2c3d4e5"),
        "selection starts on the nearest branch tip that is not the ref"
    );
    assert_eq!(review_badge(&h), None);
}

#[test]
fn git_review_resolves_a_sha_as_well_as_a_branch() {
    let mut h = harness();
    h.type_text(":git review b2c3d4e5");
    h.type_keys("enter");
    h.settle();

    let picker = h.stoat.commit_picker.as_ref().expect("picker open");
    assert_eq!(
        picker
            .commits
            .iter()
            .map(|c| c.sha.as_str())
            .collect::<Vec<_>>(),
        ["b2c3d4e5", "a1b2c3d4"],
        "the walk starts at the named commit, not the branch tip"
    );
}

#[test]
fn an_unknown_revision_badges_without_opening_anything() {
    let mut h = harness();
    h.type_text(":git review junk");
    h.type_keys("enter");
    h.settle();

    assert!(h.stoat.commit_picker.is_none());
    assert_eq!(review_badge(&h).as_deref(), Some("unknown revision"));
}

#[test]
fn outside_a_repository_it_badges_too() {
    let mut h = Stoat::test();
    h.stoat.active_workspace_mut().git_root = "/elsewhere".into();
    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();

    assert!(h.stoat.commit_picker.is_none());
    assert_eq!(review_badge(&h).as_deref(), Some("not in a git repository"));
}

#[test]
fn the_picker_takes_typing_as_its_query() {
    let mut h = harness();
    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();
    assert_eq!(h.stoat.focused_mode(), "insert", "the query field is live");

    h.type_text("widget");
    h.settle();
    assert_eq!(
        h.stoat.commit_picker.as_ref().map(|p| p.filtered.len()),
        Some(0),
        "typing filters the list rather than editing the buffer behind it"
    );
}

fn picker_shas(h: &TestHarness) -> Vec<String> {
    h.stoat
        .commit_picker
        .as_ref()
        .map(|p| p.commits.iter().map(|c| c.sha.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn git_ls_browses_head_history() {
    let mut h = harness();
    h.type_text(":git log");
    h.type_keys("enter");
    h.settle();

    let picker = h.stoat.commit_picker.as_ref().expect("picker open");
    assert_eq!(picker.role, CommitPickerRole::Browse);
    assert_eq!(
        picker.selected_commit().map(|c| c.sha.as_str()),
        Some("c3d4e5f6"),
        "a browser starts at the newest commit, not a branch-tip heuristic"
    );
    assert_eq!(picker_shas(&h), ["c3d4e5f6", "b2c3d4e5", "a1b2c3d4"]);
}

/// [`harness`] with the stash list open over two stashes, `one` saved
/// before `two`.
fn stash_list_harness() -> TestHarness {
    let mut h = harness();
    let repo = h
        .stoat
        .git_host
        .discover(Path::new("/repo"))
        .expect("the repo");
    for message in ["one", "two"] {
        h.fake_git()
            .add_repo("/repo")
            .unstaged_file("a.rs", message);
        repo.stash_save(Some(message), false).expect("a stash");
    }

    h.type_text(":git sls");
    h.type_keys("enter");
    h.settle();
    h
}

/// `:git sls` lists the stashes newest first, starts on the newest, and
/// titles the modal after the command.
#[test]
fn git_stash_list_lists_the_stashes_newest_first() {
    let mut h = stash_list_harness();
    h.snapshot();
    let titled = h.rendered_text().contains(" git stash list ");

    let picker = h.stoat.commit_picker.as_ref().expect("picker open");
    let rows: Vec<(&str, &str)> = picker
        .commits
        .iter()
        .map(|row| (row.sha.as_str(), row.summary.as_str()))
        .collect();
    assert_eq!(
        (picker.role, rows, picker.selected, titled),
        (
            CommitPickerRole::Stash,
            vec![("stash-2", "On main: two"), ("stash-1", "On main: one")],
            0,
            true,
        ),
    );
}

/// Enter pops the highlighted stash alone and closes the list.
#[test]
fn enter_pops_the_highlighted_stash_and_closes_the_list() {
    let mut h = stash_list_harness();
    h.type_keys("down");
    h.type_keys("enter");
    h.settle();

    let kept: Vec<String> = h
        .stoat
        .git_host
        .discover(Path::new("/repo"))
        .expect("the repo")
        .stash_list()
        .into_iter()
        .map(|row| row.sha)
        .collect();
    assert_eq!(
        (
            h.stoat.commit_picker.is_none(),
            kept,
            h.stoat.pending_message.clone()
        ),
        (
            true,
            vec!["stash-2".to_string()],
            Some("popped stash-1 On main: one".to_string()),
        ),
    );
}

/// A stash commit's second parent is the index it saved, so the drill-in
/// key badges and the list stays as it was.
#[test]
fn drilling_into_a_stash_badges_and_keeps_the_rows() {
    let mut h = stash_list_harness();
    h.type_keys("alt-right");
    h.settle();
    assert_eq!(
        (review_badge(&h), picker_shas(&h)),
        (
            Some("a stash has no branch to drill into".to_string()),
            vec!["stash-2".to_string(), "stash-1".to_string()],
        ),
    );
}

/// The pump reading the preview task polls with a noop waker, so without a
/// wake at completion the diff below the table stays on "loading diff..."
/// until some unrelated event drives the pumps -- which is what made
/// stepping the selection read as a dead key.
#[test]
fn a_preview_build_wakes_the_run_loop_when_it_lands() {
    use futures::FutureExt;

    let mut h = harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();

    // Opening the picker wakes the loop too. Drain that permit against an
    // Arc clone, so the observer never borrows `h` across settle, leaving
    // the next build's wake as the only one to observe. Notify holds at
    // most one permit, so a single drain clears it.
    let redraw = h.stoat.redraw_notify.clone();
    let _ = redraw.notified().now_or_never();

    stoat::action_handlers::review_walk::commit_picker_step(&mut h.stoat, 1);
    h.settle();

    let notified = redraw.notified();
    tokio::pin!(notified);
    assert!(
        notified.enable(),
        "the preview build for the newly selected commit should wake the \
         loop so the diff follows the selection on its own",
    );
}

/// Rows scrolled past do not each get a build of their own.
///
/// A dropped task keeps running on the blocking pool, so one build per
/// keystroke would put every row passed through ahead of the row the
/// selection stops on, each waiting on the repo lock. Holding the count at
/// one costs nothing, since the pump comes back for the current selection
/// the moment the running build lands.
#[test]
fn stepping_the_selection_twice_leaves_one_build_running() {
    let mut h = harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();

    // Two steps with nothing polled between them, which is what holding the
    // key down does.
    stoat::action_handlers::review_walk::commit_picker_step(&mut h.stoat, 1);
    let first = h
        .stoat
        .commit_picker
        .as_ref()
        .and_then(|p| p.pending_preview.as_ref())
        .map(|p| p.sha.clone());
    assert!(
        first.is_some(),
        "the first step has to start a build, or there is nothing to block",
    );
    stoat::action_handlers::review_walk::commit_picker_step(&mut h.stoat, 1);

    let picker = h.stoat.commit_picker.as_ref().expect("picker");
    assert_eq!(
        picker.pending_preview.as_ref().map(|p| p.sha.clone()),
        first,
        "the second step waits on the build the first one started",
    );

    // Whatever it started on, it ends up showing the row it stopped at.
    h.settle();
    let picker = h.stoat.commit_picker.as_ref().expect("picker");
    let selected = picker.selected_commit().expect("selection").sha.clone();
    assert!(
        matches!(picker.preview_sessions.get(&selected), Preview::Built(_)),
        "the preview for the row the selection came to rest on is cached",
    );
}

/// A harness whose `/repo` has a two-commit `feature` branch merged back
/// into `main`, with the merge on the tip so `:git ls` opens on it.
fn merged_harness() -> TestHarness {
    let mut h = Stoat::test();
    {
        let mut repo = h.fake_git().add_repo("/repo");
        repo.commit_with_message("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")])
            .commit_with_parent_message(
                "f1111111",
                "a1b2c3d4",
                "feat: start the widget",
                &[("a.rs", "fn a() {}\n"), ("w.rs", "fn w() {}\n")],
            )
            .commit_with_parent_message(
                "f2222222",
                "f1111111",
                "feat: finish the widget",
                &[("a.rs", "fn a() {}\n"), ("w.rs", "fn w() {}\nfn w2() {}\n")],
            )
            .commit_with_parent_message(
                "b2c3d4e5",
                "a1b2c3d4",
                "chore: tweak a",
                &[("a.rs", "fn a() {}\nfn a2() {}\n")],
            )
            .merge_commit(
                "m9999999",
                "f2222222",
                &[
                    ("a.rs", "fn a() {}\nfn a2() {}\n"),
                    ("w.rs", "fn w() {}\nfn w2() {}\n"),
                ],
            )
            .branch("main", "m9999999")
            .set_head_branch("main");
    }
    h.stoat.active_workspace_mut().git_root = "/repo".into();
    h
}

/// Every row `:git ls` lists over [`merged_harness`], newest seeded first.
/// The browser walks the whole DAG, so the merged branch is in here.
const MERGED_DAG: [&str; 5] = ["m9999999", "b2c3d4e5", "f2222222", "f1111111", "a1b2c3d4"];

fn picker_query(h: &TestHarness) -> String {
    h.stoat
        .commit_picker
        .as_ref()
        .map(|p| p.input.text(h.stoat.active_workspace()))
        .unwrap_or_default()
}

/// Shas of the rows the query actually leaves showing, as opposed to
/// [`picker_shas`], which reports the whole scope regardless of filtering.
fn picker_filtered_shas(h: &TestHarness) -> Vec<String> {
    h.stoat
        .commit_picker
        .as_ref()
        .map(|p| {
            p.filtered
                .iter()
                .map(|&idx| p.commits[idx].sha.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn browsing_lists_merged_branches_while_a_base_pick_does_not() {
    let mut h = merged_harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();
    assert_eq!(
        picker_shas(&h),
        ["m9999999", "b2c3d4e5", "f2222222", "f1111111", "a1b2c3d4"],
        "a browser walks the real DAG, so the merged branch is listed"
    );

    h.type_keys("escape");
    h.settle();
    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();
    assert_eq!(
        picker_shas(&h),
        ["m9999999", "b2c3d4e5", "a1b2c3d4"],
        "a base pick keeps first-parent history, which the review walks"
    );
}

#[test]
fn drilling_a_merge_lists_only_the_branch_it_brought_in() {
    let mut h = merged_harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();
    assert_eq!(picker_shas(&h), MERGED_DAG);

    h.type_keys("alt-right");
    h.settle();

    assert_eq!(
        picker_shas(&h),
        ["f2222222", "f1111111"],
        "the merged branch's own commits, without the mainline"
    );
    let picker = h.stoat.commit_picker.as_ref().expect("picker open");
    assert_eq!(picker.scope_label.as_deref(), Some("merge m999999"));
    assert_eq!(picker.selected, 0);
}

#[test]
fn leaving_a_drilled_scope_restores_the_list_and_the_query() {
    let mut h = merged_harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();

    h.type_text("9999");
    h.settle();
    assert_eq!(
        picker_filtered_shas(&h),
        ["m9999999"],
        "narrowed to the merge"
    );

    h.type_keys("alt-right");
    h.settle();
    assert_eq!(picker_shas(&h), ["f2222222", "f1111111"]);
    assert_eq!(picker_query(&h), "", "a drilled scope opens unfiltered");
    assert_eq!(picker_filtered_shas(&h), ["f2222222", "f1111111"]);

    h.type_keys("down");
    h.settle();
    h.type_keys("alt-left");
    h.settle();

    assert_eq!(picker_shas(&h), MERGED_DAG);
    assert_eq!(picker_query(&h), "9999", "the query typed in that scope");
    assert_eq!(
        picker_filtered_shas(&h),
        ["m9999999"],
        "and the list it was narrowing"
    );
    assert_eq!(
        h.stoat
            .commit_picker
            .as_ref()
            .and_then(|p| p.scope_label.clone()),
        None,
        "back at the root, titled by the role again"
    );
}

/// Back before a drill lands cancels the drill, so the walk never arrives
/// to strand the reader in a scope with an empty query.
#[test]
fn a_back_press_while_a_drill_walks_cancels_the_drill() {
    let mut h = merged_harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();
    h.type_text("9999");
    h.settle();

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::CommitPickerDrillIn);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::CommitPickerBack);
    h.settle();

    let picker = h.stoat.commit_picker.as_ref().expect("picker open");
    assert_eq!(
        (
            picker_shas(&h),
            picker_query(&h),
            picker.scope_label.clone(),
            picker.pending_commits.is_none()
        ),
        (
            MERGED_DAG.map(String::from).to_vec(),
            "9999".to_owned(),
            None,
            true
        ),
        "back cancels the drill, and the root list keeps its query"
    );
}

#[test]
fn drilling_a_row_that_is_not_a_merge_badges_and_leaves_the_list_alone() {
    let mut h = merged_harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();

    // Filtering by sha rather than by summary word, because the fuzzy
    // matcher runs over the whole joined row and a word like "tweak" is a
    // subsequence of several of them.
    h.type_text("b2c3d4");
    h.settle();
    assert_eq!(picker_filtered_shas(&h), ["b2c3d4e5"], "one-parent commit");

    h.type_keys("alt-right");
    h.settle();

    assert_eq!(review_badge(&h).as_deref(), Some("not a merge commit"));
    assert_eq!(picker_shas(&h), MERGED_DAG);
    assert_eq!(picker_query(&h), "b2c3d4", "the query survives the refusal");
}

#[test]
fn leaving_the_outermost_scope_does_nothing() {
    let mut h = merged_harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();

    h.type_keys("alt-left");
    h.settle();

    assert_eq!(picker_shas(&h), MERGED_DAG);
    assert!(h.stoat.commit_picker.is_some(), "the picker stays open");
}

#[test]
fn picking_inside_a_drilled_scope_walks_the_drilled_commits() {
    let mut h = merged_harness();
    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();

    h.type_keys("alt-right");
    h.settle();
    assert_eq!(picker_shas(&h), ["f2222222", "f1111111"]);

    h.type_keys("down enter");
    h.settle();

    let walk = h
        .stoat
        .active_workspace()
        .review_walk
        .as_ref()
        .expect("walk installed");
    assert_eq!(
        walk.commits
            .iter()
            .map(|c| c.sha.as_str())
            .collect::<Vec<_>>(),
        ["f1111111", "f2222222"],
        "the walk covers the drilled branch, oldest first"
    );
}

#[test]
fn space_h_l_opens_the_browser() {
    let mut h = harness();
    h.type_keys("space h l");
    h.settle();

    assert_eq!(
        h.stoat.commit_picker.as_ref().map(|p| p.role),
        Some(CommitPickerRole::Browse),
    );
    assert_eq!(
        h.stoat.focused_mode(),
        "insert",
        "the chord releases into the picker's query field"
    );

    h.type_keys("escape");
    h.settle();
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "closing the picker leaves normal mode, not the space_git chord"
    );
}

#[test]
fn selecting_while_browsing_changes_nothing() {
    let mut h = harness();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();

    h.type_keys("down");
    h.type_keys("enter");
    h.settle();

    assert!(h.stoat.commit_picker.is_none(), "the picker dismisses");
    assert!(
        h.stoat.active_workspace().review_walk.is_none(),
        "browsing installs no walk"
    );
    assert!(
        checkouts(&h).is_empty(),
        "browsing never touches the working tree"
    );
}

#[test]
fn git_ls_outside_a_repository_badges() {
    let mut h = Stoat::test();
    h.stoat.active_workspace_mut().git_root = "/elsewhere".into();
    h.type_text(":git ls");
    h.type_keys("enter");
    h.settle();

    assert!(h.stoat.commit_picker.is_none());
    assert_eq!(review_badge(&h).as_deref(), Some("not in a git repository"));
}

#[test]
fn git_ls_with_a_rev_roots_the_list_there_and_says_so() {
    let mut h = harness();
    h.type_text(":git ls feature");
    h.type_keys("enter");
    h.settle();

    let picker = h.stoat.commit_picker.as_ref().expect("picker open");
    assert_eq!(picker.role, CommitPickerRole::Browse);
    assert_eq!(
        picker_shas(&h),
        ["b2c3d4e5", "a1b2c3d4"],
        "the branch's history, not HEAD's"
    );
    assert_eq!(picker.scope_label.as_deref(), Some("git log feature"));
}

#[test]
fn git_ls_with_an_unknown_rev_badges_without_opening_anything() {
    let mut h = harness();
    h.type_text(":git ls junk");
    h.type_keys("enter");
    h.settle();

    assert!(h.stoat.commit_picker.is_none());
    assert_eq!(review_badge(&h).as_deref(), Some("unknown revision"));
}

/// A branch name attaches HEAD to the branch, any other revision detaches
/// HEAD at its commit, and a name that resolves to nothing moves nothing.
#[test]
fn git_checkout_attaches_a_branch_and_detaches_any_other_revision() {
    let check_out = |text: &str| {
        let mut h = harness();
        h.type_text(text);
        h.type_keys("enter");
        h.settle();
        (
            checkouts(&h),
            head_branch(&h),
            h.stoat.pending_message.clone(),
            review_badge(&h),
        )
    };
    assert_eq!(
        [
            check_out(":git checkout feature"),
            check_out(":git co a1b2c3d4"),
            check_out(":git co junk"),
        ],
        [
            (
                vec!["ref:feature".to_string()],
                Some("feature".to_string()),
                Some("checked out feature".to_string()),
                None,
            ),
            (
                vec!["detached:a1b2c3d4".to_string()],
                None,
                Some("HEAD detached at a1b2c3d".to_string()),
                None,
            ),
            (
                Vec::new(),
                Some("main".to_string()),
                None,
                Some("unknown revision".to_string()),
            ),
        ],
    );
}

/// A review walk or a rebase owns HEAD until it ends, so a checkout during
/// either one moves nothing.
#[test]
fn git_checkout_during_a_walk_or_a_rebase_moves_nothing() {
    let refused = |h: &mut TestHarness| {
        let before = checkouts(h);
        h.type_text(":git checkout feature");
        h.type_keys("enter");
        h.settle();
        (checkouts(h) == before, review_badge(h))
    };
    let mut walking = harness();
    start_walk(&mut walking);
    let mut rebasing = harness();
    rebasing.stoat.active_workspace_mut().rebase_active = Some(ActiveRebase::new(
        RebaseState::new("/repo".into(), "a1b2c3d4".into(), Vec::new()),
        None,
    ));
    assert_eq!(
        [refused(&mut walking), refused(&mut rebasing)],
        [
            (true, Some("a review walk is open".to_string())),
            (true, Some("a rebase is in progress".to_string())),
        ],
    );
}

/// The checkout moves HEAD under the diffs, so each one recomputes against
/// the new HEAD, even over a file whose text on disk stays the same.
#[test]
fn git_checkout_rediffs_against_the_new_head() {
    let mut h = harness();
    h.fake_git()
        .add_repo("/repo")
        .head_file("a.rs", "fn a() {}\nfn a2() {}\n");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/a.rs"));
    h.settle_diff_jobs();
    let buffer_id = h.stoat.focused_editor_ids().expect("an editor").1;
    let base = |h: &TestHarness| {
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("poisoned");
        let map = guard.diff_map.as_ref().expect("a diff map");
        map.base_text().map(|text| text.to_string())
    };
    let before = base(&h);

    // The fake host moves HEAD without its tree, so the test moves the tree.
    h.fake_git()
        .add_repo("/repo")
        .head_file("a.rs", "fn a() {}\n");
    h.type_text(":git co a1b2c3d4");
    h.type_keys("enter");
    h.settle();
    h.settle_diff_jobs();
    assert_eq!(
        (before.as_deref(), base(&h).as_deref()),
        (Some("fn a() {}\nfn a2() {}\n"), Some("fn a() {}\n")),
    );
}

/// The checkout rewrites the tree under the open buffers, so they show it
/// rather than the commit HEAD left.
#[test]
fn git_checkout_re_reads_open_files() {
    let mut h = harness();
    h.open_file(Path::new("/repo/a.rs"));
    h.fake_fs().insert_file("/repo/a.rs", CHECKED_OUT_A);
    h.type_text(":git checkout feature");
    h.type_keys("enter");
    h.settle();
    assert_eq!(open_text(&h), (CHECKED_OUT_A.to_string(), false));
}

/// Every commits list reloads from the HEAD the checkout left.
#[test]
fn git_checkout_reloads_the_commits_lists() {
    let mut h = harness();
    h.open_commits("/repo");
    h.type_text(":git checkout feature");
    h.type_keys("enter");
    h.settle();
    let listed: Vec<String> = h
        .stoat
        .active_workspace()
        .focused_commits()
        .expect("a commits list")
        .commits
        .iter()
        .map(|commit| commit.sha.clone())
        .collect();
    assert_eq!(listed, ["b2c3d4e5", "a1b2c3d4"]);
}

fn checkouts(h: &TestHarness) -> Vec<String> {
    h.fake_git().checkouts(Path::new("/repo"))
}

fn head_branch(h: &TestHarness) -> Option<String> {
    h.stoat
        .git_host
        .discover(Path::new("/repo"))
        .and_then(|repo| repo.head_branch())
}

fn walk_shas(h: &TestHarness) -> Vec<String> {
    h.stoat
        .active_workspace()
        .review_walk
        .as_ref()
        .map(|w| w.commits.iter().map(|c| c.sha.clone()).collect())
        .unwrap_or_default()
}

fn walk_cursor(h: &TestHarness) -> Option<usize> {
    h.stoat
        .active_workspace()
        .review_walk
        .as_ref()
        .map(|w| w.cursor)
}

/// Open the picker over `main` and select the oldest commit as the base.
fn start_walk(h: &mut TestHarness) {
    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();
    h.type_keys("down");
    h.settle();
    assert_eq!(
        h.stoat
            .commit_picker
            .as_ref()
            .and_then(|p| p.selected_commit())
            .map(|c| c.sha.as_str()),
        Some("a1b2c3d4"),
    );
    h.type_keys("enter");
    h.settle();
}

#[test]
fn selecting_a_base_installs_the_walk_oldest_first() {
    let mut h = harness();
    start_walk(&mut h);

    assert!(h.stoat.commit_picker.is_none(), "the picker closes");
    assert_eq!(walk_shas(&h), ["a1b2c3d4", "b2c3d4e5", "c3d4e5f6"]);
    assert_eq!(walk_cursor(&h), Some(0));
    assert_eq!(checkouts(&h), ["detached:a1b2c3d4"]);
}

/// The base is the commit's parent, so what the reader sees is the commit.
/// A root commit has no parent, and the empty tree is the base that makes
/// every one of its lines read as added rather than as nothing at all.
#[test]
fn a_walk_lands_on_the_root_commits_file_against_the_empty_tree() {
    let mut h = harness();
    start_walk(&mut h);

    assert_eq!(
        (diff_base(&h), latched(&h), open_path(&h)),
        (Some(None), true, Some(PathBuf::from("/repo/a.rs"))),
    );
}

/// Stepping re-points the base at the new commit's parent and opens what
/// that commit changed, which is not the file the previous step left open.
#[test]
fn a_step_opens_what_the_new_commit_changed() {
    let mut h = harness();
    start_walk(&mut h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(
        (diff_base(&h), open_path(&h)),
        (
            Some(Some("a1b2c3d4".to_string())),
            Some(PathBuf::from("/repo/a.rs"))
        ),
        "the middle commit touched a.rs",
    );

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(
        (diff_base(&h), latched(&h), open_path(&h)),
        (
            Some(Some("b2c3d4e5".to_string())),
            true,
            Some(PathBuf::from("/repo/b.rs"))
        ),
        "the tip commit added b.rs, so the walk crosses to it",
    );
}

/// A landing names the commit's parent on the left and the commit on the
/// right. A root commit's parent is the empty tree.
#[test]
fn a_landing_names_the_parent_and_the_commit() {
    let mut h = harness();
    start_walk(&mut h);
    let root = diff::diff_sides(h.stoat.active_workspace().diff_base());

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    let child = diff::diff_sides(h.stoat.active_workspace().diff_base());

    assert_eq!(
        [root, child],
        [
            ("empty", "commit a1b2c3d"),
            ("parent a1b2c3d", "commit b2c3d4e")
        ]
        .map(|(left, right)| (left.to_string(), right.to_string())),
    );
}

/// A step onto a file the reader already has open shows the text the
/// checkout wrote. The open hands back the buffer the file already has, so
/// without a re-read the step shows the last commit's text against the new
/// base.
#[test]
fn a_step_re_reads_an_open_file_the_checkout_rewrote() {
    let mut h = harness();
    start_walk(&mut h);
    h.fake_fs().insert_file("/repo/a.rs", CHECKED_OUT_A);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(
        (open_path(&h), open_text(&h)),
        (
            Some(PathBuf::from("/repo/a.rs")),
            (CHECKED_OUT_A.to_string(), false)
        ),
        "the step lands on a.rs with the checked-out text, clean",
    );
}

/// A checkout rewrites only the files that differ between its two commits,
/// so a landing reads those and stats the rest.
#[test]
fn a_checkout_landing_reads_only_files_whose_mtime_moved() {
    let mut h = harness();
    h.open_file(Path::new("/repo/b.rs"));
    h.open_file(Path::new("/repo/a.rs"));
    h.fake_fs().insert_file("/repo/a.rs", CHECKED_OUT_A);
    let before = h.fake_fs().ops().len();

    auto_reload::reload_clean_buffers(&mut h.stoat);
    let reads: Vec<FakeFsOp> = h.fake_fs().ops()[before..]
        .iter()
        .filter(|op| matches!(op, FakeFsOp::Read { .. }))
        .cloned()
        .collect();
    assert_eq!(
        (reads, open_text(&h)),
        (
            vec![FakeFsOp::Read {
                path: PathBuf::from("/repo/a.rs")
            }],
            (CHECKED_OUT_A.to_string(), false)
        ),
        "only the rewritten file reads, and a.rs holds its new text, clean",
    );
}

/// The focused cursor as a `(row, column)` pair.
fn cursor_cell(h: &mut TestHarness) -> (u32, u32) {
    let (id, offset) = h.stoat.focused_cursor_pos().expect("a cursor");
    let buffer = h.stoat.active_workspace().buffers.get(id).expect("buffer");
    let point = buffer
        .read()
        .expect("poisoned")
        .rope()
        .offset_to_point(offset);
    (point.row, point.column)
}

/// The step's file holds one change, rows 1 and 2. The cursor waits inside
/// it at column 3, a cell the reload keeps, so only a step that reads from
/// the top moves it onto the change's first cell.
#[test]
fn a_step_reads_its_file_from_the_first_change() {
    let mut h = harness();
    start_walk(&mut h);
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("an editor");
    stoat::action_handlers::movement::set_cursor_row(editor, 1);
    h.type_keys("l l l");
    let before = cursor_cell(&mut h);
    h.fake_fs().insert_file("/repo/a.rs", CHECKED_OUT_A);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(
        (before, cursor_cell(&mut h)),
        ((1, 3), (1, 0)),
        "the step lands on the first cell of the file's first change",
    );
}

/// Edits the reader has not saved outlive a step, since only the reader
/// decides to discard them.
#[test]
fn a_landing_leaves_an_unsaved_buffer_alone() {
    let mut h = harness();
    start_walk(&mut h);
    h.edit_focused(0..0, "// note\n");
    h.fake_fs().insert_file("/repo/a.rs", CHECKED_OUT_A);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(
        open_text(&h),
        ("// note\nfn a() {}\nfn a2() {}\n".to_string(), true),
        "the unsaved edit survives the step",
    );
}

/// The badge is the only thing naming where a walk stands, since the view
/// it lands on is the ordinary diff view rather than a screen of its own.
#[test]
fn the_badge_names_the_commit_and_the_position() {
    let mut h = harness();
    start_walk(&mut h);
    assert_eq!(review_badge(&h).as_deref(), Some("reviewing a1b2c3d (1/3)"));

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(review_badge(&h).as_deref(), Some("reviewing b2c3d4e (2/3)"));
}

/// Drive a walk without the commit picker that normally installs one.
///
/// The picker spawns a preview build per selected commit, and a commit
/// whose diff is empty is exactly what it cannot cache, so opening the
/// picker over a history holding one never settles. That is a defect in the
/// picker rather than in the walk, and the walk is what this covers.
fn seed_walk(h: &mut TestHarness, shas: &[&str]) {
    let walk = walk_over(h, shas);
    h.stoat.active_workspace_mut().review_walk = Some(walk);
    walk_navigate_as(&mut h.stoat, WalkLandingKind::Walk);
    h.settle();
}

/// A walk over `shas` in `/repo` that returns to `main`, built but not yet
/// installed.
fn walk_over(h: &TestHarness, shas: &[&str]) -> ReviewWalk {
    let repo = h.stoat.git_host.discover(Path::new("/repo")).expect("repo");
    let commits = shas
        .iter()
        .map(|sha| {
            repo.log_from(sha, 1)
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("no commit {sha}"))
        })
        .collect();
    ReviewWalk {
        workdir: PathBuf::from("/repo"),
        commits,
        cursor: 0,
        return_ref: ReturnRef::Branch("main".to_string()),
        span: WalkSpan::Commits,
        origin: WalkOrigin::Commits,
        stash: None,
    }
}

/// A commit that changed nothing has no file to land on. The base still
/// follows the tree onto it, so the view left up from the last step reads
/// against the right commit rather than against a stale one.
#[test]
fn an_empty_commit_moves_the_base_and_leaves_the_view_alone() {
    let mut h = Stoat::test();
    h.seed_linear_history(
        "/repo",
        &[
            ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
            ("b2c3d4e5", "chore: nothing", &[("a.rs", "fn a() {}\n")]),
        ],
    );
    h.stoat.active_workspace_mut().git_root = "/repo".into();
    h.fake_fs().insert_file("/repo/a.rs", b"fn a() {}\n");
    seed_walk(&mut h, &["a1b2c3d4", "b2c3d4e5"]);

    let landed = open_path(&h);
    assert_eq!(landed, Some(PathBuf::from("/repo/a.rs")), "the root landed");

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(
        (diff_base(&h), open_path(&h), review_badge(&h)),
        (
            Some(Some("a1b2c3d4".to_string())),
            landed,
            Some("reviewing b2c3d4e (2/2)".to_string())
        ),
        "the base moved to the empty commit's parent and the view stayed",
    );
}

/// A walk that opens on an empty commit has nothing to show, so it shows
/// nothing. Arming the view regardless would widen the pane over whichever
/// buffer happened to be focused and call it the commit's diff.
#[test]
fn a_walk_opening_on_an_empty_commit_arms_nothing() {
    let mut h = Stoat::test();
    h.seed_linear_history(
        "/repo",
        &[
            ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
            ("b2c3d4e5", "chore: nothing", &[("a.rs", "fn a() {}\n")]),
        ],
    );
    h.stoat.active_workspace_mut().git_root = "/repo".into();
    h.fake_fs().insert_file("/repo/a.rs", b"fn a() {}\n");
    seed_walk(&mut h, &["b2c3d4e5"]);

    assert_eq!(
        (diff_base(&h), latched(&h), review_badge(&h)),
        (
            Some(Some("a1b2c3d4".to_string())),
            false,
            Some("reviewing b2c3d4e (1/1)".to_string())
        ),
        "the base tracks the commit but no view opened over it",
    );
}

/// The badge names the commit the walk stands on, so once the return lands
/// it names a tree nobody is on.
#[test]
fn review_done_takes_the_walks_badge_down() {
    let mut h = harness();
    start_walk(&mut h);
    assert_eq!(review_badge(&h).as_deref(), Some("reviewing a1b2c3d (1/3)"));

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();
    assert_eq!(review_badge(&h), None);
}

/// Ending the walk puts the diff back where the tree goes: off the commit
/// and onto the working tree's own base.
#[test]
fn review_done_clears_the_base_and_the_latch() {
    let mut h = harness();
    start_walk(&mut h);
    assert!(latched(&h), "the walk armed the view");

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();
    assert_eq!(
        (diff_base(&h), latched(&h)),
        (None, false),
        "the base and the latch went with the walk",
    );
}

/// The return checks the starting ref out under the open buffers, so they
/// show the tree it restored rather than the last walked commit.
#[test]
fn a_return_re_reads_open_files() {
    let mut h = harness();
    start_walk(&mut h);
    h.fake_fs().insert_file("/repo/a.rs", CHECKED_OUT_A);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();
    assert_eq!(
        (open_path(&h), open_text(&h)),
        (
            Some(PathBuf::from("/repo/a.rs")),
            (CHECKED_OUT_A.to_string(), false)
        ),
        "a.rs shows the restored tree, clean",
    );
}

/// A step's checkout is sometimes still out when the walk ends. The return
/// waits behind it, and the step's landing finds no walk, so it moves
/// neither the base nor the latch the return clears.
#[test]
fn review_done_during_a_step_checkout_leaves_no_base_behind() {
    let mut h = harness();
    start_walk(&mut h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    let at_press = checkouts(&h);
    h.settle();

    assert_eq!(
        (
            at_press,
            checkouts(&h),
            diff_base(&h),
            latched(&h),
            h.stoat.active_workspace().review_walk.is_none()
        ),
        (
            vec![
                "detached:a1b2c3d4".to_string(),
                "detached:b2c3d4e5".to_string()
            ],
            vec![
                "detached:a1b2c3d4".to_string(),
                "detached:b2c3d4e5".to_string(),
                "ref:main".to_string()
            ],
            None,
            false,
            true
        ),
        "the return waits for the step, whose landing moves nothing",
    );
}

/// A step that waits behind another git job when the walk ends never runs.
/// No checkout after the return then leaves the tree on the walked commit.
#[test]
fn review_done_drops_a_step_still_waiting_its_turn() {
    let mut h = harness();
    start_walk(&mut h);

    git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();

    assert_eq!(
        (
            checkouts(&h),
            h.stoat.active_workspace().review_walk.is_none()
        ),
        (
            vec!["detached:a1b2c3d4".to_string(), "ref:main".to_string()],
            true
        ),
        "the tree ends where the walk started",
    );
}

/// A return that fails leaves the tree at the commit, so the base has to
/// keep naming that commit's parent. Dropping it would leave the statusline
/// claiming a base the files on disk are not measured against.
#[test]
fn a_failed_return_keeps_the_base() {
    let mut h = harness();
    h.fake_git().add_repo("/repo").set_head_branch("gone");
    start_walk(&mut h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();
    assert_eq!(
        (diff_base(&h), latched(&h)),
        (Some(None), true),
        "the view still reads against the commit the tree is at",
    );
}

#[test]
fn the_walk_spans_history_the_query_hid() {
    let mut h = harness();
    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();
    h.type_text("a1b2");
    h.settle();
    assert_eq!(
        h.stoat.commit_picker.as_ref().map(|p| p.filtered.len()),
        Some(1),
        "the sha query hides the two newer commits"
    );

    h.type_keys("enter");
    h.settle();
    assert_eq!(
        walk_shas(&h),
        ["a1b2c3d4", "b2c3d4e5", "c3d4e5f6"],
        "the filtered-out commits are still walked through"
    );
}

#[test]
fn stepping_checks_each_commit_out_and_wraps_at_the_tip() {
    let mut h = harness();
    start_walk(&mut h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(walk_cursor(&h), Some(2));
    assert_eq!(
        checkouts(&h),
        [
            "detached:a1b2c3d4",
            "detached:b2c3d4e5",
            "detached:c3d4e5f6"
        ]
    );

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    assert_eq!(
        (walk_cursor(&h), checkouts(&h)),
        (
            Some(0),
            vec![
                "detached:a1b2c3d4".to_string(),
                "detached:b2c3d4e5".to_string(),
                "detached:c3d4e5f6".to_string(),
                "detached:a1b2c3d4".to_string(),
            ]
        ),
        "a step past the tip wraps to the base and checks it out"
    );
}

/// A checkout walks the tree and writes files, and the dirty guard ahead of
/// it walks the whole status. On a large repository that is most of a
/// second, and a step used to spend all of it where the key was pressed.
///
/// What the press does now is hand the work off. The test executor runs a
/// blocking job inline, so the checkout itself is not what shows the split.
/// The landing is: it waits for the pump, and the diff base holds still
/// until then.
#[test]
fn a_step_leaves_the_checkout_to_a_worker() {
    let mut h = harness();
    start_walk(&mut h);
    let before = diff_base_sha(&h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);

    assert_eq!(
        (
            h.stoat.git_jobs.holds(GitJobKey::WalkLanding),
            diff_base_sha(&h)
        ),
        (true, before.clone()),
        "the press armed the landing and applied none of it",
    );

    h.settle();
    assert_eq!(
        (
            h.stoat.git_jobs.holds(GitJobKey::WalkLanding),
            diff_base_sha(&h)
        ),
        (false, Some("a1b2c3d4".to_string())),
        "and the pump applied it once the job landed",
    );
    assert_ne!(diff_base_sha(&h), before, "the base moved with the step");
}

/// An edit pause checks out the commit the rebase stopped on, which the
/// walk cursor does not name. Its landing leaves the tree there and does
/// not check the cursor's commit out.
#[test]
fn an_edit_pause_landing_starts_no_walk_checkout() {
    let mut h = harness();
    seed_walk(&mut h, &["a1b2c3d4", "b2c3d4e5"]);

    queue_walk_landing(
        &mut h.stoat,
        WalkLandingKind::EditPause,
        PathBuf::from("/repo"),
        "c3d4e5f6".to_string(),
        "editing c3d4e5f, C continues".to_string(),
    );
    h.settle();

    assert_eq!(
        (checkouts(&h), diff_base(&h)),
        (
            vec![
                "detached:a1b2c3d4".to_string(),
                "detached:c3d4e5f6".to_string()
            ],
            Some(Some("b2c3d4e5".to_string()))
        ),
        "the tree and the base stay on the paused commit"
    );
}

/// The sha the workspace diffs against, which a landed step moves.
fn diff_base_sha(h: &TestHarness) -> Option<String> {
    match h.stoat.active_workspace().diff_base() {
        Some(DiffBase::Rev { sha, .. }) => sha.clone(),
        _ => None,
    }
}

#[test]
fn stepping_back_walks_toward_the_base() {
    let mut h = harness();
    start_walk(&mut h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewPrevCommit);
    h.settle();
    assert_eq!(walk_cursor(&h), Some(0));

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewPrevCommit);
    h.settle();
    assert_eq!(
        (walk_cursor(&h), checkouts(&h).last().map(String::as_str)),
        (Some(2), Some("detached:c3d4e5f6")),
        "a step past the base wraps to the tip"
    );
}

#[test]
fn a_dirty_tree_refuses_to_start_the_walk() {
    let mut h = harness();
    h.fake_git()
        .add_repo("/repo")
        .modified("a.rs", "fn a() {}\n", "fn a() { edited }\n");

    start_walk(&mut h);

    assert!(h.stoat.active_workspace().review_walk.is_none());
    assert_eq!(review_badge(&h).as_deref(), Some("uncommitted changes"));
    assert!(checkouts(&h).is_empty(), "nothing was checked out");
    assert!(
        h.stoat.commit_picker.is_some(),
        "the picker stays open so the base is not lost"
    );
}

/// A stashing landing whose checkout fails puts the stash back, so the tree
/// holds its changes again and the stash list holds nothing.
#[test]
fn a_stashing_landing_pops_the_stash_back_when_its_checkout_fails() {
    let h = harness();
    h.fake_git()
        .add_repo("/repo")
        .unstaged_file("a.rs", "fn a() { 1 }\n")
        .untracked("notes.md");
    let repo = h.stoat.git_host.discover(Path::new("/repo")).expect("repo");
    let before = repo.changed_files();
    let entry = WalkEntry {
        open: None,
        stash: Some("wip".to_string()),
    };

    let landed = walk_landing(
        &*repo,
        WalkLandingKind::Walk,
        Path::new("/repo"),
        "f0f0f0f0",
        None,
        entry,
    );
    let detail = match landed {
        Err(WalkFailure::Checkout(detail)) => Some(detail),
        _ => None,
    };
    assert_eq!(
        (detail, repo.changed_files(), repo.stash_list().len()),
        (
            Some("git backend failure: unknown sha: f0f0f0f0".to_string()),
            before,
            0,
        ),
    );
}

/// A stashing landing over a clean tree finds nothing to stash and checks
/// out all the same.
#[test]
fn a_stashing_landing_over_a_clean_tree_checks_out_with_no_stash() {
    let h = harness();
    let repo = h.stoat.git_host.discover(Path::new("/repo")).expect("repo");
    let entry = WalkEntry {
        open: None,
        stash: Some("wip".to_string()),
    };

    let landed = walk_landing(
        &*repo,
        WalkLandingKind::Walk,
        Path::new("/repo"),
        "b2c3d4e5",
        None,
        entry,
    );
    assert_eq!(
        (
            landed.ok().map(|landing| landing.parent),
            checkouts(&h),
            repo.stash_list().len(),
        ),
        (
            Some(Some("a1b2c3d4".to_string())),
            vec!["detached:b2c3d4e5".to_string()],
            0,
        ),
    );
}

#[test]
fn review_done_returns_to_the_starting_branch() {
    let mut h = harness();
    start_walk(&mut h);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();

    assert!(h.stoat.active_workspace().review_walk.is_none());
    assert_eq!(
        checkouts(&h).last().map(String::as_str),
        Some("ref:main"),
        "the branch HEAD was attached to is restored"
    );
}

/// A walk started while an ended walk's return waits in the git queue
/// reads HEAD after that return lands. It records the branch the return
/// restores, so ending it goes back to that branch and not to the ended
/// walk's commit.
#[test]
fn a_walk_started_behind_a_return_records_the_restored_branch() {
    let mut h = harness();
    start_walk(&mut h);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);

    let commit = {
        let repo = h.stoat.git_host.discover(Path::new("/repo")).expect("repo");
        repo.log_from("c3d4e5f6", 1)
            .into_iter()
            .next()
            .expect("the tip commit")
    };
    walk_one_commit(&mut h.stoat, PathBuf::from("/repo"), commit, None);
    assert_eq!(
        (
            h.stoat.active_workspace().review_walk.is_none(),
            h.stoat.git_jobs.holds(GitJobKey::WalkLanding),
        ),
        (true, true),
        "the start waits its turn behind the return",
    );

    h.settle();
    assert_eq!(
        (
            h.stoat
                .active_workspace()
                .review_walk
                .as_ref()
                .map(|walk| walk.return_ref.clone()),
            checkouts(&h),
        ),
        (
            Some(ReturnRef::Branch("main".to_string())),
            vec![
                "detached:a1b2c3d4".to_string(),
                "detached:b2c3d4e5".to_string(),
                "ref:main".to_string()
            ],
        ),
        "the start read HEAD after the return put it back on main",
    );

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();
    assert_eq!(
        checkouts(&h),
        [
            "detached:a1b2c3d4",
            "detached:b2c3d4e5",
            "ref:main",
            "ref:main"
        ],
        "ending the new walk goes back to main",
    );
}

/// A range review checks the tip of `from` out once and diffs it against
/// the merge base, named by both refs as typed. It opens the first tracked
/// file the whole range changed, where the tip commit alone changed
/// `c.rs`. The changes view over the range lists the untracked scratch file
/// beside the files the range changed, as it does under every base. It
/// holds one commit, so a step moves nothing, and `:review-done` returns
/// the tree.
#[test]
fn a_range_review_diffs_the_tip_against_the_merge_base() {
    let mut h = fork_harness();
    let tip = {
        let repo = h.stoat.git_host.discover(Path::new("/repo")).expect("repo");
        repo.log_from("e5f6a7b8", 1)
            .into_iter()
            .next()
            .expect("the tip of feature")
    };
    let span = WalkSpan::Range {
        base: "b2c3d4e5".to_string(),
        from: "feature".to_string(),
        against: "main".to_string(),
    };
    queue_walk_start(
        &mut h.stoat,
        PathBuf::from("/repo"),
        vec![tip],
        WalkLandingKind::Walk,
        span,
        WalkEntry::default(),
        WalkOrigin::Commits,
    );
    h.settle();
    let landed = (
        diff_base(&h),
        review_badge(&h),
        open_path(&h),
        checkouts(&h),
        diff::diff_sides(h.stoat.active_workspace().diff_base()),
    );

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
    h.settle();
    let stepped = checkouts(&h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffChanges);
    h.settle();
    let listed: Vec<String> = {
        let ws = h.stoat.active_workspace();
        ws.focused_diff_changes_id()
            .and_then(|id| ws.diff_changes_views.get(id))
            .map(|view| {
                view.excerpts
                    .iter()
                    .map(|excerpt| excerpt.rel_path.clone())
                    .collect()
            })
            .unwrap_or_default()
    };

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();

    assert_eq!(
        (
            landed,
            stepped,
            listed,
            checkouts(&h).last().cloned(),
            diff_base(&h)
        ),
        (
            (
                Some(Some("b2c3d4e5".to_string())),
                Some("reviewing feature against main".to_string()),
                Some(PathBuf::from("/repo/a.rs")),
                vec!["detached:e5f6a7b8".to_string()],
                ("main b2c3d4e".to_string(), "feature".to_string()),
            ),
            vec!["detached:e5f6a7b8".to_string()],
            vec![
                "0-scratch.txt".to_string(),
                "a.rs".to_string(),
                "c.rs".to_string()
            ],
            Some("ref:main".to_string()),
            None,
        ),
    );
}

/// The walk, base, badge, and checkouts that `command` leaves once it is
/// typed into the palette over [`fork_harness`] and submitted.
fn reviewed(
    command: &str,
) -> (
    Vec<String>,
    Option<Option<String>>,
    Option<String>,
    Vec<String>,
) {
    let mut h = fork_harness();
    h.type_text(command);
    let _ = h.snapshot();
    h.type_keys("enter");
    h.settle();
    (
        walk_shas(&h),
        diff_base(&h),
        review_badge(&h),
        checkouts(&h),
    )
}

/// `:review <from>` reviews `from` against the branch it forked from, and a
/// named `against`, a branch or a sha, lands the same base. `:review main`
/// reads against `feature`, and HEAD already sits on its tip.
#[test]
fn review_takes_the_tip_of_from_against_the_merge_base() {
    let range = |against: &str| {
        (
            vec!["e5f6a7b8".to_string()],
            Some(Some("b2c3d4e5".to_string())),
            Some(format!("reviewing feature against {against}")),
            vec!["detached:e5f6a7b8".to_string()],
        )
    };
    assert_eq!(
        [
            reviewed(":review feature"),
            reviewed(":review feature main"),
            reviewed(":review feature c3d4e5f6"),
            reviewed(":review main"),
        ],
        [
            range("main"),
            range("main"),
            range("c3d4e5f6"),
            (
                vec!["c3d4e5f6".to_string()],
                Some(Some("b2c3d4e5".to_string())),
                Some("reviewing main against feature".to_string()),
                Vec::new(),
            ),
        ],
    );
}

/// A revision that names nothing, a `from` that `against` already holds,
/// and a review while a walk holds HEAD each badge why and start nothing.
#[test]
fn review_refuses_what_it_cannot_review() {
    let mut walking = fork_harness();
    for _ in 0..2 {
        walking.type_text(":review feature");
        walking.type_keys("enter");
        walking.settle();
    }

    let refused = |label: &str| (Vec::new(), None, Some(label.to_string()), Vec::new());
    assert_eq!(
        [
            reviewed(":review junk"),
            reviewed(":review b2c3d4e5 main"),
            (
                walk_shas(&walking),
                diff_base(&walking),
                review_badge(&walking),
                checkouts(&walking),
            ),
        ],
        [
            refused("unknown revision"),
            refused("nothing to review"),
            (
                vec!["e5f6a7b8".to_string()],
                Some(Some("b2c3d4e5".to_string())),
                Some("a review walk is open".to_string()),
                vec!["detached:e5f6a7b8".to_string()],
            ),
        ],
    );
}

/// `:review done` ends a range review as `:review-done` ends a walk.
#[test]
fn review_done_ends_a_range_review() {
    let mut h = fork_harness();
    h.type_text(":review feature");
    h.type_keys("enter");
    h.settle();
    h.type_text(":review done");
    h.type_keys("enter");
    h.settle();

    assert_eq!(
        (
            checkouts(&h),
            h.stoat.active_workspace().review_walk.is_none()
        ),
        (
            vec!["detached:e5f6a7b8".to_string(), "ref:main".to_string()],
            true
        ),
    );
}

/// The branch `from` forked from is the one with the newest fork point.
/// A tie goes to `main` over a name that sorts before it, and a repo with
/// no other branch has none.
#[test]
fn fork_branch_takes_the_newest_fork_point() {
    let fork = |extra: &[(&str, &str)]| {
        let h = fork_harness();
        for (name, sha) in extra {
            h.fake_git().add_repo("/repo").branch(name, sha);
        }
        let repo = h.stoat.git_host.discover(Path::new("/repo")).expect("repo");
        stoat::action_handlers::review_walk::fork_branch(&*repo, "feature", "e5f6a7b8")
            .map(|(name, _)| name)
    };
    let lone = {
        let h = Stoat::test();
        h.seed_linear_history("/repo", &[("a1b2c3d4", "one", &[("a.rs", "1\n")])]);
        h.fake_git().add_repo("/repo").branch("main", "a1b2c3d4");
        let repo = h.stoat.git_host.discover(Path::new("/repo")).expect("repo");
        stoat::action_handlers::review_walk::fork_branch(&*repo, "main", "a1b2c3d4")
    };

    assert_eq!(
        [
            fork(&[]),
            fork(&[("alpha", "b2c3d4e5")]),
            fork(&[("topic", "d4e5f6a7")]),
            lone.map(|(name, _)| name),
        ],
        [
            Some("main".to_string()),
            Some("main".to_string()),
            Some("topic".to_string()),
            None,
        ],
    );
}

/// The palette completes `:review`'s first argument from the branches, and
/// Enter takes the highlighted row for the second.
#[test]
fn the_palette_completes_both_review_arguments() {
    let mut h = fork_harness();
    h.type_text(":review featu");
    let _ = h.snapshot();
    h.type_keys("tab");
    let completed = h
        .stoat
        .command_palette
        .as_ref()
        .map(|palette| palette.input.text(h.stoat.active_workspace()));

    assert_eq!(
        (
            completed.as_deref(),
            reviewed(":review feature ma").2.as_deref()
        ),
        (
            Some("review feature"),
            Some("reviewing feature against main")
        ),
    );
}

/// [`fork_harness`] with HEAD on `feature`, and a `topic` branch at
/// `d4e5f6a7` for each step of a range pick to land on.
fn range_pick_harness() -> TestHarness {
    let h = fork_harness();
    h.fake_git()
        .add_repo("/repo")
        .branch("topic", "d4e5f6a7")
        .set_head("e5f6a7b8")
        .set_head_branch("feature");
    h
}

/// A bare `:review` opened over [`range_pick_harness`], with its list in.
fn open_range_pick() -> TestHarness {
    let mut h = range_pick_harness();
    h.type_text(":review");
    h.type_keys("enter");
    h.settle();
    h
}

/// Put the commit picker's cursor on the row for `sha`.
fn select_row(h: &mut TestHarness, sha: &str) {
    let picker = h.stoat.commit_picker.as_mut().expect("picker");
    picker.selected = picker
        .filtered
        .iter()
        .position(|&idx| picker.commits[idx].sha == sha)
        .unwrap_or_else(|| panic!("no row {sha}"));
}

/// The picker's role, scope label, listed shas, and the sha under its
/// cursor.
type PickerState = (
    Option<CommitPickerRole>,
    Option<String>,
    Vec<String>,
    Option<String>,
);

fn picker_state(h: &TestHarness) -> PickerState {
    let picker = h.stoat.commit_picker.as_ref();
    (
        picker.map(|picker| picker.role),
        picker.and_then(|picker| picker.scope_label.clone()),
        picker_shas(h),
        picker
            .and_then(|picker| picker.selected_commit())
            .map(|commit| commit.sha.clone()),
    )
}

/// A bare `:review` picks both ends from the commit list over HEAD's
/// graph. The first pick re-scopes the list to its first-parent history,
/// and each step lands on the nearest tip of another branch. The second
/// pick starts the review, which names `from` by its branch and `against`
/// by its short sha.
#[test]
fn a_bare_review_picks_both_ends_from_the_commit_list() {
    let mut h = open_range_pick();
    let first = picker_state(&h);

    select_row(&mut h, "e5f6a7b8");
    stoat::action_handlers::review_walk::commit_picker_select(&mut h.stoat);
    h.settle();
    let second = picker_state(&h);

    select_row(&mut h, "b2c3d4e5");
    stoat::action_handlers::review_walk::commit_picker_select(&mut h.stoat);
    h.settle();

    let history: Vec<String> = ["e5f6a7b8", "d4e5f6a7", "b2c3d4e5", "a1b2c3d4"]
        .map(String::from)
        .to_vec();
    let topic = Some("d4e5f6a7".to_string());
    assert_eq!(
        (
            first,
            second,
            h.stoat.commit_picker.is_none(),
            walk_shas(&h),
            diff_base(&h),
            review_badge(&h),
        ),
        (
            (
                Some(CommitPickerRole::PickRange),
                None,
                history.clone(),
                topic.clone()
            ),
            (
                Some(CommitPickerRole::PickRange),
                Some("review e5f6a7b against".to_string()),
                history,
                topic,
            ),
            true,
            vec!["e5f6a7b8".to_string()],
            Some(Some("b2c3d4e5".to_string())),
            Some("reviewing feature against b2c3d4e".to_string()),
        ),
    );
}

/// Back from the second step drops the first pick, whether the second
/// list has landed or not, and a second pick of the first commit has
/// nothing to review.
#[test]
fn a_range_pick_backs_out_and_refuses_one_commit_twice() {
    let mut h = open_range_pick();
    let pick_tip = |h: &mut TestHarness| {
        select_row(h, "e5f6a7b8");
        stoat::action_handlers::review_walk::commit_picker_select(&mut h.stoat);
    };
    let dropped = |h: &TestHarness| {
        h.stoat
            .commit_picker
            .as_ref()
            .map(|picker| (picker.scope_label.clone(), picker.range_from.is_none()))
    };

    pick_tip(&mut h);
    stoat::action_handlers::review_walk::commit_picker_back(&mut h.stoat);
    let before_landing = dropped(&h);

    pick_tip(&mut h);
    h.settle();
    stoat::action_handlers::review_walk::commit_picker_back(&mut h.stoat);
    let after_landing = dropped(&h);

    pick_tip(&mut h);
    h.settle();
    pick_tip(&mut h);
    h.settle();
    let badge = {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| (badge.label.clone(), badge.detail.clone()))
    };

    assert_eq!(
        (before_landing, after_landing, badge, walk_shas(&h)),
        (
            Some((None, true)),
            Some((None, true)),
            Some((
                "nothing to review".to_string(),
                Some("pick an older commit".to_string())
            )),
            Vec::new(),
        ),
    );
}

/// The first step lists HEAD's whole graph, so a merged branch's commits
/// are rows of their own, and the second lists the pick's first-parent
/// history.
#[test]
fn a_range_pick_lists_the_graph_then_first_parents() {
    let mut h = merged_harness();
    h.type_text(":review");
    h.type_keys("enter");
    h.settle();
    let first = picker_shas(&h);

    select_row(&mut h, "m9999999");
    stoat::action_handlers::review_walk::commit_picker_select(&mut h.stoat);
    h.settle();

    assert_eq!(
        (first, picker_shas(&h)),
        (
            MERGED_DAG.map(String::from).to_vec(),
            ["m9999999", "b2c3d4e5", "a1b2c3d4"]
                .map(String::from)
                .to_vec()
        ),
    );
}

#[test]
fn review_done_from_a_detached_head_returns_to_the_sha() {
    let mut h = Stoat::test();
    h.seed_linear_history(
        "/repo",
        &[
            ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
            ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
        ],
    );
    h.fake_git().add_repo("/repo").branch("main", "b2c3d4e5");
    h.stoat.active_workspace_mut().git_root = "/repo".into();

    h.type_text(":git review main");
    h.type_keys("enter");
    h.settle();
    h.type_keys("down");
    h.type_keys("enter");
    h.settle();

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();
    assert_eq!(
        checkouts(&h).last().map(String::as_str),
        Some("detached:b2c3d4e5"),
        "an already-detached HEAD is restored by sha, not attached to a branch"
    );
}

/// The fake fails a return checkout on an unknown branch rather than on a
/// dirty tree, which it does not model. Either way this is the path where
/// the return fails and the walk has to survive it.
#[test]
fn a_failed_return_keeps_the_walk() {
    let mut h = harness();
    h.fake_git().add_repo("/repo").set_head_branch("gone");
    start_walk(&mut h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();

    assert!(
        h.stoat.active_workspace().review_walk.is_some(),
        "the walk survives so :review-done can be retried"
    );
    assert_eq!(review_badge(&h).as_deref(), Some("could not return"));
}

/// A failed return puts its walk back only into a workspace with no walk.
/// A walk started while the return waited keeps its place.
#[test]
fn a_failed_return_keeps_a_walk_started_after_the_press() {
    let mut h = harness();
    h.fake_git().add_repo("/repo").set_head_branch("gone");
    start_walk(&mut h);

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    seed_walk(&mut h, &["b2c3d4e5"]);

    assert_eq!(walk_shas(&h), ["b2c3d4e5"], "the later walk stands");
}

/// One return waits in the git queue at a time. A walk ended while an
/// earlier walk's return is still out stays in place and says why.
///
/// The second walk goes in without a settle. A settle there lands the first
/// return before the second press.
#[test]
fn review_done_refuses_while_a_return_is_out() {
    let mut h = harness();
    start_walk(&mut h);
    git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    let second = walk_over(&h, &["b2c3d4e5"]);
    h.stoat.active_workspace_mut().review_walk = Some(second);
    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);

    assert_eq!(
        (walk_shas(&h), review_badge(&h).as_deref()),
        (
            vec!["b2c3d4e5".to_string()],
            Some("the last walk is still returning")
        ),
    );
}

/// A walk over a workdir outside every repository has no checkout to return
/// to, so it stays for another try.
#[test]
fn review_done_outside_a_repository_keeps_the_walk() {
    let mut h = harness();
    seed_walk(&mut h, &["a1b2c3d4"]);
    h.stoat
        .active_workspace_mut()
        .review_walk
        .as_mut()
        .expect("a walk")
        .workdir = PathBuf::from("/elsewhere");

    stoat::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
    h.settle();

    assert_eq!(
        (walk_shas(&h), review_badge(&h)),
        (
            vec!["a1b2c3d4".to_string()],
            Some("not in a git repository".to_string())
        ),
    );
}

/// A restored walk over a workdir outside every repository ends at the
/// check of HEAD, since a return from it fails, and the Review badge says
/// so.
#[test]
fn the_check_ends_a_restored_walk_outside_a_repository() {
    let mut h = harness();
    let ws = h.stoat.active_workspace_mut();
    ws.review_walk = Some(ReviewWalk {
        workdir: PathBuf::from("/elsewhere"),
        ..ReviewWalk::blame_checkout("a1b2c3d4", ReturnRef::Branch("main".into()), None)
    });
    ws.walk_unchecked = true;

    check_restored_walk(&mut h.stoat);
    h.settle();
    assert_eq!(
        (walk_shas(&h), review_badge(&h)),
        (
            Vec::<String>::new(),
            Some("checkout ended outside stoat".to_string())
        ),
    );
}

/// The check's landing leaves alone a walk that is no longer at the checked
/// commit, such as one that a return took while the check ran.
#[test]
fn the_check_leaves_a_walk_at_another_commit() {
    let mut h = harness();
    h.stoat.active_workspace_mut().review_walk = Some(ReviewWalk::blame_checkout(
        "b2c3d4e5",
        ReturnRef::Branch("main".into()),
        None,
    ));

    land_walk_check(&mut h.stoat, "a1b2c3d4", Some("c3d4e5f6".to_string()));
    assert_eq!(
        (walk_shas(&h), review_badge(&h)),
        (vec!["b2c3d4e5".to_string()], None),
    );
}
