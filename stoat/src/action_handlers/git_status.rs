//! The git status panel's actions, which open it, read its model, walk its
//! rows, and close it.

use crate::{
    action_handlers::review,
    app::{Stoat, UpdateEffect},
    git_jobs::{self, GitJob, GitLanding, GitWork},
    git_status::{GitStatusModel, GitStatusPanel, RebaseProgress},
};

/// Open the git status panel and queue the read of its model.
///
/// Outside a git repository the panel does not open, and a review badge says
/// why.
pub(super) fn open_git_status(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    if stoat.git_host.discover(&git_root).is_none() {
        review::emit_review_error_badge(stoat, "not in a git repository", None);
        return UpdateEffect::Redraw;
    }

    stoat.set_focused_mode("normal".into());
    stoat.git_status = Some(GitStatusPanel::default());
    reload_git_status(stoat);
    UpdateEffect::Redraw
}

pub(super) fn close_git_status(stoat: &mut Stoat) -> UpdateEffect {
    stoat.git_status = None;
    UpdateEffect::Redraw
}

/// Move the panel's selection `delta` rows.
pub(super) fn git_status_step(stoat: &mut Stoat, delta: isize) -> UpdateEffect {
    let Some(panel) = stoat.git_status.as_mut() else {
        return UpdateEffect::None;
    };
    panel.step(delta);
    UpdateEffect::Redraw
}

/// Queue a read of the panel's model behind every git write queued before it.
///
/// The read waits its turn in the git queue, so it sees the repository that
/// every earlier write left. Its inputs are taken when its turn comes. A
/// panel closed by then reads nothing, and one closed while the read runs
/// takes no model.
fn reload_git_status(stoat: &mut Stoat) {
    let job = GitJob::new("git status read", None, |stoat: &mut Stoat| {
        stoat.git_status.as_ref()?;
        let ws = stoat.active_workspace();
        let repo = stoat.git_host.discover(&ws.git_root)?;
        let rebase = ws.rebase_active.as_ref().map(RebaseProgress::of);
        let resolved = ws.resolved_conflicts.clone();
        let fs = stoat.fs_host.clone();
        let languages = stoat.language_registry.clone();
        Some(Box::new(move || {
            let model = GitStatusModel::read(&*repo, &*fs, &languages, rebase.as_ref(), &resolved);
            Box::new(move |stoat: &mut Stoat| {
                if let Some(panel) = stoat.git_status.as_mut() {
                    panel.land(model);
                }
            }) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch,
        app::Stoat,
        git_status::Note,
        host::FakeRepoBuilder,
        keymap_state::{active_modal, ActiveModal},
        test_harness::TestHarness,
    };
    use std::path::PathBuf;

    /// A harness rooted in `/repo`, with HEAD on `main` and the files
    /// `seed` registers.
    fn repo_harness(seed: impl FnOnce(&mut FakeRepoBuilder<'_>)) -> TestHarness {
        let mut h = Stoat::test();
        {
            let mut repo = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            repo.set_head_branch("main");
            seed(&mut repo);
        }
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    fn selected(h: &TestHarness) -> Option<usize> {
        h.stoat.git_status.as_ref().map(|panel| panel.selected)
    }

    /// Run `command` on the command line and let the panel's read land.
    fn run(h: &mut TestHarness, command: &str) {
        h.type_text(command);
        h.type_keys("enter");
        h.settle();
    }

    /// `:git status` and `:git st` open the panel on the repository's read,
    /// and Escape closes it.
    #[test]
    fn the_status_command_opens_the_panel_and_escape_closes_it() {
        let opened = |command: &str| {
            let mut h = repo_harness(|_| {});
            run(&mut h, command);
            let opened = (
                active_modal(&h.stoat),
                h.stoat
                    .git_status
                    .as_ref()
                    .and_then(|panel| panel.model.as_ref())
                    .and_then(|model| model.branch.clone()),
            );
            h.type_keys("Escape");
            (opened, active_modal(&h.stoat))
        };

        let open_then_closed = (
            (Some(ActiveModal::GitStatus), Some("main".to_string())),
            None,
        );
        assert_eq!(
            [opened(":git status"), opened(":git st")],
            [open_then_closed.clone(), open_then_closed],
        );
    }

    #[test]
    fn the_panel_does_not_open_outside_a_repository() {
        let mut h = Stoat::test();
        dispatch(&mut h.stoat, &stoat_action::GitStatus);
        h.settle();
        assert_eq!(active_modal(&h.stoat), None);
    }

    /// j and k walk the files of every section, and stop at the first and
    /// the last file.
    #[test]
    fn j_and_k_walk_the_files_and_stop_at_the_ends() {
        let mut h = repo_harness(|repo| {
            repo.head_file("a.rs", "a")
                .staged_file("a.rs", "b")
                .untracked("b.rs");
        });
        run(&mut h, ":git status");

        let walk: Vec<Option<usize>> = ["j", "j", "k", "k"]
            .into_iter()
            .map(|key| {
                h.type_keys(key);
                selected(&h)
            })
            .collect();
        assert_eq!(walk, [Some(1), Some(1), Some(0), Some(0)]);
    }

    /// A landed tally keeps the resolution notes while a merge is in
    /// progress, and drops them once no merge is.
    #[test]
    fn a_tally_with_no_merge_drops_the_resolution_notes() {
        let notes_after_tally = |merging: bool| {
            let mut h = repo_harness(|repo| {
                if merging {
                    repo.merge_in_progress("f00dfeed", Some("feature"), "Merge");
                }
            });
            h.stoat
                .active_workspace_mut()
                .resolved_conflicts
                .insert(PathBuf::from("/repo/a.rs"), Note::Resolved);
            h.stoat.set_diff_warm_auto(true);
            h.settle_diff_jobs();
            h.stoat.active_workspace().resolved_conflicts.len()
        };

        assert_eq!([notes_after_tally(true), notes_after_tally(false)], [1, 0]);
    }

    /// The board's dirty state at 80 by 20, as the cells hold it.
    #[test]
    fn snapshot_git_status_dirty() {
        let mut h = repo_harness(|repo| {
            repo.commit("c1", &[])
                .commit_with_parent("c2", "c1", &[])
                .branch("main", "c1")
                .branch("ermine-tokens", "c2")
                .set_head("c2")
                .set_head_branch("ermine-tokens")
                .head_file("stoat/src/theme.rs", "a")
                .staged_file("stoat/src/theme.rs", "b")
                .lines("stoat/src/theme.rs", 6, 0)
                .modified("stoat/src/render/chrome.rs", "a", "b")
                .lines("stoat/src/render/chrome.rs", 5, 6)
                .modified("config.stcfg", "a", "b")
                .lines("config.stcfg", 14, 2)
                .untracked("themes/ermine-night.json");
        });
        h.resize(80, 20);
        run(&mut h, ":git status");
        h.assert_snapshot("git_status_dirty");
    }
}
