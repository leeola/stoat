use crate::{
    app::{Stoat, UpdateEffect},
    commit_list::PendingPreview,
    commit_picker::{CommitPicker, CommitPickerRole},
    review_session::ReviewSession,
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

    let commits = repo.log_from(&ref_sha, WALK_LIMIT);
    let mut branch_tips: HashMap<String, Vec<String>> = HashMap::new();
    for (name, sha) in repo.local_branches() {
        branch_tips.entry(sha).or_default().push(name);
    }

    let executor = stoat.executor.clone();
    let picker = CommitPicker::new(
        stoat.active_workspace_mut(),
        executor,
        CommitPickerRole::PickBase,
        workdir,
        ref_sha,
        commits,
        branch_tips,
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

/// Take the selected commit as the review base.
pub(crate) fn commit_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    match picker.role {
        // FIXME: installing the walk lands with the ReviewWalk item. Until
        // then, selecting only dismisses the picker.
        CommitPickerRole::PickBase => commit_picker_close(stoat),
    }
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
    use crate::{app::Stoat, badge::BadgeSource, test_harness::TestHarness};

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
            .branch("feature", "b2c3d4e5");
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
}
