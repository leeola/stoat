use crate::{
    action_handlers::rebase::{drive_rebase, emit_rebase_error},
    app::{Stoat, UpdateEffect},
};
use std::path::PathBuf;

#[derive(Copy, Clone, Debug)]
pub(super) enum ConflictChoice {
    Ours,
    Theirs,
}

/// One empty row slot per file, with `selected`'s filled.
///
/// Called where a pause is installed, so the file the screen opens on is
/// aligned before its first paint rather than during it.
pub(crate) fn aligned_slots(
    files: &[crate::host::ConflictedFile],
    selected: usize,
) -> Vec<Option<Vec<crate::merge_view::MergeRow>>> {
    let mut slots = vec![None; files.len()];
    fill_slot(files, &mut slots, selected);
    slots
}

/// Align `index`'s file into its slot, unless it is aligned already.
///
/// Both halves of the paint read the slot, so this runs where the selection
/// moves rather than where it is drawn.
fn fill_slot(
    files: &[crate::host::ConflictedFile],
    slots: &mut [Option<Vec<crate::merge_view::MergeRow>>],
    index: usize,
) {
    let (Some(file), Some(slot)) = (files.get(index), slots.get_mut(index)) else {
        return;
    };
    if slot.is_some() {
        return;
    }
    *slot = Some(crate::merge_view::build_merge_rows(
        file.ancestor.as_deref().unwrap_or(""),
        file.ours.as_deref().unwrap_or(""),
        file.theirs.as_deref().unwrap_or(""),
        None,
    ));
}
/// Select the conflicted file at `index`, or do nothing when it names none.
///
/// A press selects and nothing more. Taking a side stays on its own key, so a
/// misclick picks a different file rather than resolving one.
pub(crate) fn conflict_select(stoat: &mut Stoat, index: usize) -> UpdateEffect {
    use crate::rebase::RebasePause;
    let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
        return UpdateEffect::None;
    };
    let Some(RebasePause::Conflict {
        files,
        selected,
        merge_rows,
        ..
    }) = active.pause.as_mut()
    else {
        return UpdateEffect::None;
    };
    if index >= files.len() || index == *selected {
        return UpdateEffect::None;
    }
    *selected = index;
    fill_slot(files, merge_rows, index);
    UpdateEffect::Redraw
}

pub(crate) fn conflict_step(stoat: &mut Stoat, down: bool) -> UpdateEffect {
    use crate::rebase::RebasePause;
    let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
        return UpdateEffect::None;
    };
    let Some(RebasePause::Conflict {
        files,
        selected,
        merge_rows,
        ..
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
    if *selected == before {
        return UpdateEffect::None;
    }
    fill_slot(files, merge_rows, *selected);
    UpdateEffect::Redraw
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

    let (workdir, updates, author_name, author_email, message, parent) = {
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

        // One entry per conflicted file, against the head the rebase has
        // reached. Every path the conflict did not name keeps the entry it
        // already had, so a binary the merge left alone rides through.
        let updates: Vec<(PathBuf, Option<String>)> = files
            .iter()
            .filter_map(|file| {
                let choice = resolutions
                    .get(&file.path)
                    .copied()
                    .unwrap_or(ConflictResolution::TakeTheirs);
                let side = match choice {
                    ConflictResolution::TakeOurs => &file.ours,
                    ConflictResolution::TakeTheirs => &file.theirs,
                    ConflictResolution::SkipEntry => return None,
                };
                Some((file.path.clone(), side.clone()))
            })
            .collect();
        let message = format!("conflict-resolved {source_sha}");
        (
            active.workdir.clone(),
            updates,
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
    let tree = match repo.tree_with_updates(&parent, &updates) {
        Ok(tree) => tree,
        Err(err) => {
            emit_rebase_error(stoat, &format!("conflict apply failed: {err}"), None);
            return UpdateEffect::Redraw;
        },
    };
    match repo.create_commit(Some(&parent), &tree, &message, &author_name, &author_email) {
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
