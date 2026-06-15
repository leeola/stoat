use crate::{
    action_handlers::{
        commits::commits_refresh,
        review::{close_review, install_review_session, scan_commit},
        reword::install_reword_pause,
    },
    app::{Stoat, UpdateEffect},
};

#[derive(Copy, Clone, Debug)]
pub(super) enum RebaseMove {
    Next,
    Prev,
    SwapUp,
    SwapDown,
}

pub(super) fn enter_rebase(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{
        host::RebaseTodoOp,
        rebase::{RebaseEntry, RebaseState},
    };

    let Some(state) = stoat.active_workspace().commits.as_ref() else {
        return UpdateEffect::None;
    };
    if state.commits.is_empty() {
        return UpdateEffect::None;
    }

    // Cursor position selects the rebase boundary:
    //   onto = commits[selected]
    //   todo = commits[0..selected] (newest-first) reversed to oldest-first
    // So pressing `i` on the 4th entry (HEAD~3) rebases the top 3 commits
    // onto it. Cursor at HEAD leaves nothing to rebase.
    let selected = state.selected;
    if selected == 0 || selected >= state.commits.len() {
        emit_rebase_error(
            stoat,
            "nothing to rebase",
            Some("select an older commit first; commits above it become the rebase plan".into()),
        );
        return UpdateEffect::Redraw;
    }

    let workdir = state.workdir.clone();
    let onto = state.commits[selected].sha.clone();
    let entries: Vec<RebaseEntry> = state.commits[..selected]
        .iter()
        .rev()
        .cloned()
        .map(|commit| RebaseEntry {
            op: RebaseTodoOp::Pick,
            commit,
        })
        .collect();

    stoat.active_workspace_mut().rebase = Some(RebaseState::new(workdir, onto, entries));
    stoat.mode = "rebase".to_string();
    UpdateEffect::Redraw
}

pub(super) fn abort_rebase(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    if ws.rebase.take().is_none() {
        return UpdateEffect::None;
    }
    stoat.mode = if stoat.active_workspace().commits.is_some() {
        "commits".into()
    } else {
        "normal".into()
    };
    UpdateEffect::Redraw
}

pub(super) fn rebase_move(stoat: &mut Stoat, step: RebaseMove) -> UpdateEffect {
    let Some(state) = stoat.active_workspace_mut().rebase.as_mut() else {
        return UpdateEffect::None;
    };
    let moved = match step {
        RebaseMove::Next => state.move_down(),
        RebaseMove::Prev => state.move_up(),
        RebaseMove::SwapUp => state.swap_up(),
        RebaseMove::SwapDown => state.swap_down(),
    };
    if moved {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

pub(super) fn rebase_set_op(stoat: &mut Stoat, op: crate::host::RebaseTodoOp) -> UpdateEffect {
    let Some(state) = stoat.active_workspace_mut().rebase.as_mut() else {
        return UpdateEffect::None;
    };
    if state.set_op(op) {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

pub(super) fn rebase_continue(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{
        rebase::RebasePause,
        review_session::{ReviewOrigin, ReviewSource},
    };

    let maybe_new_head = {
        let ws = stoat.active_workspace();
        ws.review
            .as_ref()
            .filter(|s| s.origin == ReviewOrigin::FromRebaseEdit)
            .and_then(|s| match &s.source {
                ReviewSource::Commit { sha, .. } => Some(sha.clone()),
                _ => None,
            })
    };

    let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
        return UpdateEffect::None;
    };
    if !matches!(active.pause, Some(RebasePause::Edit { .. })) {
        return UpdateEffect::None;
    }
    if let Some(new_head) = maybe_new_head {
        active.current_head = new_head.clone();
        active.last_pick_sha = Some(new_head);
    }
    active.pause = None;

    if stoat.active_workspace().review.is_some() {
        close_review(stoat);
    }
    drive_rebase(stoat)
}

/// Core rebase stepper. Pops entries from `remaining`, applying each via
/// `cherry_pick_tree` + `create_commit`. On Reword, Edit, or a merge
/// conflict it installs a `RebasePause` and returns so the UI can collect
/// user input before re-entry resumes via `drive_rebase`. When the queue
/// drains, HEAD is updated and `rebase_active` cleared.
pub(super) fn drive_rebase(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{
        host::{CherryPickOutcome, GitApplyError, RebaseTodoOp},
        rebase::RebasePause,
        review_session::ReviewOrigin,
    };

    loop {
        let entry = {
            let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
                return UpdateEffect::None;
            };
            if active.pause.is_some() {
                return UpdateEffect::Redraw;
            }
            match active.remaining.pop_front() {
                Some(e) => e,
                None => {
                    let final_head = active.current_head.clone();
                    stoat.active_workspace_mut().rebase_active = None;
                    let workdir = stoat.active_workspace().git_root.clone();
                    if let Some(repo) = stoat.git_host.discover(&workdir) {
                        let _ = repo.update_head(&final_head);
                    }
                    emit_rebase_complete(
                        stoat,
                        &format!(
                            "rebase complete, HEAD at {}",
                            &final_head[..final_head.len().min(7)]
                        ),
                    );
                    stoat.mode = if stoat.active_workspace().commits.is_some() {
                        "commits".into()
                    } else {
                        "normal".into()
                    };
                    commits_refresh(stoat);
                    return UpdateEffect::Redraw;
                },
            }
        };

        match entry.op {
            RebaseTodoOp::Drop => continue,
            RebaseTodoOp::Pick | RebaseTodoOp::Reword | RebaseTodoOp::Edit => {
                let (workdir, current_head) = {
                    let active = stoat
                        .active_workspace()
                        .rebase_active
                        .as_ref()
                        .expect("rebase_active present");
                    (active.workdir.clone(), active.current_head.clone())
                };
                let Some(repo) = stoat.git_host.discover(&workdir) else {
                    emit_rebase_error(stoat, "git repo not found", None);
                    return UpdateEffect::Redraw;
                };
                match repo.cherry_pick_tree(&entry.commit.sha, &current_head) {
                    Ok(CherryPickOutcome::Clean {
                        tree,
                        message,
                        author_name,
                        author_email,
                        ..
                    }) => match repo.create_commit(
                        Some(&current_head),
                        &tree,
                        &message,
                        &author_name,
                        &author_email,
                    ) {
                        Ok(new_sha) => {
                            let active = stoat
                                .active_workspace_mut()
                                .rebase_active
                                .as_mut()
                                .expect("rebase_active present");
                            active.current_head = new_sha.clone();
                            active.last_pick_sha = Some(new_sha.clone());
                            active.last_message = Some(message.clone());
                            match entry.op {
                                RebaseTodoOp::Pick => continue,
                                RebaseTodoOp::Reword => {
                                    install_reword_pause(stoat, new_sha, message.clone());
                                    stoat.mode = "reword".into();
                                    return UpdateEffect::Redraw;
                                },
                                RebaseTodoOp::Edit => {
                                    active.pause = Some(RebasePause::Edit {
                                        cherry_picked_commit: new_sha.clone(),
                                    });
                                    match scan_commit(stoat, &workdir, &new_sha) {
                                        Some(mut session) => {
                                            session.origin = ReviewOrigin::FromRebaseEdit;
                                            install_review_session(stoat, session);
                                        },
                                        _ => {
                                            stoat.mode = "review".into();
                                        },
                                    }
                                    return UpdateEffect::Redraw;
                                },
                                _ => unreachable!(),
                            }
                        },
                        Err(GitApplyError::Backend { reason, .. }) => {
                            emit_rebase_error(stoat, "create_commit failed", Some(reason));
                            return UpdateEffect::Redraw;
                        },
                    },
                    Ok(CherryPickOutcome::Conflict { files }) => {
                        let active = stoat
                            .active_workspace_mut()
                            .rebase_active
                            .as_mut()
                            .expect("rebase_active present");
                        active.pause = Some(RebasePause::Conflict {
                            source_sha: entry.commit.sha.clone(),
                            files,
                            selected: 0,
                            resolutions: std::collections::HashMap::new(),
                        });
                        stoat.mode = "conflict".into();
                        return UpdateEffect::Redraw;
                    },
                    Err(GitApplyError::Backend { reason, .. }) => {
                        emit_rebase_error(stoat, "cherry-pick failed", Some(reason));
                        return UpdateEffect::Redraw;
                    },
                }
            },
            RebaseTodoOp::Squash | RebaseTodoOp::Fixup => {
                let (workdir, last_pick, last_message) = {
                    let active = stoat
                        .active_workspace()
                        .rebase_active
                        .as_ref()
                        .expect("rebase_active present");
                    (
                        active.workdir.clone(),
                        match active.last_pick_sha.clone() {
                            Some(s) => s,
                            None => {
                                emit_rebase_error(
                                    stoat,
                                    "squash/fixup without preceding pick",
                                    None,
                                );
                                return UpdateEffect::Redraw;
                            },
                        },
                        active.last_message.clone().unwrap_or_default(),
                    )
                };
                let Some(repo) = stoat.git_host.discover(&workdir) else {
                    emit_rebase_error(stoat, "git repo not found", None);
                    return UpdateEffect::Redraw;
                };
                match repo.cherry_pick_tree(&entry.commit.sha, &last_pick) {
                    Ok(CherryPickOutcome::Clean {
                        tree,
                        message: source_msg,
                        author_name,
                        author_email,
                        ..
                    }) => {
                        let prev_parent = repo.parent_sha(&last_pick);
                        let combined = match entry.op {
                            RebaseTodoOp::Squash => {
                                format!("{}\n\n{}", last_message.trim_end(), source_msg.trim_end())
                            },
                            _ => last_message.clone(),
                        };
                        match repo.create_commit(
                            prev_parent.as_deref(),
                            &tree,
                            &combined,
                            &author_name,
                            &author_email,
                        ) {
                            Ok(new_sha) => {
                                let active = stoat
                                    .active_workspace_mut()
                                    .rebase_active
                                    .as_mut()
                                    .expect("rebase_active present");
                                active.current_head = new_sha.clone();
                                active.last_pick_sha = Some(new_sha);
                                active.last_message = Some(combined);
                            },
                            Err(GitApplyError::Backend { reason, .. }) => {
                                emit_rebase_error(stoat, "squash commit failed", Some(reason));
                                return UpdateEffect::Redraw;
                            },
                        }
                    },
                    Ok(CherryPickOutcome::Conflict { files }) => {
                        let active = stoat
                            .active_workspace_mut()
                            .rebase_active
                            .as_mut()
                            .expect("rebase_active present");
                        active.pause = Some(RebasePause::Conflict {
                            source_sha: entry.commit.sha.clone(),
                            files,
                            selected: 0,
                            resolutions: std::collections::HashMap::new(),
                        });
                        stoat.mode = "conflict".into();
                        return UpdateEffect::Redraw;
                    },
                    Err(GitApplyError::Backend { reason, .. }) => {
                        emit_rebase_error(stoat, "squash cherry-pick failed", Some(reason));
                        return UpdateEffect::Redraw;
                    },
                }
            },
        }
    }
}

fn emit_rebase_complete(stoat: &mut Stoat, label: &str) {
    use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};
    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::Review);
    ws.badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: Anchor::BottomRight,
        state: BadgeState::Complete,
        label: label.to_string(),
        detail: None,
    });
}

pub(super) fn execute_rebase(stoat: &mut Stoat) -> UpdateEffect {
    use crate::rebase::ActiveRebase;

    let Some(plan) = stoat.active_workspace_mut().rebase.take() else {
        return UpdateEffect::None;
    };
    let workdir = plan.workdir.clone();

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        emit_rebase_error(stoat, "git repo not found", None);
        return UpdateEffect::Redraw;
    };
    if !repo.changed_files().is_empty() {
        emit_rebase_error(stoat, "working tree dirty: commit or stash first", None);
        return UpdateEffect::Redraw;
    }

    stoat.active_workspace_mut().rebase_active = Some(ActiveRebase::new(plan));
    drive_rebase(stoat)
}

pub(super) fn emit_rebase_error(stoat: &mut Stoat, label: &str, detail: Option<String>) {
    use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};
    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::Review);
    ws.badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: Anchor::BottomRight,
        state: BadgeState::Error,
        label: label.to_string(),
        detail,
    });
}
