use crate::{
    action_handlers::{git_commit::commit_repo, review::emit_review_error_badge},
    app::{Stoat, UpdateEffect},
    auto_reload,
    git_jobs::{self, GitJob, GitLanding, GitWork},
    host::{CommitInfo, GitApplyError},
};

/// Stash every tracked change, under `message` or git's default message.
///
/// The stash waits its turn in the git queue behind every git write queued
/// before the press. A review walk or a rebase owns HEAD until it ends, so
/// either one refuses the command at its turn.
pub(super) fn git_stash(stoat: &mut Stoat, message: Option<&str>) -> UpdateEffect {
    let message = message.map(str::to_string);
    let job = GitJob::new("stash", None, move |stoat: &mut Stoat| {
        let (repo, _) = commit_repo(stoat)?;
        Some(Box::new(move || {
            let stashed = match repo.stash_save(message.as_deref()) {
                Ok(sha) => Ok(sha),
                Err(GitApplyError::Backend { reason, .. }) => Err(reason),
            };
            Box::new(move |stoat: &mut Stoat| land_stash(stoat, stashed)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Bring the editor to the tree a landed stash left, or badge why the stash
/// failed.
///
/// The stash put the working tree back to HEAD, so the clean buffers re-read
/// their files and every diff recomputes, as after a checkout.
fn land_stash(stoat: &mut Stoat, stashed: Result<String, String>) {
    let sha = match stashed {
        Ok(sha) => sha,
        Err(reason) => {
            emit_review_error_badge(stoat, "stash failed", Some(reason));
            return;
        },
    };

    auto_reload::reload_clean_buffers(stoat);
    stoat.active_workspace_mut().invalidate_all_diffs();
    stoat.set_status(format!("stashed {}", &sha[..sha.len().min(7)]));
}

/// Apply the newest stash to the working tree and drop it.
///
/// The pop waits its turn in the git queue behind every git write queued
/// before the press, and reads the stash list at its turn. A review walk or a
/// rebase owns HEAD until it ends, so either one refuses the command. A pop
/// that conflicts keeps the stash.
pub(super) fn git_stash_pop(stoat: &mut Stoat) -> UpdateEffect {
    queue_stash_pop(stoat, None);
    UpdateEffect::Redraw
}

/// Queue a pop of `stash`, or of the newest stash at the pop's turn for
/// `None`.
///
/// A stash that is gone by the pop's turn badges the host's refusal and pops
/// nothing.
pub(super) fn queue_stash_pop(stoat: &mut Stoat, stash: Option<CommitInfo>) {
    let job = GitJob::new("stash pop", None, move |stoat: &mut Stoat| {
        let (repo, _) = commit_repo(stoat)?;
        Some(Box::new(move || {
            let popped = match stash.or_else(|| repo.stash_list().into_iter().next()) {
                None => Err(("no stash", None)),
                Some(row) => match repo.stash_pop(&row.sha) {
                    Ok(()) => Ok(row),
                    Err(GitApplyError::Backend { reason, .. }) => {
                        Err(("stash pop failed", Some(reason)))
                    },
                },
            };
            Box::new(move |stoat: &mut Stoat| land_stash_pop(stoat, popped)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
}

/// Bring the editor to the tree a landed pop left and name the popped stash,
/// or badge why the pop did not run.
///
/// The pop rewrote the working tree, so the clean buffers re-read their files
/// and every diff recomputes, as after a stash.
fn land_stash_pop(stoat: &mut Stoat, popped: Result<CommitInfo, (&str, Option<String>)>) {
    let row = match popped {
        Ok(row) => row,
        Err((label, detail)) => {
            emit_review_error_badge(stoat, label, detail);
            return;
        },
    };

    auto_reload::reload_clean_buffers(stoat);
    stoat.active_workspace_mut().invalidate_all_diffs();
    stoat.set_status(format!("popped {} {}", row.short_sha, row.summary));
}

#[cfg(test)]
mod tests {
    use crate::{app::Stoat, badge::BadgeSource, test_harness::TestHarness};
    use std::path::{Path, PathBuf};

    /// A harness rooted in `/repo`, with HEAD on `main` at `c2` over `c1`, and
    /// `c.rs` edited in the working tree when `edited` holds.
    fn harness(edited: bool) -> TestHarness {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("c1", "one", &[("a.rs", "1\n")]),
                ("c2", "two", &[("a.rs", "2\n")]),
            ],
        );
        let mut repo = h.fake_git().add_repo("/repo");
        repo.branch("main", "c2").set_head_branch("main");
        if edited {
            repo.unstaged_file("c.rs", "edit");
        }
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    fn submit(h: &mut TestHarness, command: &str) {
        h.type_text(command);
        h.type_keys("enter");
        h.settle();
    }

    /// The sha and summary of each stash, newest first.
    fn stashes(h: &TestHarness) -> Vec<(String, String)> {
        h.stoat
            .git_host
            .discover(Path::new("/repo"))
            .expect("the repo")
            .stash_list()
            .into_iter()
            .map(|row| (row.sha, row.summary))
            .collect()
    }

    /// The path of each changed file of `/repo`.
    fn changed_paths(h: &TestHarness) -> Vec<PathBuf> {
        h.stoat
            .git_host
            .discover(Path::new("/repo"))
            .expect("the repo")
            .changed_files()
            .into_iter()
            .map(|file| file.path)
            .collect()
    }

    fn badge(h: &TestHarness) -> Option<(String, Option<String>)> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| (badge.label.clone(), badge.detail.clone()))
    }

    /// `:git stash` takes the edit out of the working tree under git's default
    /// message and names the stash in the status line.
    #[test]
    fn git_stash_saves_the_edit_under_the_default_message() {
        let mut h = harness(true);
        submit(&mut h, ":git stash");

        let listed = stashes(&h);
        let sha = listed.first().map(|(sha, _)| sha.clone()).expect("a stash");
        let changed = h
            .stoat
            .git_host
            .discover(Path::new("/repo"))
            .expect("the repo")
            .changed_files();
        assert_eq!(
            (changed, listed, h.stoat.pending_message.clone()),
            (
                Vec::new(),
                vec![(sha.clone(), "WIP on main: c2 two".to_string())],
                Some(format!("stashed {}", &sha[..sha.len().min(7)])),
            ),
        );
    }

    /// `:git stash push` saves under the message typed after it, which git
    /// writes after the branch.
    #[test]
    fn git_stash_push_saves_under_the_given_message() {
        let mut h = harness(true);
        submit(&mut h, ":git stash push wip");
        let summaries: Vec<String> = stashes(&h)
            .into_iter()
            .map(|(_, summary)| summary)
            .collect();
        assert_eq!(summaries, ["On main: wip"]);
    }

    /// A tree with no tracked change badges the refusal and stashes nothing.
    #[test]
    fn git_stash_over_a_clean_tree_badges_nothing_to_stash() {
        let mut h = harness(false);
        submit(&mut h, ":git stash");
        assert_eq!(
            (badge(&h), stashes(&h)),
            (
                Some((
                    "stash failed".to_string(),
                    Some("nothing to stash".to_string())
                )),
                Vec::new(),
            ),
        );
    }

    /// `:git spo` puts the newest stash back in the working tree, drops it,
    /// and names it in the status line.
    #[test]
    fn git_stash_pop_restores_the_newest_stash() {
        let mut h = harness(true);
        submit(&mut h, ":git stash");
        let (sha, summary) = stashes(&h).into_iter().next().expect("a stash");

        submit(&mut h, ":git spo");
        let short: String = sha.chars().take(7).collect();
        assert_eq!(
            (
                changed_paths(&h),
                stashes(&h),
                h.stoat.pending_message.clone()
            ),
            (
                vec![PathBuf::from("/repo/c.rs")],
                Vec::new(),
                Some(format!("popped {short} {summary}")),
            ),
        );
    }

    #[test]
    fn git_stash_pop_with_no_stash_badges_no_stash() {
        let mut h = harness(false);
        submit(&mut h, ":git spo");
        assert_eq!(badge(&h), Some(("no stash".to_string(), None)));
    }

    /// A pop the repository refuses badges the reason and keeps the stash.
    #[test]
    fn a_refused_stash_pop_badges_and_keeps_the_stash() {
        let mut h = harness(true);
        submit(&mut h, ":git stash");
        h.fake_git().add_repo("/repo").fail_apply_with("disk full");

        submit(&mut h, ":git spo");
        assert_eq!(
            (badge(&h), stashes(&h).len()),
            (
                Some((
                    "stash pop failed".to_string(),
                    Some("disk full".to_string())
                )),
                1,
            ),
        );
    }
}
