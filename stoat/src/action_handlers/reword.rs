use crate::{
    action_handlers::rebase::{drive_rebase, emit_rebase_error},
    app::{Stoat, UpdateEffect},
    git_jobs::{self, GitJob, GitLanding, GitWork},
    host::{GitApplyError, GitRepo},
    input_view::{InputView, SubmitTarget},
    rebase::RebasePause,
};

pub(super) fn reword_abort(stoat: &mut Stoat) -> UpdateEffect {
    let input = {
        let ws = stoat.active_workspace();
        ws.rebase_active
            .as_ref()
            .and_then(|a| a.pause.as_ref())
            .and_then(|p| match p {
                RebasePause::Reword { input, .. } => Some(input.clone()),
                _ => None,
            })
    };
    if let Some(input) = input {
        input.dispose(stoat.active_workspace_mut());
    }
    stoat.active_workspace_mut().rebase_active = None;
    emit_rebase_error(
        stoat,
        "rebase aborted during reword",
        Some("HEAD left at partial rebase state".into()),
    );
    UpdateEffect::Redraw
}

/// Commit the reworded message over the picked commit and resume the plan.
///
/// The commit waits its turn in the git queue, and the reword screen stays up
/// until it lands. A failed commit keeps the screen and its message, so the
/// reader edits the message again or aborts.
pub(super) fn reword_confirm(stoat: &mut Stoat) -> UpdateEffect {
    let (workdir, picked_sha, new_message, fallback_parent, input) = {
        let Some(active) = stoat.active_workspace().rebase_active.as_ref() else {
            return UpdateEffect::None;
        };
        let Some(RebasePause::Reword {
            cherry_picked_commit,
            input,
            ..
        }) = active.pause.as_ref()
        else {
            return UpdateEffect::None;
        };
        let buffer_text = input.text(stoat.active_workspace());
        (
            active.workdir.clone(),
            cherry_picked_commit.clone(),
            buffer_text,
            Some(active.current_head.clone()),
            input.clone(),
        )
    };

    // Empty (whitespace-only) message auto-aborts, matching git's
    // behaviour when the commit message file is emptied by the editor.
    if new_message.trim().is_empty() {
        input.dispose(stoat.active_workspace_mut());
        stoat.active_workspace_mut().rebase_active = None;
        emit_rebase_error(
            stoat,
            "rebase aborted: empty commit message",
            Some("HEAD left at partial rebase state".into()),
        );
        return UpdateEffect::Redraw;
    }

    let message = new_message.trim().to_string();
    let job = GitJob::new(None, move |stoat: &mut Stoat| {
        if !reword_paused_on(stoat, &picked_sha) {
            return None;
        }
        let Some(repo) = stoat.git_host.discover(&workdir) else {
            emit_rebase_error(stoat, "git repo not found", None);
            return None;
        };
        Some(Box::new(move || {
            let committed = reword_commit(&*repo, &picked_sha, fallback_parent, &message);
            Box::new(move |stoat: &mut Stoat| land_reword(stoat, &picked_sha, message, committed))
                as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Write `message` as a commit over the tree of `picked_sha`, on whatever
/// thread calls.
///
/// The commit goes onto the parent of `picked_sha`, or onto `fallback_parent`
/// when that parent does not resolve. A failure answers the badge label and
/// what the backend said.
fn reword_commit(
    repo: &dyn GitRepo,
    picked_sha: &str,
    fallback_parent: Option<String>,
    message: &str,
) -> Result<String, (String, Option<String>)> {
    let Some(tree) = repo.tree_oid(picked_sha) else {
        return Err(("reword: commit tree unreadable".to_string(), None));
    };
    let parent = repo.parent_sha(picked_sha).or(fallback_parent);
    match repo.create_commit(
        parent.as_deref(),
        &tree,
        message,
        "stoat",
        "stoat@example.invalid",
    ) {
        Ok(new_sha) => Ok(new_sha),
        Err(GitApplyError::Backend { reason, .. }) => {
            Err(("reword failed".to_string(), Some(reason)))
        },
    }
}

/// Apply a landed reword commit and resume the plan, unless the reword stop
/// ended before the commit landed.
///
/// A failure badges and leaves the pause and its input in place.
fn land_reword(
    stoat: &mut Stoat,
    picked_sha: &str,
    message: String,
    committed: Result<String, (String, Option<String>)>,
) {
    if !reword_paused_on(stoat, picked_sha) {
        return;
    }
    let new_sha = match committed {
        Ok(new_sha) => new_sha,
        Err((label, detail)) => {
            emit_rebase_error(stoat, &label, detail);
            return;
        },
    };

    let ws = stoat.active_workspace_mut();
    let Some(active) = ws.rebase_active.as_mut() else {
        return;
    };
    active.current_head = new_sha.clone();
    active.last_pick_sha = Some(new_sha);
    active.last_message = Some(message);
    if let Some(RebasePause::Reword { input, .. }) = active.pause.take() {
        input.dispose(ws);
    }
    drive_rebase(stoat);
}

/// Whether the rebase stands at the Reword stop over `picked_sha`.
fn reword_paused_on(stoat: &Stoat, picked_sha: &str) -> bool {
    stoat
        .active_workspace()
        .rebase_active
        .as_ref()
        .is_some_and(|active| {
            matches!(
                &active.pause,
                Some(RebasePause::Reword { cherry_picked_commit, .. })
                    if cherry_picked_commit == picked_sha
            )
        })
}

/// Create an [`InputView`] seeded with `original_message`, place the cursor
/// at end, and install a [`crate::rebase::RebasePause::Reword`] pointing at
/// the new input. The input is born in normal mode, and the `reword` screen is
/// derived from the pause, so the Helix-scratch-buffer workflow (normal mode
/// default, `i`/`a` to edit, `Ctrl-s` to submit) applies without a mode
/// assignment.
pub(super) fn install_reword_pause(
    stoat: &mut Stoat,
    cherry_picked_commit: String,
    original_message: String,
) {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();

    let input = InputView::create(
        ws,
        executor,
        SubmitTarget::Reword,
        &original_message,
        "normal",
        u16::MAX,
    );

    let Some(active) = ws.rebase_active.as_mut() else {
        input.dispose(ws);
        return;
    };
    active.pause = Some(RebasePause::Reword {
        cherry_picked_commit,
        original_message,
        input,
    });
}
