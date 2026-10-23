use crate::{
    action_handlers::review::emit_review_error_badge,
    app::{Stoat, UpdateEffect},
    commit_message::CommitMessageEditor,
    git_jobs::{self, GitJob, GitLanding, GitWork},
    host::{GitApplyError, GitRepo},
    input_view::{InputView, SubmitTarget},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Open the commit message screen over the staged changes, or badge why it
/// does not open.
///
/// The status read that lists the staged paths walks the working tree, so it
/// runs on the pool behind every git write queued before the press. A review
/// walk or a rebase owns HEAD until it ends, so either one refuses the command
/// at its turn. A screen already up stays as it is.
pub(super) fn git_commit(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().commit_message.is_some() {
        return UpdateEffect::None;
    }
    let job = GitJob::new("commit prepare", None, |stoat: &mut Stoat| {
        let (repo, workdir) = commit_repo(stoat)?;
        Some(Box::new(move || {
            let staged = staged_paths(&*repo, &workdir);
            Box::new(move |stoat: &mut Stoat| open_screen(stoat, workdir, staged)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Commit the index with the screen's message.
///
/// The commit waits its turn in the git queue, and the screen stays up until it
/// lands. A failed commit keeps the screen and its message, so the reader edits
/// the message again or closes the screen. A blank message closes the screen
/// with no commit.
pub(super) fn git_commit_confirm(stoat: &mut Stoat) -> UpdateEffect {
    let (workdir, message) = {
        let ws = stoat.active_workspace();
        let Some(screen) = ws.commit_message.as_ref() else {
            return UpdateEffect::None;
        };
        (
            screen.workdir.clone(),
            screen.input.text(ws).trim().to_string(),
        )
    };
    if message.is_empty() {
        close_screen(stoat);
        emit_review_error_badge(stoat, "commit aborted: empty message", None);
        return UpdateEffect::Redraw;
    }

    let job = GitJob::new("commit", None, move |stoat: &mut Stoat| {
        stoat.active_workspace().commit_message.as_ref()?;
        let Some(repo) = stoat.git_host.discover(&workdir) else {
            emit_review_error_badge(stoat, "not in a git repository", None);
            return None;
        };
        Some(Box::new(move || {
            let committed = match repo.commit_index(&message) {
                Ok(sha) => Ok(sha),
                Err(GitApplyError::Backend { reason, .. }) => Err(reason),
            };
            Box::new(move |stoat: &mut Stoat| land_commit(stoat, &message, committed)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Close the commit message screen without a commit.
pub(super) fn git_commit_abort(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().commit_message.is_none() {
        return UpdateEffect::None;
    }
    close_screen(stoat);
    UpdateEffect::Redraw
}

/// The repository and working tree a commit goes into at the job's turn, or
/// `None` after badging why the commit does not run.
fn commit_repo(stoat: &mut Stoat) -> Option<(Arc<dyn GitRepo>, PathBuf)> {
    let ws = stoat.active_workspace();
    let refusal = if ws.review_walk.is_some() {
        Some("a review walk is open")
    } else if ws.rebase_active.is_some() {
        Some("a rebase is in progress")
    } else {
        None
    };
    if let Some(label) = refusal {
        emit_review_error_badge(stoat, label, None);
        return None;
    }

    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        emit_review_error_badge(stoat, "not in a git repository", None);
        return None;
    };
    let Some(workdir) = repo.workdir() else {
        emit_review_error_badge(stoat, "git repo has no working tree", None);
        return None;
    };
    Some((repo, workdir))
}

/// The staged paths of `repo`, relative to `workdir`, on whatever thread calls.
fn staged_paths(repo: &dyn GitRepo, workdir: &Path) -> Vec<PathBuf> {
    repo.changed_files()
        .into_iter()
        .filter(|file| file.staged)
        .map(|file| {
            let relative = file.path.strip_prefix(workdir).map(Path::to_path_buf);
            relative.unwrap_or(file.path)
        })
        .collect()
}

/// Open the screen over `staged`, or badge that the index holds no change.
fn open_screen(stoat: &mut Stoat, workdir: PathBuf, staged: Vec<PathBuf>) {
    if staged.is_empty() {
        emit_review_error_badge(stoat, "nothing staged", None);
        return;
    }
    install_commit_message(stoat, workdir, staged);
}

/// Put the commit message screen up over the focused pane, for the `staged`
/// paths of the repository at `workdir`.
///
/// The message starts empty and in insert mode, so the reader types it at once.
/// A screen already up stays as it is.
fn install_commit_message(stoat: &mut Stoat, workdir: PathBuf, staged: Vec<PathBuf>) {
    if stoat.active_workspace().commit_message.is_some() {
        return;
    }
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = InputView::create(ws, executor, SubmitTarget::CommitMessage, "", "insert");
    ws.commit_message = Some(CommitMessageEditor {
        workdir,
        staged,
        input,
    });
}

/// Close the screen over a landed commit and bring the editor to the new HEAD,
/// or badge a failed commit and keep the screen.
///
/// A screen the reader closed while the commit ran stays closed. The commit
/// still moved HEAD, so the diffs and the commits lists follow either way.
fn land_commit(stoat: &mut Stoat, message: &str, committed: Result<String, String>) {
    let sha = match committed {
        Ok(sha) => sha,
        Err(reason) => {
            emit_review_error_badge(stoat, "commit failed", Some(reason));
            return;
        },
    };

    close_screen(stoat);
    stoat.active_workspace_mut().invalidate_all_diffs();
    super::commits::reload_every_list(stoat);
    let first_line = message.lines().next().unwrap_or("");
    stoat.set_status(format!(
        "committed {} {first_line}",
        &sha[..sha.len().min(7)]
    ));
}

/// Close the commit message screen and dispose its editor.
fn close_screen(stoat: &mut Stoat) {
    let ws = stoat.active_workspace_mut();
    if let Some(screen) = ws.commit_message.take() {
        screen.input.dispose(ws);
    }
}

#[cfg(test)]
mod tests {
    use super::install_commit_message;
    use crate::{
        action_handlers,
        app::Stoat,
        badge::BadgeSource,
        command_palette::Availability,
        git_jobs::{self, GitJob, GitLanding, GitWork},
        rebase::{ActiveRebase, RebaseState},
        test_harness::TestHarness,
    };
    use std::path::{Path, PathBuf};
    use stoat_action::{GitCommitAbort, GitCommitConfirm};

    /// A harness rooted in `/repo`, with HEAD on `main` at `c2` over `c1`, and
    /// each of `staged` staged with its text in the index.
    fn harness(staged: &[(&str, &str)]) -> TestHarness {
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
        for (path, text) in staged {
            repo.staged_file(path, text).index_file(path, text);
        }
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    fn submit(h: &mut TestHarness, command: &str) {
        h.type_text(command);
        h.type_keys("enter");
        h.settle();
    }

    fn head(h: &TestHarness) -> Option<String> {
        h.stoat
            .git_host
            .discover(Path::new("/repo"))
            .and_then(|repo| repo.resolve_rev("HEAD"))
    }

    fn head_message(h: &TestHarness) -> Option<String> {
        h.fake_git().commit_message(Path::new("/repo"), &head(h)?)
    }

    /// The message on the screen, or `None` with no screen up.
    fn screen_text(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.commit_message
            .as_ref()
            .map(|screen| screen.input.text(ws))
    }

    fn review_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }

    /// The screen opens in insert mode under its own view, and the palette
    /// reads it as up.
    #[test]
    fn the_screen_opens_in_insert_mode_under_its_own_view() {
        let mut h = harness(&[("a.rs", "3\n")]);
        install_commit_message(&mut h.stoat, "/repo".into(), vec!["a.rs".into()]);
        assert_eq!(
            (
                h.stoat.current_view(),
                h.stoat.focused_mode(),
                Availability::from_stoat(&h.stoat).in_commit_message,
            ),
            (Some("commit_message"), "insert", true),
        );
    }

    /// A second install while the screen is up keeps the screen and the message
    /// typed into it.
    #[test]
    fn a_second_install_keeps_the_screen_and_its_message() {
        let mut h = harness(&[("a.rs", "3\n")]);
        install_commit_message(&mut h.stoat, "/repo".into(), vec!["a.rs".into()]);
        h.type_text("feat: x");
        install_commit_message(&mut h.stoat, "/repo".into(), vec!["b.rs".into()]);
        let staged = h
            .stoat
            .active_workspace()
            .commit_message
            .as_ref()
            .map(|screen| screen.staged.clone());
        assert_eq!(
            (screen_text(&h), staged),
            (
                Some("feat: x".to_string()),
                Some(vec![PathBuf::from("a.rs")])
            ),
        );
    }

    /// `:git commit` and `:git ci` open the screen over the staged paths, and
    /// an unstaged edit stays off it.
    #[test]
    fn git_commit_opens_the_screen_over_the_staged_paths() {
        let staged = |command: &str| {
            let mut h = harness(&[("a.rs", "3\n")]);
            h.fake_git().add_repo("/repo").unstaged_file("c.rs", "edit");
            submit(&mut h, command);
            let ws = h.stoat.active_workspace();
            ws.commit_message
                .as_ref()
                .map(|screen| screen.staged.clone())
        };
        assert_eq!(
            [staged(":git commit"), staged(":git ci")],
            [
                Some(vec![PathBuf::from("a.rs")]),
                Some(vec![PathBuf::from("a.rs")])
            ],
        );
    }

    /// Ctrl-s commits the index with the message, closes the screen, and names
    /// the commit in the status line.
    #[test]
    fn ctrl_s_commits_the_index_with_the_message() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        h.type_text("feat: x");
        h.type_keys("ctrl-s");
        h.settle();

        let sha = head(&h).expect("a HEAD");
        assert_eq!(
            (
                screen_text(&h),
                head_message(&h),
                h.stoat.pending_message.clone(),
                h.fake_git().commit_message(Path::new("/repo"), "c2"),
            ),
            (
                None,
                Some("feat: x".to_string()),
                Some(format!("committed {} feat: x", &sha[..7])),
                Some("two".to_string()),
            ),
        );
    }

    /// The commit moves HEAD under the diffs, so each one recomputes against
    /// the new HEAD, which holds the text that was staged.
    #[test]
    fn a_commit_rediffs_against_the_new_head() {
        let mut h = harness(&[("a.rs", "3\n")]);
        h.fake_git().add_repo("/repo").head_file("a.rs", "2\n");
        h.fake_fs().insert_file("/repo/a.rs", "3\n");
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

        submit(&mut h, ":git commit");
        h.type_text("feat: x");
        h.type_keys("ctrl-s");
        h.settle();
        h.settle_diff_jobs();
        assert_eq!(
            (before.as_deref(), base(&h).as_deref()),
            (Some("2\n"), Some("3\n")),
        );
    }

    /// Every commits list reloads with the new commit on top.
    #[test]
    fn a_commit_reloads_the_commits_lists() {
        let mut h = harness(&[("a.rs", "3\n")]);
        h.open_commits("/repo");
        submit(&mut h, ":git commit");
        h.type_text("feat: x");
        h.type_keys("ctrl-s");
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
        assert_eq!(
            listed,
            [
                head(&h).expect("a HEAD"),
                "c2".to_string(),
                "c1".to_string()
            ],
        );
    }

    /// A screen closed while its commit waits behind another git write commits
    /// nothing at the commit's turn.
    #[test]
    fn a_screen_closed_while_its_commit_waits_commits_nothing() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        h.type_text("feat: x");
        let hold = GitJob::new("hold", None, |_: &mut Stoat| {
            Some(Box::new(|| Box::new(|_: &mut Stoat| {}) as GitLanding) as GitWork)
        });
        git_jobs::enqueue(&mut h.stoat, hold);
        // A dispatch paints no frame and pumps no job, so the hold stays out
        // until the settle and the commit waits behind it.
        action_handlers::dispatch(&mut h.stoat, &GitCommitConfirm);
        action_handlers::dispatch(&mut h.stoat, &GitCommitAbort);
        h.settle();
        assert_eq!((screen_text(&h), head(&h)), (None, Some("c2".to_string())));
    }

    /// Escape leaves insert mode, and Escape in normal mode closes the screen
    /// with no commit.
    #[test]
    fn escape_twice_closes_the_screen_with_no_commit() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        h.type_text("feat: x");
        h.type_keys("escape escape");
        h.settle();
        assert_eq!((screen_text(&h), head(&h)), (None, Some("c2".to_string())));
    }

    #[test]
    fn a_blank_message_closes_the_screen_with_no_commit() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        h.type_text("  ");
        h.type_keys("ctrl-s");
        h.settle();
        assert_eq!(
            (screen_text(&h), head(&h), review_badge(&h)),
            (
                None,
                Some("c2".to_string()),
                Some("commit aborted: empty message".to_string()),
            ),
        );
    }

    /// A commit the repository refuses keeps the screen and its message, so the
    /// reader edits it again or closes the screen.
    #[test]
    fn a_failed_commit_keeps_the_screen_and_its_message() {
        let mut h = harness(&[("a.rs", "3\n")]);
        h.fake_git().add_repo("/repo").fail_apply_with("disk full");
        submit(&mut h, ":git commit");
        h.type_text("feat: x");
        h.type_keys("ctrl-s");
        h.settle();
        assert_eq!(
            (screen_text(&h), head(&h), review_badge(&h)),
            (
                Some("feat: x".to_string()),
                Some("c2".to_string()),
                Some("commit failed".to_string()),
            ),
        );
    }

    /// The reader stages hunks in the diff view and commits from there, so the
    /// screen outranks the diff view and takes the keys over it.
    #[test]
    fn the_screen_takes_the_keys_over_a_diff_view() {
        let mut h = harness(&[("a.rs", "3\n")]);
        h.fake_fs().insert_file("/repo/a.rs", "3\n");
        h.open_file(Path::new("/repo/a.rs"));
        let editor_id = h.stoat.focused_editor_ids().expect("an editor").0;
        h.stoat
            .active_workspace_mut()
            .editors
            .get_mut(editor_id)
            .expect("the editor")
            .set_diff_view(true);
        let under = h.stoat.current_view();

        submit(&mut h, ":git commit");
        let over = h.stoat.current_view();
        h.type_text("fix: y");
        h.type_keys("ctrl-s");
        h.settle();
        assert_eq!(
            (under, over, head_message(&h)),
            (
                Some("diff"),
                Some("commit_message"),
                Some("fix: y".to_string())
            ),
        );
    }

    /// An index with no change badges and opens nothing, even over an unstaged
    /// edit.
    #[test]
    fn git_commit_over_a_clean_index_badges_and_opens_nothing() {
        let mut h = harness(&[]);
        h.fake_git().add_repo("/repo").unstaged_file("c.rs", "edit");
        submit(&mut h, ":git commit");
        assert_eq!(
            (screen_text(&h), review_badge(&h)),
            (None, Some("nothing staged".to_string())),
        );
    }

    /// A review walk or a rebase owns HEAD until it ends, so either one refuses
    /// the command.
    #[test]
    fn git_commit_during_a_walk_or_a_rebase_opens_nothing() {
        let refused = |h: &mut TestHarness| {
            submit(h, ":git commit");
            (screen_text(h), review_badge(h))
        };
        let mut walking = harness(&[]);
        submit(&mut walking, ":git review main");
        walking.type_keys("down");
        walking.type_keys("enter");
        walking.settle();
        assert!(
            walking.stoat.active_workspace().review_walk.is_some(),
            "the walk must be running for the refusal"
        );
        let mut rebasing = harness(&[("a.rs", "3\n")]);
        rebasing.stoat.active_workspace_mut().rebase_active = Some(ActiveRebase::new(
            RebaseState::new("/repo".into(), "c1".into(), Vec::new()),
            None,
        ));
        assert_eq!(
            [refused(&mut walking), refused(&mut rebasing)],
            [
                (None, Some("a review walk is open".to_string())),
                (None, Some("a rebase is in progress".to_string())),
            ],
        );
    }

    #[test]
    fn snapshot_commit_message_screen() {
        let mut h = harness(&[("a.rs", "3\n"), ("b.rs", "new\n")]);
        h.resize(90, 12);
        submit(&mut h, ":git commit");
        h.type_text("feat: x");
        assert_eq!(h.stoat.current_view(), Some("commit_message"));
        h.assert_snapshot("commit_message_screen");
    }
}
