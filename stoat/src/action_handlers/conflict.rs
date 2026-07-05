use crate::{
    action_handlers::rebase::{drive_rebase, emit_rebase_error},
    app::{Stoat, UpdateEffect},
};

#[derive(Copy, Clone, Debug)]
pub(super) enum ConflictChoice {
    Ours,
    Theirs,
}

pub(super) fn conflict_step(stoat: &mut Stoat, down: bool) -> UpdateEffect {
    use crate::rebase::RebasePause;
    let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
        return UpdateEffect::None;
    };
    let Some(RebasePause::Conflict {
        files, selected, ..
    }) = active.pause.as_mut()
    else {
        return UpdateEffect::None;
    };
    if files.is_empty() {
        return UpdateEffect::None;
    }
    let before = *selected;
    if down {
        if *selected + 1 < files.len() {
            *selected += 1;
        }
    } else if *selected > 0 {
        *selected -= 1;
    }
    if *selected != before {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

pub(super) fn conflict_set(stoat: &mut Stoat, choice: ConflictChoice) -> UpdateEffect {
    use crate::rebase::{ConflictResolution, RebasePause};
    let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
        return UpdateEffect::None;
    };
    let Some(RebasePause::Conflict {
        files,
        selected,
        resolutions,
        ..
    }) = active.pause.as_mut()
    else {
        return UpdateEffect::None;
    };
    let Some(file) = files.get(*selected) else {
        return UpdateEffect::None;
    };
    let resolution = match choice {
        ConflictChoice::Ours => ConflictResolution::TakeOurs,
        ConflictChoice::Theirs => ConflictResolution::TakeTheirs,
    };
    resolutions.insert(file.path.clone(), resolution);
    UpdateEffect::Redraw
}

pub(super) fn conflict_skip_entry(stoat: &mut Stoat) -> UpdateEffect {
    use crate::rebase::RebasePause;
    let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
        return UpdateEffect::None;
    };
    if !matches!(active.pause, Some(RebasePause::Conflict { .. })) {
        return UpdateEffect::None;
    }
    active.pause = None;
    drive_rebase(stoat)
}

pub(super) fn conflict_abort(stoat: &mut Stoat) -> UpdateEffect {
    stoat.active_workspace_mut().rebase_active = None;
    emit_rebase_error(stoat, "rebase aborted during conflict", None);
    UpdateEffect::Redraw
}

pub(super) fn conflict_apply(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{
        host::GitApplyError,
        rebase::{ConflictResolution, RebasePause},
    };

    let (workdir, resolved_tree, author_name, author_email, message, parent) = {
        let Some(active) = stoat.active_workspace().rebase_active.as_ref() else {
            return UpdateEffect::None;
        };
        let Some(RebasePause::Conflict {
            source_sha,
            files,
            resolutions,
            ..
        }) = active.pause.as_ref()
        else {
            return UpdateEffect::None;
        };

        let Some(repo) = stoat.git_host.discover(&active.workdir) else {
            return UpdateEffect::None;
        };
        let Some(mut tree) = repo.commit_tree(&active.current_head) else {
            return UpdateEffect::None;
        };
        for file in files {
            let choice = resolutions
                .get(&file.path)
                .copied()
                .unwrap_or(ConflictResolution::TakeTheirs);
            match choice {
                ConflictResolution::TakeOurs => {
                    if let Some(content) = &file.ours {
                        tree.insert(file.path.clone(), content.clone());
                    } else {
                        tree.remove(&file.path);
                    }
                },
                ConflictResolution::TakeTheirs => {
                    if let Some(content) = &file.theirs {
                        tree.insert(file.path.clone(), content.clone());
                    } else {
                        tree.remove(&file.path);
                    }
                },
                ConflictResolution::SkipEntry => {},
            }
        }
        let message = format!("conflict-resolved {source_sha}");
        (
            active.workdir.clone(),
            tree,
            "stoat".to_string(),
            "stoat@example.invalid".to_string(),
            message,
            active.current_head.clone(),
        )
    };

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        emit_rebase_error(stoat, "git repo not found", None);
        return UpdateEffect::Redraw;
    };
    match repo.create_commit(
        Some(&parent),
        &resolved_tree,
        &message,
        &author_name,
        &author_email,
    ) {
        Ok(new_sha) => {
            let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
                return UpdateEffect::None;
            };
            active.current_head = new_sha.clone();
            active.last_pick_sha = Some(new_sha.clone());
            active.last_message = Some(message);
            active.pause = None;
            drive_rebase(stoat)
        },
        Err(GitApplyError::Backend { reason, .. }) => {
            emit_rebase_error(stoat, "conflict commit failed", Some(reason));
            UpdateEffect::Redraw
        },
    }
}
