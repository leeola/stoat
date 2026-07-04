use crate::{
    action_handlers::rebase::{drive_rebase, emit_rebase_error},
    app::{Stoat, UpdateEffect},
    input_view::{InputView, SubmitTarget},
};

pub(super) fn reword_abort(stoat: &mut Stoat) -> UpdateEffect {
    use crate::rebase::RebasePause;
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
    stoat.set_focused_mode(if stoat.active_workspace().commits.is_some() {
        "commits".into()
    } else {
        "normal".into()
    });
    UpdateEffect::Redraw
}

pub(super) fn reword_confirm(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{host::GitApplyError, rebase::RebasePause};

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
        stoat.set_focused_mode(if stoat.active_workspace().commits.is_some() {
            "commits".into()
        } else {
            "normal".into()
        });
        return UpdateEffect::Redraw;
    }

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        emit_rebase_error(stoat, "git repo not found", None);
        return UpdateEffect::Redraw;
    };
    let Some(tree) = repo.commit_tree(&picked_sha) else {
        emit_rebase_error(stoat, "reword: commit tree unreadable", None);
        return UpdateEffect::Redraw;
    };
    let real_parent = repo.parent_sha(&picked_sha).or(fallback_parent);
    let trimmed_message = new_message.trim().to_string();
    match repo.create_commit(
        real_parent.as_deref(),
        &tree,
        &trimmed_message,
        "stoat",
        "stoat@example.invalid",
    ) {
        Ok(new_sha) => {
            input.dispose(stoat.active_workspace_mut());
            let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
                return UpdateEffect::None;
            };
            active.current_head = new_sha.clone();
            active.last_pick_sha = Some(new_sha.clone());
            active.last_message = Some(trimmed_message);
            active.pause = None;
            drive_rebase(stoat)
        },
        Err(GitApplyError::Backend { reason, .. }) => {
            emit_rebase_error(stoat, "reword failed", Some(reason));
            UpdateEffect::Redraw
        },
    }
}

/// Create an [`InputView`] seeded with `original_message`, place the cursor
/// at end, and install a [`crate::rebase::RebasePause::Reword`] pointing at
/// the new input. Caller is responsible for transitioning the focused mode to
/// `"reword"` after this returns so the Helix-scratch-buffer workflow
/// (normal mode default, `Ctrl-s` to submit) applies.
pub(super) fn install_reword_pause(
    stoat: &mut Stoat,
    cherry_picked_commit: String,
    original_message: String,
) {
    use crate::rebase::RebasePause;

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();

    let input = InputView::create(
        ws,
        executor,
        SubmitTarget::Reword,
        &original_message,
        "reword",
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
