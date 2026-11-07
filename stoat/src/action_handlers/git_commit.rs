use crate::{
    action_handlers::review::emit_review_error_badge,
    agent_ipc::{self, BridgeOutcome},
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    buffer_registry::OpenOrigin,
    commit_message::{self, CommitDraft, CommitWait, StagedChange},
    git_jobs::{self, GitJob, GitLanding, GitWork},
    host::{FsHost, GitApplyError, GitRepo},
    pane::{Axis, FocusTarget},
    workspace::BridgeWaiter,
};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc::{self, error::TryRecvError};

/// What a commit of a message file came to.
enum Committed {
    /// The commit landed as `sha`, with the cleaned `message`.
    Landed { sha: String, message: String },
    /// The file cleaned to nothing, so nothing was committed.
    Empty,
    /// The file did not read, for the reason given.
    Unread(String),
    /// The repository refused the commit for `reason`, and `rewritten` is how
    /// the write of the message back to the file went.
    Refused {
        reason: String,
        rewritten: io::Result<()>,
    },
}

/// Open the repository's COMMIT_EDITMSG in a new split over git's message
/// template for the staged changes, and commit when the buffer leaves the
/// editor clean.
///
/// The status read that lists the staged paths walks the working tree, so it
/// runs on the pool behind every git write queued before the press, and the
/// template write runs with it. A review walk or a rebase owns HEAD until it
/// ends, so either one refuses the command at its turn. A message buffer
/// already held stays as it is.
pub(super) fn git_commit(stoat: &mut Stoat) -> UpdateEffect {
    queue_message_open(stoat, false)
}

/// Open the repository's COMMIT_EDITMSG in a new split over HEAD's message and
/// git's template, and amend HEAD when the buffer leaves the editor clean.
///
/// The message opens with nothing staged too, because an amend of the message
/// alone is valid. A review walk or a rebase owns HEAD until it ends, so
/// either one refuses the command at its turn. A message buffer already held
/// stays as it is.
pub(super) fn git_amend(stoat: &mut Stoat) -> UpdateEffect {
    queue_message_open(stoat, true)
}

/// Start the commit that the held message buffer waits on once the buffer
/// leaves the editor, and answer whether anything happened.
///
/// A clean close commits what the file holds, which is what the last write
/// left. A close over unsaved edits commits nothing, as git commits nothing
/// when its editor exits without a write. Only the active workspace's wait is
/// read, because the commit lands in the active workspace.
pub(crate) fn pump_commit_wait(stoat: &mut Stoat) -> bool {
    let ws = stoat.active_workspace_mut();
    let Some(mut wait) = ws.commit_wait.take() else {
        return false;
    };
    let outcome = match wait.outcome.try_recv() {
        Ok(outcome) => Some(outcome),
        Err(TryRecvError::Empty) => {
            ws.commit_wait = Some(wait);
            return false;
        },
        Err(TryRecvError::Disconnected) => None,
    };

    match outcome {
        Some(BridgeOutcome::Closed) => queue_commit(stoat, wait.draft),
        Some(BridgeOutcome::Abandoned) | None => emit_review_error_badge(
            stoat,
            "commit aborted: message closed without a write",
            None,
        ),
    }
    true
}

/// Stage every tracked change in the working tree, as `git add -u` does.
///
/// The stage waits its turn in the git queue behind every git write queued
/// before the press. A review walk or a rebase owns HEAD until it ends, so
/// either one refuses the command at its turn. Untracked files stay out of the
/// index.
pub(super) fn git_add(stoat: &mut Stoat) -> UpdateEffect {
    let job = GitJob::new("stage tracked", None, |stoat: &mut Stoat| {
        let (repo, _) = commit_repo(stoat)?;
        Some(Box::new(move || {
            let staged = match repo.stage_tracked() {
                Ok(count) => Ok(count),
                Err(GitApplyError::Backend { reason, .. }) => Err(reason),
            };
            Box::new(move |stoat: &mut Stoat| land_stage(stoat, staged)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Name how many files a landed stage moved into the index, and rediff every
/// buffer against the index it changed.
///
/// A stage that moved nothing, or that the repository refused, badges instead.
fn land_stage(stoat: &mut Stoat, staged: Result<usize, String>) {
    match staged {
        Ok(0) => emit_review_error_badge(stoat, "nothing to stage", None),
        Ok(count) => {
            stoat.active_workspace_mut().invalidate_all_diffs();
            let noun = if count == 1 { "file" } else { "files" };
            stoat.set_status(format!("staged {count} {noun}"));
        },
        Err(reason) => emit_review_error_badge(stoat, "stage failed", Some(reason)),
    }
}

/// The repository and working tree a git write acts on at the job's turn, or
/// `None` after badging why the write does not run.
///
/// A review walk or a rebase owns HEAD until it ends, so either one refuses
/// the write. The commit, stage, and stash commands share this check.
pub(super) fn commit_repo(stoat: &mut Stoat) -> Option<(Arc<dyn GitRepo>, PathBuf)> {
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

/// The staged changes of `repo`, with paths relative to `workdir`, on whatever
/// thread calls.
fn staged_changes(repo: &dyn GitRepo, workdir: &Path) -> Vec<StagedChange> {
    let relative = |path: PathBuf| {
        let stripped = path.strip_prefix(workdir).map(Path::to_path_buf);
        stripped.unwrap_or(path)
    };
    repo.changed_files()
        .into_iter()
        .filter_map(|file| {
            Some(StagedChange {
                kind: file.staged_kind?,
                path: relative(file.path),
                from: file.renamed_from.map(relative),
            })
        })
        .collect()
}

/// Queue the template write and the open of the message buffer, for a commit
/// or, with `amend`, for an amend of HEAD.
fn queue_message_open(stoat: &mut Stoat, amend: bool) -> UpdateEffect {
    if stoat.active_workspace().commit_wait.is_some() {
        return UpdateEffect::None;
    }
    let label = if amend {
        "amend prepare"
    } else {
        "commit prepare"
    };
    let job = GitJob::new(label, None, move |stoat: &mut Stoat| {
        let (repo, workdir) = commit_repo(stoat)?;
        let fs = stoat.fs_host.clone();
        Some(Box::new(move || {
            let prepared = prepare_draft(&*fs, &*repo, workdir, amend);
            Box::new(move |stoat: &mut Stoat| match prepared {
                Ok((draft, written)) => open_message_buffer(stoat, draft, written),
                Err(label) => emit_review_error_badge(stoat, label, None),
            }) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Write the message template for a commit, or with `amend` for an amend, on
/// whatever thread calls.
///
/// Answers the draft and how the write went. When there is nothing to commit,
/// it answers the badge label instead. A commit needs a staged change, and an
/// amend needs a HEAD.
///
/// A commit during a merge opens over the merge's message and needs no staged
/// change, since a merge whose conflicts all took HEAD's side changes no file.
fn prepare_draft(
    fs: &dyn FsHost,
    repo: &dyn GitRepo,
    workdir: PathBuf,
    amend: bool,
) -> Result<(CommitDraft, io::Result<()>), &'static str> {
    let staged = staged_changes(repo, &workdir);
    let merge = if amend { None } else { repo.merge_state() };
    let message = if amend {
        let head = repo
            .resolve_rev("HEAD")
            .and_then(|head| repo.commit_message(&head));
        head.ok_or("no commit to amend")?.trim_end().to_string()
    } else if let Some(merge) = &merge {
        merge.message.trim_end().to_string()
    } else if staged.is_empty() {
        return Err("nothing staged");
    } else {
        String::new()
    };

    let draft = CommitDraft {
        workdir,
        path: repo.git_dir().join("COMMIT_EDITMSG"),
        staged,
        branch: repo.head_branch(),
        amend,
        merge: merge.is_some(),
    };
    let written = write_message(fs, &draft, &message);
    Ok((draft, written))
}

/// Write `message` above git's template to the draft's file.
fn write_message(fs: &dyn FsHost, draft: &CommitDraft, message: &str) -> io::Result<()> {
    let text = commit_message::template(message, draft.branch.as_deref(), &draft.staged);
    fs.write(&draft.path, text.as_bytes())
}

/// Open the draft's file as a buffer in a new split and hold it, so the commit
/// lands when the buffer leaves the editor.
///
/// `written` is how the write of the file went. A clean buffer already open on
/// the file closes first, since it holds the text from before the write. A
/// dirty one keeps its edits, and the open badges instead. A message buffer
/// already held stays as it is.
fn open_message_buffer(stoat: &mut Stoat, draft: CommitDraft, written: io::Result<()>) {
    if let Err(err) = written {
        emit_review_error_badge(stoat, "commit message not written", Some(err.to_string()));
        return;
    }
    let (held, open) = {
        let ws = stoat.active_workspace();
        let open = ws.buffers.id_for_path(&draft.path).map(|id| {
            let dirty = ws
                .buffers
                .get(id)
                .is_some_and(|buffer| buffer.read().expect("buffer poisoned").dirty);
            (id, dirty)
        });
        (ws.commit_wait.is_some(), open)
    };
    if held {
        return;
    }
    match open {
        Some((_, true)) => {
            emit_review_error_badge(stoat, "commit message buffer holds unsaved edits", None);
            return;
        },
        Some((id, false)) => {
            buffer_lifecycle::close_buffer_by_id(stoat, id);
        },
        None => {},
    }

    let pane = {
        let executor = stoat.executor.clone();
        let ws = stoat.active_workspace_mut();
        let pane = ws.panes.split(Axis::Vertical);
        // The split copies the focused view, so the pane takes an editor of its
        // own before the open fills it.
        let scratch = super::pane::scratch_editor_view(ws, executor);
        ws.panes.pane_mut(pane).view = scratch;
        ws.focus = FocusTarget::SplitPane;
        pane
    };
    let opened =
        buffer_lifecycle::open_file_in_pane_now(stoat, pane, &draft.path, OpenOrigin::Named);
    let Some(buffer) = opened else {
        super::pane::close_pane_by_id(stoat, pane);
        emit_review_error_badge(stoat, "commit message did not open", None);
        return;
    };

    let (done, outcome) = mpsc::unbounded_channel();
    let label = if draft.amend {
        "git amend"
    } else {
        "git commit"
    };
    let ws = stoat.active_workspace_mut();
    ws.hold_buffer(
        buffer,
        BridgeWaiter {
            client: agent_ipc::next_client(),
            label: label.to_string(),
            done,
        },
    );
    ws.commit_wait = Some(CommitWait { draft, outcome });
}

/// Queue the commit of the draft's file, or the amend of HEAD for an amend
/// draft.
///
/// The commit waits its turn in the git queue behind every git write queued
/// before the close. The file is read at that turn and cleaned as git cleans
/// it, and an empty message commits nothing.
fn queue_commit(stoat: &mut Stoat, draft: CommitDraft) {
    let label = if draft.amend { "amend" } else { "commit" };
    let fs = stoat.fs_host.clone();
    let job = GitJob::new(label, None, move |stoat: &mut Stoat| {
        let Some(repo) = stoat.git_host.discover(&draft.workdir) else {
            emit_review_error_badge(stoat, "not in a git repository", None);
            return None;
        };
        Some(Box::new(move || {
            let committed = commit_draft(&*fs, &*repo, &draft);
            Box::new(move |stoat: &mut Stoat| land_commit(stoat, draft, committed)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
}

/// Read the draft's file, clean it, and commit or amend with what is left, on
/// whatever thread calls.
///
/// A refused commit writes the cleaned message back above git's template, so
/// the file that opens again holds it.
fn commit_draft(fs: &dyn FsHost, repo: &dyn GitRepo, draft: &CommitDraft) -> Committed {
    let text = match super::read_string_via_host(fs, &draft.path) {
        Ok(text) => text,
        Err(err) => return Committed::Unread(err.to_string()),
    };
    let message = commit_message::cleanup(&text);
    if message.is_empty() {
        return Committed::Empty;
    }

    let written = if draft.amend {
        repo.amend_index(&message)
    } else {
        repo.commit_index(&message)
    };
    match written {
        Ok(sha) => Committed::Landed { sha, message },
        Err(GitApplyError::Backend { reason, .. }) => Committed::Refused {
            reason,
            rewritten: write_message(fs, draft, &message),
        },
    }
}

/// Bring the editor to the HEAD a landed commit left, or badge why the commit
/// did not land.
///
/// A refused commit opens the message again, so the reader edits it and
/// retries or closes it. The status line names an amend as one.
fn land_commit(stoat: &mut Stoat, draft: CommitDraft, committed: Committed) {
    let (sha, message) = match committed {
        Committed::Landed { sha, message } => (sha, message),
        Committed::Empty => {
            emit_review_error_badge(stoat, "commit aborted: empty message", None);
            return;
        },
        Committed::Unread(reason) => {
            emit_review_error_badge(stoat, "commit message not read", Some(reason));
            return;
        },
        Committed::Refused { reason, rewritten } => {
            emit_review_error_badge(stoat, "commit failed", Some(reason));
            open_message_buffer(stoat, draft, rewritten);
            return;
        },
    };

    stoat.active_workspace_mut().invalidate_all_diffs();
    super::commits::reload_every_list(stoat);
    let first_line = message.lines().next().unwrap_or("");
    let verb = if draft.amend {
        "amended"
    } else if draft.merge {
        "merged"
    } else {
        "committed"
    };
    stoat.set_status(format!("{verb} {} {first_line}", &sha[..sha.len().min(7)]));
}

#[cfg(test)]
mod tests {
    use crate::{
        app::Stoat,
        badge::BadgeSource,
        buffer::BufferId,
        commit_message::{template, StagedChange},
        host::{FsHost, StagedKind},
        rebase::{ActiveRebase, RebaseState},
        test_harness::TestHarness,
    };
    use std::{
        io::ErrorKind,
        path::{Path, PathBuf},
    };

    /// Where the fake repository at `/repo` keeps its commit message.
    const MESSAGE: &str = "/repo/.git/COMMIT_EDITMSG";

    /// A staged edit of `path`, which HEAD holds.
    fn modified(path: &str) -> StagedChange {
        StagedChange {
            kind: StagedKind::Modified,
            path: path.into(),
            from: None,
        }
    }

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

    /// Type `message` at the cursor in insert mode, then return to normal mode.
    fn type_message(h: &mut TestHarness, message: &str) {
        h.type_keys("i");
        h.type_text(message);
        h.type_keys("escape");
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

    /// The open COMMIT_EDITMSG buffer, or `None` while the file is not open.
    fn message_buffer(h: &TestHarness) -> Option<BufferId> {
        h.stoat
            .active_workspace()
            .buffers
            .id_for_path(Path::new(MESSAGE))
    }

    /// The text of COMMIT_EDITMSG on the fake disk, or `None` before a write.
    fn message_file(h: &TestHarness) -> Option<String> {
        let mut bytes = Vec::new();
        h.fake_fs().read(Path::new(MESSAGE), &mut bytes).ok()?;
        String::from_utf8(bytes).ok()
    }

    fn pane_count(h: &TestHarness) -> usize {
        h.stoat.active_workspace().panes.split_pane_ids().len()
    }

    fn waiting(h: &TestHarness) -> bool {
        h.stoat.active_workspace().commit_wait.is_some()
    }

    fn review_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }

    /// Each changed path of `/repo`, relative to it, with whether the index
    /// holds the change.
    fn staged_flags(h: &TestHarness) -> Vec<(PathBuf, bool)> {
        let repo = h
            .stoat
            .git_host
            .discover(Path::new("/repo"))
            .expect("the repo");
        repo.changed_files()
            .into_iter()
            .map(|file| {
                (
                    file.path.strip_prefix("/repo").expect("under /repo").into(),
                    file.staged,
                )
            })
            .collect()
    }

    /// `:git commit` writes git's template to COMMIT_EDITMSG and opens it in a
    /// split in normal mode, and the status row names what waits on it.
    #[test]
    fn git_commit_opens_the_message_buffer_in_a_split() {
        let mut h = harness(&[("a.rs", "3\n")]);
        h.resize(120, 24);
        submit(&mut h, ":git commit");
        h.snapshot();

        let focused = h.stoat.focused_editor_ids().map(|(_, buffer)| buffer);
        assert_eq!(
            (
                pane_count(&h),
                focused.is_some() && focused == message_buffer(&h),
                h.stoat.focused_mode(),
                message_file(&h),
                h.rendered_text()
                    .contains("COMMIT_EDITMSG [for git commit]"),
            ),
            (
                2,
                true,
                "normal",
                Some(template("", Some("main"), &[modified("a.rs")])),
                true,
            ),
        );
    }

    /// `:git commit` and `:git ci` write the staged paths into the template,
    /// and an unstaged edit stays off it.
    #[test]
    fn git_commit_lists_the_staged_paths_in_the_template() {
        let written = |command: &str| {
            let mut h = harness(&[("a.rs", "3\n")]);
            h.fake_git().add_repo("/repo").unstaged_file("c.rs", "edit");
            submit(&mut h, command);
            message_file(&h)
        };
        let listed = Some(template("", Some("main"), &[modified("a.rs")]));
        assert_eq!(
            [written(":git commit"), written(":git ci")],
            [listed.clone(), listed],
        );
    }

    /// `:git add` and `:git au` stage every tracked change, and an untracked
    /// file stays out of the index.
    #[test]
    fn git_add_stages_the_tracked_changes_and_leaves_untracked_files() {
        let added = |command: &str| {
            let mut h = harness(&[]);
            h.fake_git()
                .add_repo("/repo")
                .unstaged_file("c.rs", "edit")
                .untracked("d.rs");
            submit(&mut h, command);
            (staged_flags(&h), h.stoat.pending_message.clone())
        };
        let staged = (
            vec![
                (PathBuf::from("c.rs"), true),
                (PathBuf::from("d.rs"), false),
            ],
            Some("staged 1 file".to_string()),
        );
        assert_eq!(
            [added(":git add"), added(":git au")],
            [staged.clone(), staged]
        );
    }

    /// A tree with no tracked change badges and stages nothing, and an
    /// untracked file does not count as one.
    #[test]
    fn git_add_with_no_tracked_change_badges_nothing_to_stage() {
        let mut h = harness(&[]);
        h.fake_git().add_repo("/repo").untracked("d.rs");
        submit(&mut h, ":git add");
        assert_eq!(
            (staged_flags(&h), review_badge(&h)),
            (
                vec![(PathBuf::from("d.rs"), false)],
                Some("nothing to stage".to_string())
            ),
        );
    }

    /// A stage the repository refuses badges the failure and leaves the change
    /// unstaged.
    #[test]
    fn a_refused_stage_badges_the_failure() {
        let mut h = harness(&[]);
        h.fake_git()
            .add_repo("/repo")
            .unstaged_file("c.rs", "edit")
            .fail_apply_with("disk full");
        submit(&mut h, ":git add");
        assert_eq!(
            (staged_flags(&h), review_badge(&h)),
            (
                vec![(PathBuf::from("c.rs"), false)],
                Some("stage failed".to_string())
            ),
        );
    }

    /// A write alone leaves the commit waiting, as a write in git's editor
    /// does.
    #[test]
    fn a_write_alone_commits_nothing() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        type_message(&mut h, "feat: x");
        submit(&mut h, ":w");
        assert_eq!(
            (head(&h), message_buffer(&h).is_some(), waiting(&h)),
            (Some("c2".to_string()), true, true),
        );
    }

    /// `:wq` commits the index with the written message, closes the split, and
    /// names the commit in the status line.
    #[test]
    fn write_quit_commits_the_cleaned_message_and_closes_the_split() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        type_message(&mut h, "feat: x");
        submit(&mut h, ":wq");

        let sha = head(&h).expect("a HEAD");
        assert_eq!(
            (
                head_message(&h),
                pane_count(&h),
                h.stoat.pending_message.clone(),
                waiting(&h),
                message_buffer(&h),
            ),
            (
                Some("feat: x".to_string()),
                1,
                Some(format!("committed {} feat: x", &sha[..7])),
                false,
                None,
            ),
        );
    }

    /// Git's cleanup takes a typed comment line and the trailing blank lines out
    /// of the committed message.
    #[test]
    fn comment_lines_leave_the_committed_message() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        h.type_keys("i");
        h.type_text("feat: x");
        h.type_keys("enter");
        h.type_text("# note");
        h.type_keys("enter enter escape");
        submit(&mut h, ":wq");
        assert_eq!(head_message(&h), Some("feat: x".to_string()));
    }

    /// A quit with no write leaves the bare template in the file, which cleans
    /// to an empty message.
    #[test]
    fn a_quit_with_no_write_aborts_with_an_empty_message() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        submit(&mut h, ":q");
        assert_eq!(
            (head(&h), review_badge(&h)),
            (
                Some("c2".to_string()),
                Some("commit aborted: empty message".to_string())
            ),
        );
    }

    /// A quit over unsaved edits abandons the message, as git does when its
    /// editor exits without a write.
    #[test]
    fn a_quit_over_unsaved_edits_aborts_the_commit() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        type_message(&mut h, "feat: x");
        submit(&mut h, ":q");
        assert_eq!(
            (head(&h), review_badge(&h)),
            (
                Some("c2".to_string()),
                Some("commit aborted: message closed without a write".to_string())
            ),
        );
    }

    /// A file that does not read at the commit's turn commits nothing and badges
    /// the read.
    #[test]
    fn an_unreadable_message_commits_nothing() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        type_message(&mut h, "feat: x");
        submit(&mut h, ":w");
        h.fake_fs()
            .fail_next_read(MESSAGE, ErrorKind::PermissionDenied);
        submit(&mut h, ":q");
        assert_eq!(
            (head(&h), review_badge(&h)),
            (
                Some("c2".to_string()),
                Some("commit message not read".to_string())
            ),
        );
    }

    /// A commit the repository refuses opens the message again, so the reader
    /// edits it and retries or closes it.
    #[test]
    fn a_failed_commit_reopens_the_message() {
        let mut h = harness(&[("a.rs", "3\n")]);
        h.fake_git().add_repo("/repo").fail_apply_with("disk full");
        submit(&mut h, ":git commit");
        type_message(&mut h, "feat: x");
        submit(&mut h, ":wq");

        let first_line = message_buffer(&h).map(|id| {
            let text = h.buffer_text(id);
            text.lines().next().unwrap_or("").to_string()
        });
        assert_eq!(
            (head(&h), review_badge(&h), first_line, waiting(&h)),
            (
                Some("c2".to_string()),
                Some("commit failed".to_string()),
                Some("feat: x".to_string()),
                true,
            ),
        );
    }

    /// The reader stages hunks in the diff view and commits from there, so the
    /// message opens beside that pane and the close leaves it as it was.
    #[test]
    fn git_commit_from_a_diff_view_keeps_that_pane() {
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

        submit(&mut h, ":git commit");
        let beside = pane_count(&h);
        type_message(&mut h, "fix: y");
        submit(&mut h, ":wq");
        let focused = h.stoat.focused_editor_ids().map(|(editor, _)| editor);
        let diff_view = h
            .stoat
            .active_workspace()
            .editors
            .get(editor_id)
            .map(|editor| editor.diff_view);
        assert_eq!(
            (beside, pane_count(&h), focused, diff_view, head_message(&h)),
            (
                2,
                1,
                Some(editor_id),
                Some(true),
                Some("fix: y".to_string())
            ),
        );
    }

    /// A second `:git commit` while the message is open changes nothing.
    #[test]
    fn a_second_git_commit_while_the_message_is_open_changes_nothing() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git commit");
        submit(&mut h, ":git commit");
        assert_eq!((pane_count(&h), head(&h)), (2, Some("c2".to_string())));
    }

    /// `:git amend` and `:git cia` write HEAD's message above the template and
    /// name the amend in the status row, and an empty index opens it too.
    #[test]
    fn git_amend_opens_the_message_buffer_over_heads_message() {
        let opened = |staged: &[(&str, &str)], command: &str| {
            let mut h = harness(staged);
            h.resize(120, 24);
            submit(&mut h, command);
            h.snapshot();
            (
                message_file(&h),
                h.rendered_text().contains("COMMIT_EDITMSG [for git amend]"),
            )
        };
        let amending =
            |staged: &[StagedChange]| (Some(template("two", Some("main"), staged)), true);
        assert_eq!(
            [
                opened(&[("a.rs", "3\n")], ":git amend"),
                opened(&[("a.rs", "3\n")], ":git cia"),
                opened(&[], ":git amend"),
            ],
            [
                amending(&[modified("a.rs")]),
                amending(&[modified("a.rs")]),
                amending(&[]),
            ],
        );
    }

    /// `:wq` on the amend message replaces HEAD with a commit of the index and
    /// the edited message, and names the amend in the status line.
    #[test]
    fn write_quit_amends_head_with_the_index_and_the_message() {
        let mut h = harness(&[("a.rs", "3\n")]);
        submit(&mut h, ":git amend");
        type_message(&mut h, "fix: ");
        submit(&mut h, ":wq");

        let sha = head(&h).expect("a HEAD");
        let amends: Vec<(String, String)> = h
            .fake_git()
            .amend_history(Path::new("/repo"))
            .into_iter()
            .map(|amend| (amend.old_head, amend.new_head))
            .collect();
        assert_eq!(
            (head_message(&h), h.stoat.pending_message.clone(), amends),
            (
                Some("fix: two".to_string()),
                Some(format!("amended {} fix: two", &sha[..7])),
                vec![("c2".to_string(), sha.clone())],
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
        type_message(&mut h, "feat: x");
        submit(&mut h, ":wq");
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
        type_message(&mut h, "feat: x");
        submit(&mut h, ":wq");
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

    /// An index with no change badges and writes nothing, even over an
    /// unstaged edit.
    #[test]
    fn git_commit_over_a_clean_index_badges_and_opens_nothing() {
        let mut h = harness(&[]);
        h.fake_git().add_repo("/repo").unstaged_file("c.rs", "edit");
        submit(&mut h, ":git commit");
        assert_eq!(
            (pane_count(&h), message_file(&h), review_badge(&h)),
            (1, None, Some("nothing staged".to_string())),
        );
    }

    /// A review walk or a rebase owns HEAD until it ends, so either one refuses
    /// the command.
    #[test]
    fn git_commit_during_a_walk_or_a_rebase_opens_nothing() {
        let refused = |h: &mut TestHarness| {
            submit(h, ":git commit");
            (message_file(h), review_badge(h))
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

    /// A continue during a merge opens the message over the merge's message
    /// with nothing staged. The commit drops the message's comment lines,
    /// takes the merged tip as its second parent, and ends the merge.
    #[test]
    fn a_merge_continue_commits_the_merge_message_over_both_parents() {
        let mut h = harness(&[]);
        h.fake_git().add_repo("/repo").merge_in_progress(
            "f1",
            Some("feature"),
            "Merge branch 'feature'\n\n#Conflicts:\n#\ta.rs\n",
        );
        submit(&mut h, ":git merge continue");
        let opened =
            message_file(&h).is_some_and(|text| text.starts_with("Merge branch 'feature'\n"));
        submit(&mut h, ":wq");

        let sha = head(&h).expect("a HEAD");
        let repo = h
            .stoat
            .git_host
            .discover(Path::new("/repo"))
            .expect("the repo");
        assert_eq!(
            (
                opened,
                head_message(&h),
                repo.parent_shas(&sha),
                repo.merge_state(),
                h.stoat.pending_message.clone(),
            ),
            (
                true,
                Some("Merge branch 'feature'".to_string()),
                vec!["c2".to_string(), "f1".to_string()],
                None,
                Some(format!("merged {} Merge branch 'feature'", &sha[..7])),
            ),
        );
    }

    #[test]
    fn snapshot_commit_message_buffer() {
        let mut h = harness(&[("a.rs", "3\n"), ("b.rs", "new\n")]);
        h.resize(90, 12);
        submit(&mut h, ":git commit");
        type_message(&mut h, "feat: x");
        h.assert_snapshot("commit_message_buffer");
    }
}
