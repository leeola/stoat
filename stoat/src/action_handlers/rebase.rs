use crate::{
    action_handlers::{commits::commits_refresh, reword::install_reword_pause},
    app::{Stoat, UpdateEffect},
    git_jobs::{self, GitJob, GitJobKey, GitLanding, GitWork},
    host::{CherryPickOutcome, ConflictedFile, GitApplyError, GitRepo, RebaseTodoOp},
    merge_view::MergeRow,
    rebase::{ActiveRebase, RebaseEntry, RebasePause, RebaseState},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Copy, Clone, Debug)]
pub(crate) enum RebaseMove {
    Next,
    Prev,
    SwapUp,
    SwapDown,
}

/// What the git work of one rebase entry produced, carried to its landing.
enum StepOutcome {
    /// The commit the entry made, with the message a later squash builds on.
    Committed { new_sha: String, message: String },
    /// The pick conflicted. `merge_rows` holds the first file aligned.
    Conflict {
        files: Vec<ConflictedFile>,
        merge_rows: Vec<Option<Vec<MergeRow>>>,
    },
    /// A git call failed, with the badge label and what the backend said.
    Failed { label: &'static str, reason: String },
}

pub(super) fn enter_rebase(stoat: &mut Stoat) -> UpdateEffect {
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
    UpdateEffect::Redraw
}

pub(super) fn abort_rebase(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    if ws.rebase.take().is_none() {
        return UpdateEffect::None;
    }
    UpdateEffect::Redraw
}

/// Select the todo entry at `index`, or do nothing when it names no entry.
///
/// A press selects and nothing more, the way the commits list does. Running
/// the plan stays on its own key, so a misclick edits no rebase.
pub(crate) fn rebase_select(stoat: &mut Stoat, index: usize) -> UpdateEffect {
    let Some(state) = stoat.active_workspace_mut().rebase.as_mut() else {
        return UpdateEffect::None;
    };
    if index >= state.todo.len() || index == state.selected {
        return UpdateEffect::None;
    }
    state.selected = index;
    UpdateEffect::Redraw
}

pub(crate) fn rebase_move(stoat: &mut Stoat, step: RebaseMove) -> UpdateEffect {
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

pub(super) fn rebase_set_op(stoat: &mut Stoat, op: RebaseTodoOp) -> UpdateEffect {
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
    use crate::rebase::RebasePause;

    // Read from HEAD rather than from what the stepper last recorded. The pause
    // checked the commit out, so HEAD is where an amend made while stopped would
    // have left it, and resuming from the recorded sha would drop that amend.
    let head = {
        let workdir = stoat.active_workspace().git_root.clone();
        stoat
            .git_host
            .discover(&workdir)
            .and_then(|repo| repo.resolve_rev("HEAD"))
    };

    let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
        return UpdateEffect::None;
    };
    if !matches!(active.pause, Some(RebasePause::Edit { .. })) {
        return UpdateEffect::None;
    }
    if let Some(head) = head {
        active.current_head = head.clone();
        active.last_pick_sha = Some(head);
    }
    active.pause = None;

    stoat.active_workspace_mut().set_diff_base(None);
    super::review::exit_diff_view(stoat);
    drive_rebase(stoat)
}

/// Check the just-picked commit out and point `:diff` at what it changed.
///
/// The stepper builds commits without moving HEAD or touching the working tree,
/// so an Edit stop has to do both before the diff means anything: `:diff`
/// compares live buffers against a base, and the buffers have to be the commit
/// for that comparison to be the commit. This is also what an interactive
/// rebase does when it stops to let you edit, so the tree the user lands in is
/// the one they would expect.
///
/// The checkout runs off the loop, the same way a review-walk step does, so the
/// badge and the diff appear when it lands.
///
/// A failed checkout badges and leaves the pause standing. The stepper is
/// stopped either way, and reporting it is better than showing a diff against a
/// tree that is not there.
fn install_edit_pause(stoat: &mut Stoat, workdir: &Path, sha: &str) {
    let short = &sha[..sha.len().min(7)];
    super::review_walk::queue_walk_landing(
        stoat,
        super::review_walk::WalkLandingKind::EditPause,
        workdir.to_path_buf(),
        sha.to_string(),
        format!("editing {short}, C continues"),
    );
}

/// Move the rebase plan to its next git step, its next pause, or its end.
///
/// Each pick runs as a queued git job. The landing of that job drives the next
/// entry, so a call while a step is out does nothing. The keys that end a
/// Reword, an Edit, or a conflict pause call this to resume the plan.
///
/// When the plan drains, the rebase ends at once. The move of HEAD onto the
/// rebased tip waits its turn in the git queue, and the complete badge shows
/// when that move lands.
pub(super) fn drive_rebase(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.git_jobs.holds(GitJobKey::RebaseStep) {
        return UpdateEffect::Redraw;
    }

    loop {
        let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
            return UpdateEffect::None;
        };
        if active.pause.is_some() {
            return UpdateEffect::Redraw;
        }

        let Some(entry) = active.remaining.pop_front() else {
            let final_head = active.current_head.clone();
            stoat.active_workspace_mut().rebase_active = None;
            queue_rebase_finish(stoat, final_head);
            return UpdateEffect::Redraw;
        };
        if entry.op == RebaseTodoOp::Drop {
            continue;
        }

        queue_rebase_step(stoat, entry);
        return UpdateEffect::Redraw;
    }
}

/// Queue the pick and commit of `entry`, whose landing drives the next entry.
///
/// The start reads the base and the squash message at the job's turn, after
/// every earlier job landed, so the entry stacks onto what those jobs built. A
/// start after an abort finds no rebase and refuses.
fn queue_rebase_step(stoat: &mut Stoat, entry: RebaseEntry) {
    let job = GitJob::new(Some(GitJobKey::RebaseStep), move |stoat: &mut Stoat| {
        let active = stoat.active_workspace().rebase_active.as_ref()?;
        let workdir = active.workdir.clone();
        let last_message = active.last_message.clone().unwrap_or_default();
        let base = match entry.op {
            RebaseTodoOp::Squash | RebaseTodoOp::Fixup => active.last_pick_sha.clone(),
            _ => Some(active.current_head.clone()),
        };

        let Some(base) = base else {
            emit_rebase_error(stoat, "squash/fixup without preceding pick", None);
            return None;
        };
        let Some(repo) = stoat.git_host.discover(&workdir) else {
            emit_rebase_error(stoat, "git repo not found", None);
            return None;
        };

        Some(Box::new(move || {
            let outcome = run_rebase_step(&*repo, &entry, &base, &last_message);
            Box::new(move |stoat: &mut Stoat| land_rebase_step(stoat, entry, workdir, outcome))
                as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
}

/// Pick `entry` onto `base` and commit the result, on whatever thread calls.
///
/// A Squash or Fixup folds its pick into the commit at `base`, so its commit
/// replaces that one on the parent of `base`. A Squash joins both messages,
/// and a Fixup keeps `last_message` alone.
///
/// A conflict aligns its first file here, off the loop, because the pause
/// paints that file first.
fn run_rebase_step(
    repo: &dyn GitRepo,
    entry: &RebaseEntry,
    base: &str,
    last_message: &str,
) -> StepOutcome {
    let folds = matches!(entry.op, RebaseTodoOp::Squash | RebaseTodoOp::Fixup);
    let (tree, source_message, author_name, author_email) =
        match repo.cherry_pick_tree(&entry.commit.sha, base) {
            Ok(CherryPickOutcome::Clean {
                tree,
                message,
                author_name,
                author_email,
                ..
            }) => (tree, message, author_name, author_email),
            Ok(CherryPickOutcome::Conflict { files }) => {
                return StepOutcome::Conflict {
                    merge_rows: super::conflict::aligned_slots(&files, 0),
                    files,
                };
            },
            Err(GitApplyError::Backend { reason, .. }) => {
                let label = match folds {
                    true => "squash cherry-pick failed",
                    false => "cherry-pick failed",
                };
                return StepOutcome::Failed { label, reason };
            },
        };

    let (parent, message) = match entry.op {
        RebaseTodoOp::Squash => (
            repo.parent_sha(base),
            format!(
                "{}\n\n{}",
                last_message.trim_end(),
                source_message.trim_end()
            ),
        ),
        RebaseTodoOp::Fixup => (repo.parent_sha(base), last_message.to_string()),
        _ => (Some(base.to_string()), source_message),
    };
    match repo.create_commit(
        parent.as_deref(),
        &tree,
        &message,
        &author_name,
        &author_email,
    ) {
        Ok(new_sha) => StepOutcome::Committed { new_sha, message },
        Err(GitApplyError::Backend { reason, .. }) => {
            let label = match folds {
                true => "squash commit failed",
                false => "create_commit failed",
            };
            StepOutcome::Failed { label, reason }
        },
    }
}

/// Apply the landed git work of `entry`, then pause or drive the next entry.
///
/// A landing after an abort applies nothing, since its plan is gone.
fn land_rebase_step(stoat: &mut Stoat, entry: RebaseEntry, workdir: PathBuf, outcome: StepOutcome) {
    let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
        return;
    };

    match outcome {
        StepOutcome::Committed { new_sha, message } => {
            active.current_head = new_sha.clone();
            active.last_pick_sha = Some(new_sha.clone());
            active.last_message = Some(message.clone());
            match entry.op {
                RebaseTodoOp::Reword => install_reword_pause(stoat, new_sha, message),
                RebaseTodoOp::Edit => {
                    active.pause = Some(RebasePause::Edit {
                        cherry_picked_commit: new_sha.clone(),
                    });
                    install_edit_pause(stoat, &workdir, &new_sha);
                },
                _ => {
                    drive_rebase(stoat);
                },
            }
        },
        StepOutcome::Conflict { files, merge_rows } => {
            active.pause = Some(RebasePause::Conflict {
                source_sha: entry.commit.sha,
                files,
                selected: 0,
                resolutions: HashMap::new(),
                merge_rows,
            });
        },
        StepOutcome::Failed { label, reason } => emit_rebase_error(stoat, label, Some(reason)),
    }
}

/// Queue the move of HEAD onto `final_head`, the tip the drained plan built.
///
/// The job has no key. A keyed job that waits in the queue gives its place to
/// a later plan's keyed step, and HEAD then never moves.
fn queue_rebase_finish(stoat: &mut Stoat, final_head: String) {
    let job = GitJob::new(None, move |stoat: &mut Stoat| {
        let repo = stoat.git_host.discover(&stoat.active_workspace().git_root);
        Some(Box::new(move || {
            if let Some(repo) = repo {
                let _ = repo.update_head(&final_head);
            }
            Box::new(move |stoat: &mut Stoat| {
                let short = &final_head[..final_head.len().min(7)];
                emit_rebase_complete(stoat, &format!("rebase complete, HEAD at {short}"));
                commits_refresh(stoat);
            }) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
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

/// Run the rebase plan on screen.
///
/// The press takes the plan and queues a check for tracked changes. The plan
/// starts when that check lands clean. While another rebase runs, the press
/// reports that and leaves the plan on screen.
pub(super) fn execute_rebase(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().rebase_active.is_some() {
        emit_rebase_error(stoat, "rebase already in progress", None);
        return UpdateEffect::Redraw;
    }
    let Some(plan) = stoat.active_workspace_mut().rebase.take() else {
        return UpdateEffect::None;
    };

    // A keyed check that waits in the queue gives its place to the next plan's
    // keyed check, which drops this plan with no report. So the check has no
    // key.
    let job = GitJob::new(None, move |stoat: &mut Stoat| {
        let Some(repo) = stoat.git_host.discover(&plan.workdir) else {
            emit_rebase_error(stoat, "git repo not found", None);
            return None;
        };
        Some(Box::new(move || {
            let dirty = repo.has_tracked_changes();
            Box::new(move |stoat: &mut Stoat| land_rebase_check(stoat, plan, dirty)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Start `plan` when its dirty check lands clean.
///
/// A check that waited behind another plan's check finds that plan installed,
/// with its first step queued, and refuses.
fn land_rebase_check(stoat: &mut Stoat, plan: RebaseState, dirty: bool) {
    if dirty {
        emit_rebase_error(stoat, "working tree dirty: commit or stash first", None);
        return;
    }
    if stoat.active_workspace().rebase_active.is_some() {
        emit_rebase_error(stoat, "rebase already in progress", None);
        return;
    }

    stoat.active_workspace_mut().rebase_active = Some(ActiveRebase::new(plan));
    drive_rebase(stoat);
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
