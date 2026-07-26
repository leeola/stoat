use crate::{
    app::{Stoat, UpdateEffect},
    commit_list::PendingPreview,
    commit_picker::{CommitPicker, CommitPickerRole},
    review_session::{ReviewOrigin, ReviewSession},
    review_walk::{ReturnRef, ReviewWalk},
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

/// Commits the picker loads for a ref. Bounds the eager walk on a long
/// history without paging, which fuzzy filtering could not work across.
const WALK_LIMIT: usize = 1000;

/// Open the commit picker over `reference`'s first-parent history.
///
/// The whole walk is loaded up front rather than paged. The picker filters
/// across every row, and a virtualized window would only ever search the part
/// already fetched. [`WALK_LIMIT`] bounds that, so a history longer than the
/// limit is silently truncated at its oldest end.
pub(crate) fn git_review(stoat: &mut Stoat, reference: &str) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        return review_error(stoat, "not in a git repository", None);
    };
    let Some(workdir) = repo.workdir() else {
        return review_error(stoat, "git repo has no working tree", None);
    };
    let Some(ref_sha) = repo.resolve_rev(reference) else {
        return review_error(
            stoat,
            "unknown revision",
            Some(format!("no revision named {reference}")),
        );
    };
    open_commit_picker(stoat, &repo, CommitPickerRole::PickBase, workdir, ref_sha)
}

/// List the current branch's history read-only.
///
/// The same picker `:git-review` opens, in a role where selecting a row
/// dismisses rather than starting a walk, so browsing history never touches the
/// working tree.
pub(crate) fn git_ls(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        return review_error(stoat, "not in a git repository", None);
    };
    let Some(workdir) = repo.workdir() else {
        return review_error(stoat, "git repo has no working tree", None);
    };
    let Some(head) = repo.resolve_rev("HEAD") else {
        return review_error(stoat, "no commits on this branch", None);
    };
    open_commit_picker(stoat, &repo, CommitPickerRole::Browse, workdir, head)
}

/// Build and install a picker over `ref_sha`'s first-parent history.
///
/// The editor underneath is dropped to normal mode first, so a chord that
/// opened the picker releases rather than waiting for a key the picker has
/// taken focus away from. The picker's own input then supplies the insert mode
/// its keymap block is gated on.
fn open_commit_picker(
    stoat: &mut Stoat,
    repo: &Arc<dyn crate::host::GitRepo>,
    role: CommitPickerRole,
    workdir: PathBuf,
    ref_sha: String,
) -> UpdateEffect {
    stoat.set_focused_mode("normal".to_string());

    let commits = repo.log_from(&ref_sha, WALK_LIMIT);
    let mut branch_tips: HashMap<String, Vec<String>> = HashMap::new();
    for (name, sha) in repo.local_branches() {
        branch_tips.entry(sha).or_default().push(name);
    }

    let executor = stoat.executor.clone();
    let picker = CommitPicker::new(
        stoat.active_workspace_mut(),
        executor,
        role,
        workdir,
        ref_sha,
        commits,
        branch_tips,
        // Ages are measured against the moment the picker opened, so they hold
        // still while the user scrolls rather than drifting under them.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs() as i64)
            .unwrap_or(0),
    );
    stoat.commit_picker = Some(picker);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

fn review_error(stoat: &mut Stoat, label: &str, detail: Option<String>) -> UpdateEffect {
    super::review::emit_review_error_badge(stoat, label, detail);
    UpdateEffect::Redraw
}

pub(crate) fn commit_picker_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.move_selection(delta);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Advance which column of the commit table the query filters.
///
/// Refilters against the query already typed, so the list narrows to the new
/// column immediately, and re-syncs the preview because the selection may land
/// on a different commit once the rows change.
pub(crate) fn commit_picker_column_cycle(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    let query = picker.input.text(stoat.active_workspace());
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.cycle_filter_column(&query);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Page the commit picker's selection by half its visible rows in `dir`.
///
/// The paging counterpart to [`commit_picker_step`], and it syncs the preview
/// the same way so the diff follows a paged selection as it does a stepped one.
pub(crate) fn commit_picker_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.page(dir);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Take the selected commit as the review base and start walking from it.
///
/// The walk spans the selected commit through the ref tip, including commits
/// the query filtered out of view. The user picked a starting point in
/// history, not a subset of rows.
pub(crate) fn commit_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    match picker.role {
        CommitPickerRole::PickBase => install_walk(stoat),
        CommitPickerRole::Browse => commit_picker_close(stoat),
    }
}

fn install_walk(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    let Some(base_sha) = picker.selected_commit().map(|c| c.sha.clone()) else {
        return UpdateEffect::None;
    };
    let workdir = picker.workdir.clone();
    let Some(base_idx) = picker.commits.iter().position(|c| c.sha == base_sha) else {
        return UpdateEffect::None;
    };
    let mut commits = picker.commits[..=base_idx].to_vec();
    commits.reverse();

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        return review_error(stoat, "not in a git repository", None);
    };
    if repo.has_tracked_changes() {
        return dirty_tree_error(stoat);
    }
    let return_ref = match repo.head_branch() {
        Some(branch) => ReturnRef::Branch(branch),
        None => match repo.resolve_rev("HEAD") {
            Some(sha) => ReturnRef::Detached(sha),
            None => return review_error(stoat, "cannot resolve HEAD", None),
        },
    };

    commit_picker_close(stoat);
    stoat.active_workspace_mut().review_walk = Some(ReviewWalk {
        workdir,
        commits,
        cursor: 0,
        return_ref,
    });
    walk_navigate(stoat)
}

/// Check the walk's current commit out and open its diff.
///
/// The session opens as [`ReviewOrigin::Standalone`] so closing the diff drops
/// to normal mode with the walk still running, which is what lets the user
/// look around a commit's files and then step on.
fn walk_navigate(stoat: &mut Stoat) -> UpdateEffect {
    let Some((workdir, sha)) = stoat
        .active_workspace()
        .review_walk
        .as_ref()
        .map(|walk| (walk.workdir.clone(), walk.current().sha.clone()))
    else {
        return UpdateEffect::None;
    };
    let Some(repo) = stoat.git_host.discover(&workdir) else {
        return review_error(stoat, "not in a git repository", None);
    };

    // Re-entering a commit the tree already sits on has nothing to check out,
    // so the dirty guard would only reject a tree the walk itself is fine with.
    if repo.resolve_rev("HEAD").as_deref() != Some(sha.as_str()) {
        if repo.has_tracked_changes() {
            return dirty_tree_error(stoat);
        }
        if let Err(err) = repo.checkout_detached(&sha) {
            return review_error(stoat, "checkout failed", Some(err.to_string()));
        }
    }

    super::review::open_commit_review(stoat, workdir, sha, ReviewOrigin::Standalone)
}

pub(crate) fn review_next_commit(stoat: &mut Stoat) -> UpdateEffect {
    walk_step(stoat, 1)
}

pub(crate) fn review_prev_commit(stoat: &mut Stoat) -> UpdateEffect {
    walk_step(stoat, -1)
}

fn walk_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(walk) = stoat.active_workspace_mut().review_walk.as_mut() else {
        return UpdateEffect::None;
    };
    if !walk.step(delta) {
        return UpdateEffect::Redraw;
    }
    walk_navigate(stoat)
}

/// End the walk, putting the working tree back where it started.
///
/// A failed checkout back keeps the walk, so the user can fix whatever blocked
/// it and retry rather than being stranded detached with no record of where
/// they came from.
pub(crate) fn review_done(stoat: &mut Stoat) -> UpdateEffect {
    let Some(walk) = stoat.active_workspace_mut().review_walk.take() else {
        return UpdateEffect::None;
    };
    let Some(repo) = stoat.git_host.discover(&walk.workdir) else {
        stoat.active_workspace_mut().review_walk = Some(walk);
        return review_error(stoat, "not in a git repository", None);
    };

    let restored = match &walk.return_ref {
        ReturnRef::Branch(name) => repo.checkout_ref(name),
        ReturnRef::Detached(sha) => repo.checkout_detached(sha),
    };
    if let Err(err) = restored {
        stoat.active_workspace_mut().review_walk = Some(walk);
        return review_error(stoat, "could not return", Some(err.to_string()));
    }

    if stoat.active_workspace().review.is_some() {
        super::review::close_review(stoat);
    }
    UpdateEffect::Redraw
}

fn dirty_tree_error(stoat: &mut Stoat) -> UpdateEffect {
    review_error(
        stoat,
        "uncommitted changes",
        Some("commit or stash tracked changes before reviewing".to_string()),
    )
}

pub(crate) fn commit_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.take() else {
        return UpdateEffect::None;
    };
    let active_idx = stoat.active_workspace;
    picker.dispose(&mut stoat.workspaces[active_idx]);
    UpdateEffect::Redraw
}

/// Re-run the picker's filter against its input text and keep the preview
/// pointed at the selection.
///
/// Called every drive tick rather than on a change hook, because the query
/// lives in an [`crate::input_view::InputView`] the global keymap types into
/// directly, so the picker only learns about a keystroke by re-reading it.
pub(crate) fn sync_commit_picker(stoat: &mut Stoat) {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return;
    };
    let query = picker.input.text(stoat.active_workspace());
    if let Some(picker) = stoat.commit_picker.as_mut() {
        picker.refilter(&query);
    }
    ensure_selected_preview(stoat);
}

/// Poll the in-flight preview build and start one for the selection when
/// needed. Returns whether anything landed or was spawned, so the caller knows
/// a redraw is warranted.
pub(crate) fn pump_commit_picker(stoat: &mut Stoat) -> bool {
    let landed = match stoat.commit_picker.as_mut() {
        Some(picker) => poll_pending_preview(picker),
        None => return false,
    };

    let spawned_before = pending_sha(stoat).is_some();
    ensure_selected_preview(stoat);
    let spawned_after = pending_sha(stoat).is_some();

    landed || (spawned_after && !spawned_before)
}

fn pending_sha(stoat: &Stoat) -> Option<String> {
    stoat
        .commit_picker
        .as_ref()?
        .pending_preview
        .as_ref()
        .map(|p| p.sha.clone())
}

/// Spawn a background [`ReviewSession`] build for the selected commit unless
/// one is already cached or in flight for that sha.
fn ensure_selected_preview(stoat: &mut Stoat) {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return;
    };
    let Some(sha) = picker.selected_commit().map(|c| c.sha.clone()) else {
        return;
    };
    if picker.preview_sessions.contains_key(&sha)
        || picker
            .pending_preview
            .as_ref()
            .is_some_and(|p| p.sha == sha)
    {
        return;
    }

    let workdir = picker.workdir.clone();
    let Some(repo) = stoat.git_host.discover(&workdir) else {
        return;
    };
    let task = spawn_preview_load(
        &stoat.executor,
        repo,
        workdir,
        sha.clone(),
        stoat.language_registry.clone(),
    );

    if let Some(picker) = stoat.commit_picker.as_mut() {
        picker.requested_preview = Some(sha.clone());
        picker.pending_preview = Some(PendingPreview { sha, task });
    }
}

/// Poll the pending build, caching a finished session under its sha. A session
/// for a sha the selection has since moved off is dropped rather than cached,
/// since the picker only ever renders the selected commit's preview.
fn poll_pending_preview(picker: &mut CommitPicker) -> bool {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    let Some(mut pending) = picker.pending_preview.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut pending.task).poll(&mut cx) {
        Poll::Ready(Some(session)) => {
            if picker.requested_preview.as_deref() == Some(pending.sha.as_str()) {
                picker
                    .preview_sessions
                    .insert(pending.sha, Arc::new(session));
            }
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            picker.pending_preview = Some(pending);
            false
        },
    }
}

fn spawn_preview_load(
    executor: &stoat_scheduler::Executor,
    repo: Arc<dyn crate::host::GitRepo>,
    workdir: PathBuf,
    sha: String,
    language_registry: Arc<stoat_language::LanguageRegistry>,
) -> stoat_scheduler::Task<Option<ReviewSession>> {
    executor.spawn_blocking(move || {
        let new_tree = repo.commit_tree(&sha)?;
        let base_tree = match repo.parent_sha(&sha) {
            Some(parent) => repo.commit_tree(&parent).unwrap_or_default(),
            None => std::collections::BTreeMap::new(),
        };
        let source = crate::review_session::ReviewSource::Commit {
            workdir: workdir.clone(),
            sha: sha.clone(),
        };
        super::review::build_session_from_trees(
            &language_registry,
            source,
            &workdir,
            &base_tree,
            &new_tree,
        )
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        app::Stoat, badge::BadgeSource, commit_picker::CommitPickerRole, test_harness::TestHarness,
    };

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
        h
    }

    fn review_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }

    #[test]
    fn git_review_opens_the_picker_over_the_refs_history() {
        let mut h = harness();
        h.type_text(":git-review main");
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
        h.type_text(":git-review b2c3d4e5");
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
        h.type_text(":git-review junk");
        h.type_keys("enter");
        h.settle();

        assert!(h.stoat.commit_picker.is_none());
        assert_eq!(review_badge(&h).as_deref(), Some("unknown revision"));
    }

    #[test]
    fn outside_a_repository_it_badges_too() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = "/elsewhere".into();
        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();

        assert!(h.stoat.commit_picker.is_none());
        assert_eq!(review_badge(&h).as_deref(), Some("not in a git repository"));
    }

    #[test]
    fn the_picker_takes_typing_as_its_query() {
        let mut h = harness();
        h.type_text(":git-review main");
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
        h.type_text(":git-ls");
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

    #[test]
    fn space_g_l_opens_the_browser() {
        let mut h = harness();
        h.type_keys("space g l");
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
        h.type_text(":git-ls");
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
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        assert!(h.stoat.commit_picker.is_none());
        assert_eq!(review_badge(&h).as_deref(), Some("not in a git repository"));
    }

    fn checkouts(h: &TestHarness) -> Vec<String> {
        h.fake_git().checkouts(std::path::Path::new("/repo"))
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
        h.type_text(":git-review main");
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

    #[test]
    fn the_walk_spans_history_the_query_hid() {
        let mut h = harness();
        h.type_text(":git-review main");
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
    fn stepping_checks_each_commit_out_and_clamps_at_the_tip() {
        let mut h = harness();
        start_walk(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        assert_eq!(walk_cursor(&h), Some(2), "a step past the tip is a no-op");
        assert_eq!(checkouts(&h).len(), 3, "and checks nothing else out");
    }

    #[test]
    fn stepping_back_walks_toward_the_base() {
        let mut h = harness();
        start_walk(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewPrevCommit);
        h.settle();
        assert_eq!(walk_cursor(&h), Some(0));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewPrevCommit);
        h.settle();
        assert_eq!(walk_cursor(&h), Some(0), "a step past the base is a no-op");
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

    #[test]
    fn review_done_returns_to_the_starting_branch() {
        let mut h = harness();
        start_walk(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();

        assert!(h.stoat.active_workspace().review_walk.is_none());
        assert_eq!(
            checkouts(&h).last().map(String::as_str),
            Some("ref:main"),
            "the branch HEAD was attached to is restored"
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

        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();
        h.type_keys("down");
        h.type_keys("enter");
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();

        assert!(
            h.stoat.active_workspace().review_walk.is_some(),
            "the walk survives so :review-done can be retried"
        );
        assert_eq!(review_badge(&h).as_deref(), Some("could not return"));
    }
}
