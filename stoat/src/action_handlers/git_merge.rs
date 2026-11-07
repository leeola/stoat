//! The merge commands, which merge a local branch into HEAD, abort the merge in
//! progress, and commit it through the commit message buffer.
//!
//! The merge and the abort are git writes, so each waits its turn in the git
//! queue behind every write queued before the press, and runs on the pool.

use crate::{
    action_handlers::review::emit_review_error_badge,
    app::{Stoat, UpdateEffect},
    auto_reload,
    git_jobs::{self, GitJob, GitLanding, GitWork},
    host::{git::MergeOutcome, GitApplyError},
};

/// Merge the local branch `branch` into HEAD.
///
/// A fast-forward and a clean merge bring the editor to the new tree. A merge
/// that conflicts opens the conflict view on the unmerged files. A review walk,
/// a rebase, or a merge in progress refuses the command at its turn, and the
/// repository's own refusal badges when the merge does not run.
pub(super) fn git_merge(stoat: &mut Stoat, branch: &str) -> UpdateEffect {
    let branch = branch.to_string();
    let job = GitJob::new("merge", None, move |stoat: &mut Stoat| {
        let repo = super::review_walk::checkout_repo(stoat)?;
        if repo.merge_state().is_some() {
            emit_review_error_badge(stoat, "a merge is in progress", None);
            return None;
        }
        Some(Box::new(move || {
            let merged = repo.merge_branch(&branch);
            Box::new(move |stoat: &mut Stoat| land_merge(stoat, &branch, merged)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Abort the merge in progress, and bring the editor to the tree the abort
/// leaves.
pub(super) fn git_merge_abort(stoat: &mut Stoat) -> UpdateEffect {
    let job = GitJob::new("merge abort", None, |stoat: &mut Stoat| {
        let repo = super::review_walk::checkout_repo(stoat)?;
        Some(Box::new(move || {
            let aborted = repo.abort_merge();
            Box::new(move |stoat: &mut Stoat| land_abort(stoat, aborted)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Commit the merge in progress through the commit message buffer, which
/// opens over the merge's message.
///
/// Badges and changes nothing when no merge is in progress.
pub(super) fn git_merge_continue(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let merging = stoat
        .git_host
        .discover(&git_root)
        .is_some_and(|repo| repo.merge_state().is_some());
    if !merging {
        emit_review_error_badge(stoat, "no merge in progress", None);
        return UpdateEffect::Redraw;
    }
    super::git_commit::git_commit(stoat)
}

/// Bring the editor to the tree a merge left, or badge why the merge did not
/// run.
fn land_merge(stoat: &mut Stoat, branch: &str, merged: Result<MergeOutcome, GitApplyError>) {
    let outcome = match merged {
        Ok(outcome) => outcome,
        Err(err) => {
            emit_review_error_badge(stoat, "merge failed", Some(err.to_string()));
            return;
        },
    };

    match outcome {
        MergeOutcome::UpToDate => stoat.set_status("already up to date"),
        MergeOutcome::FastForward { sha } => {
            follow_tree(stoat);
            super::commits::reload_every_list(stoat);
            stoat.set_status(format!("fast-forwarded to {}", short(&sha)));
        },
        MergeOutcome::Merged { sha } => {
            follow_tree(stoat);
            super::commits::reload_every_list(stoat);
            stoat.set_status(format!("merged {branch}: {}", short(&sha)));
        },
        MergeOutcome::Conflicts { files } => {
            follow_tree(stoat);
            let noun = if files == 1 { "file" } else { "files" };
            stoat.set_status(format!("merge: {files} conflicted {noun}"));
            super::conflict_view::open_conflict(stoat);
        },
    }
}

/// Bring the editor to the tree an abort left, or badge why the abort did not
/// run.
///
/// An open conflict view closes first, since the files it shows no longer
/// conflict.
fn land_abort(stoat: &mut Stoat, aborted: Result<(), GitApplyError>) {
    if let Err(err) = aborted {
        emit_review_error_badge(stoat, "merge abort failed", Some(err.to_string()));
        return;
    }
    if stoat.active_workspace().conflict.is_some() {
        super::conflict_view::close_conflict(stoat);
    }
    follow_tree(stoat);
    stoat.set_status("merge aborted");
}

/// Re-read every clean buffer's file and recompute every diff, after a git
/// write that rewrote files in the working tree.
fn follow_tree(stoat: &mut Stoat) {
    auto_reload::reload_clean_buffers(stoat);
    stoat.active_workspace_mut().invalidate_all_diffs();
}

/// The seven leading characters of `sha`, as a status line names a commit.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch, app::Stoat, badge::BadgeSource, host::GitRepo,
        test_harness::TestHarness,
    };
    use std::{path::Path, sync::Arc};
    use stoat_action::GitMergeAbort;

    /// A harness rooted in `/repo`, with HEAD on `main` at `c2` over `c1`, and
    /// the branch `feature` at `f1`, which grows from `c1` with a file of its
    /// own.
    fn harness() -> TestHarness {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("c1", "one", &[("a.rs", "1\n")]),
                ("c2", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .commit_with_parent("f1", "c1", &[("b.rs", "feature\n")])
            .branch("feature", "f1")
            .set_head("c2")
            .branch("main", "c2")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    /// Seed one conflict on `a.rs` for the next merge.
    fn conflict_next_merge(h: &TestHarness) {
        h.fake_git().add_repo("/repo").merge_conflict(
            "a.rs",
            Some("1\n"),
            Some("2\n"),
            Some("3\n"),
        );
    }

    fn submit(h: &mut TestHarness, command: &str) {
        h.type_text(command);
        h.type_keys("enter");
        h.settle();
    }

    fn repo(h: &TestHarness) -> Arc<dyn GitRepo> {
        h.stoat
            .git_host
            .discover(Path::new("/repo"))
            .expect("the repo")
    }

    fn review_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }

    /// A clean merge records the branch, moves HEAD to a commit over both
    /// tips, and names that commit in the status line.
    #[test]
    fn a_clean_merge_lands_a_commit_over_both_tips() {
        let mut h = harness();
        submit(&mut h, ":git merge feature");

        let repo = repo(&h);
        let head = repo.resolve_rev("HEAD").expect("a HEAD");
        assert_eq!(
            (
                h.fake_git().merges(Path::new("/repo")),
                repo.parent_shas(&head),
                h.stoat.pending_message.clone(),
            ),
            (
                vec!["feature".to_string()],
                vec!["c2".to_string(), "f1".to_string()],
                Some(format!("merged feature: {}", &head[..7])),
            ),
        );
    }

    /// A merge that conflicts stays mid-merge, counts the conflicted file in
    /// the status line, and opens the conflict view on it.
    #[test]
    fn a_conflicting_merge_opens_the_conflict_view() {
        let mut h = harness();
        conflict_next_merge(&h);
        submit(&mut h, ":git merge feature");

        assert_eq!(
            (
                h.stoat.active_workspace().conflict.is_some(),
                repo(&h).merge_state().map(|state| state.conflicted),
                h.stoat.pending_message.clone(),
            ),
            (true, Some(1), Some("merge: 1 conflicted file".to_string())),
        );
    }

    /// The abort ends the merge, closes the conflict view the merge opened, and
    /// says so.
    #[test]
    fn the_abort_ends_the_merge_and_closes_the_conflict_view() {
        let mut h = harness();
        conflict_next_merge(&h);
        submit(&mut h, ":git merge feature");
        dispatch(&mut h.stoat, &GitMergeAbort);
        h.settle();

        assert_eq!(
            (
                repo(&h).merge_state(),
                h.stoat.active_workspace().conflict.is_some(),
                h.stoat.pending_message.clone(),
            ),
            (None, false, Some("merge aborted".to_string())),
        );
    }

    /// A merge during a merge, an abort with no merge, and a continue with no
    /// merge each badge why and merge nothing.
    #[test]
    fn the_merge_commands_refuse_out_of_turn() {
        let refusal = |merging: bool, command: &str| {
            let mut h = harness();
            if merging {
                h.fake_git().add_repo("/repo").merge_in_progress(
                    "f1",
                    Some("feature"),
                    "Merge branch 'feature'",
                );
            }
            submit(&mut h, command);
            (review_badge(&h), h.fake_git().merges(Path::new("/repo")))
        };

        assert_eq!(
            [
                refusal(true, ":git merge feature"),
                refusal(false, ":git merge abort"),
                refusal(false, ":git merge continue"),
            ],
            [
                (Some("a merge is in progress".to_string()), vec![]),
                (Some("merge abort failed".to_string()), vec![]),
                (Some("no merge in progress".to_string()), vec![]),
            ],
        );
    }
}
