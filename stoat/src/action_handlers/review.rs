use crate::{
    app::{Stoat, UpdateEffect},
    display_map::{BlockPlacement, BlockProperties, BlockStyle, RenderBlock},
    editor_state::{EditorId, EditorState},
    host::{self, WatchToken},
    pane::View,
    review::ReviewFileInput,
    review_session::{ReviewProgress, ReviewSession, ReviewSource, ReviewViewState},
    workspace::Workspace,
};
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use std::{path::Path, sync::Arc};

pub(super) fn open_review_commit(stoat: &mut Stoat, workdir: &Path, sha: &str) {
    let Some(session) = scan_commit(stoat, workdir, sha) else {
        return;
    };
    install_review_session(stoat, session);
}

pub(super) fn review_remove_selected(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{
        host::GitApplyError, review_apply::remove_chunks_from_buffer, review_session::ChunkStatus,
    };

    let (workdir, sha, staged_groups, full_trees_by_file) = {
        let Some(session) = stoat.active_workspace().review.as_ref() else {
            return UpdateEffect::None;
        };
        let (workdir, sha) = match &session.source {
            ReviewSource::Commit { workdir, sha } => (workdir.clone(), sha.clone()),
            _ => {
                tracing::warn!("ReviewRemoveSelected: only commit-source reviews support removal");
                emit_review_error_badge(stoat, "remove only valid for commit reviews", None);
                return UpdateEffect::Redraw;
            },
        };
        let mut groups: std::collections::HashMap<usize, Vec<&crate::review_session::ReviewChunk>> =
            std::collections::HashMap::new();
        for id in &session.order {
            if let Some(chunk) = session.chunks.get(id) {
                if chunk.status == ChunkStatus::Staged {
                    groups.entry(chunk.file_index).or_default().push(chunk);
                }
            }
        }
        let tree_snapshot: Vec<(usize, String, Arc<String>, Arc<String>)> = session
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                (
                    i,
                    f.rel_path.clone(),
                    f.base_text.clone(),
                    f.buffer_text.clone(),
                )
            })
            .collect();
        let groups_owned: Vec<(usize, Vec<crate::review_session::ReviewChunk>)> = groups
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().cloned().collect()))
            .collect();
        (workdir, sha, groups_owned, tree_snapshot)
    };

    if staged_groups.is_empty() {
        emit_review_info_badge(stoat, "nothing staged for removal");
        return UpdateEffect::Redraw;
    }

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        emit_review_error_badge(stoat, "git repo not found", None);
        return UpdateEffect::Redraw;
    };
    if !repo.changed_files().is_empty() {
        emit_review_error_badge(stoat, "working tree dirty: commit or stash first", None);
        return UpdateEffect::Redraw;
    }

    let Some(mut new_tree) = repo.commit_tree(&sha) else {
        emit_review_error_badge(stoat, "commit tree unreadable", None);
        return UpdateEffect::Redraw;
    };

    for (file_index, chunks) in &staged_groups {
        let Some((_, rel_path, base_arc, buffer_arc)) = full_trees_by_file
            .iter()
            .find(|(i, _, _, _)| i == file_index)
        else {
            continue;
        };
        let chunk_refs: Vec<&crate::review_session::ReviewChunk> = chunks.iter().collect();
        let new_buffer = remove_chunks_from_buffer(base_arc, buffer_arc, &chunk_refs);
        let rel = std::path::PathBuf::from(rel_path);
        if new_buffer.is_empty() && base_arc.is_empty() {
            new_tree.remove(&rel);
        } else {
            new_tree.insert(rel, new_buffer);
        }
    }

    let head_sha = repo.log_commits(None, 1).into_iter().next().map(|c| c.sha);
    let is_head = head_sha.as_deref() == Some(sha.as_str());

    if is_head {
        match repo.amend_head(&new_tree, None) {
            Ok(new_sha) => {
                emit_review_complete_badge(
                    stoat,
                    &format!(
                        "removed {} hunk(s), HEAD amended",
                        staged_groups.iter().map(|(_, v)| v.len()).sum::<usize>()
                    ),
                );
                reopen_review_on_commit(stoat, &workdir, &new_sha);
            },
            Err(GitApplyError::Backend { reason, .. }) => {
                emit_review_error_badge(stoat, "amend failed", Some(reason));
            },
        }
    } else {
        let descendants = repo
            .log_commits(None, usize::MAX)
            .into_iter()
            .map(|c| c.sha)
            .take_while(|candidate| candidate != &sha)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        match repo.rewrite_commit(&sha, &new_tree, None, &descendants) {
            Ok(result) => {
                let new_sha = result.mapping.get(&sha).cloned().unwrap_or(result.new_head);
                let total: usize = staged_groups.iter().map(|(_, v)| v.len()).sum();
                emit_review_complete_badge(
                    stoat,
                    &format!(
                        "removed {total} hunk(s), rewrote {} commit(s)",
                        descendants.len() + 1
                    ),
                );
                reopen_review_on_commit(stoat, &workdir, &new_sha);
            },
            Err(GitApplyError::Backend { reason, .. }) => {
                emit_review_error_badge(stoat, "rewrite failed", Some(reason));
            },
        }
    }

    UpdateEffect::Redraw
}

fn reopen_review_on_commit(stoat: &mut Stoat, workdir: &Path, sha: &str) {
    let origin = stoat
        .active_workspace()
        .review
        .as_ref()
        .map(|s| s.origin)
        .unwrap_or_default();
    if let Some(mut session) = scan_commit(stoat, workdir, sha) {
        session.origin = origin;
        install_review_session(stoat, session);
    } else {
        // Rewritten commit has no diffs vs. parent. Drop the review;
        // `close_review` routes back to commits mode if that's where the
        // user launched from.
        close_review(stoat);
    }
}

fn emit_review_complete_badge(stoat: &mut Stoat, label: &str) {
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

fn emit_review_info_badge(stoat: &mut Stoat, label: &str) {
    use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};
    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::Review);
    ws.badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: Anchor::BottomRight,
        state: BadgeState::Active,
        label: label.to_string(),
        detail: None,
    });
}

fn emit_review_error_badge(stoat: &mut Stoat, label: &str, detail: Option<String>) {
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

pub(super) fn commits_open_review(stoat: &mut Stoat) -> UpdateEffect {
    use crate::review_session::ReviewOrigin;

    let Some((workdir, sha)) = stoat.active_workspace().commits.as_ref().and_then(|s| {
        s.selected_sha()
            .map(|sha| (s.workdir.clone(), sha.to_string()))
    }) else {
        return UpdateEffect::None;
    };
    let Some(mut session) = scan_commit(stoat, &workdir, &sha) else {
        return UpdateEffect::None;
    };
    session.origin = ReviewOrigin::FromCommits;
    install_review_session(stoat, session);
    UpdateEffect::Redraw
}

pub(super) fn commits_open_branch_review(stoat: &mut Stoat) -> UpdateEffect {
    use crate::review_session::ReviewOrigin;

    let Some((workdir, sha)) = stoat.active_workspace().commits.as_ref().and_then(|s| {
        s.selected_sha()
            .map(|sha| (s.workdir.clone(), sha.to_string()))
    }) else {
        return UpdateEffect::None;
    };
    let Some(mut session) = scan_branch(stoat, &workdir, Some(&sha)) else {
        return UpdateEffect::None;
    };
    session.origin = ReviewOrigin::FromCommits;
    install_review_session(stoat, session);
    UpdateEffect::Redraw
}

pub(super) fn open_review_commit_range(stoat: &mut Stoat, workdir: &Path, from: &str, to: &str) {
    let Some(session) = scan_commit_range(stoat, workdir, from, to) else {
        return;
    };
    install_review_session(stoat, session);
}

pub(super) fn open_review_branch(stoat: &mut Stoat, workdir: &Path, base: Option<&str>) {
    let Some(session) = scan_branch(stoat, workdir, base) else {
        return;
    };
    install_review_session(stoat, session);
}

pub(super) fn open_review_agent_edits(stoat: &mut Stoat, edits: &[stoat_action::AgentEdit]) {
    let proposals: Vec<crate::review_session::AgentEditProposal> = edits
        .iter()
        .map(|e| crate::review_session::AgentEditProposal {
            path: e.path.clone(),
            base_text: e.base_text.clone(),
            proposed_text: e.proposed_text.clone(),
        })
        .collect();
    let Some(session) = scan_agent_edits(stoat, &proposals) else {
        return;
    };
    install_review_session(stoat, session);
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ReviewStep {
    Next,
    Prev,
    NextCommit,
    PrevCommit,
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ReviewMark {
    Stage,
    Unstage,
    Toggle,
    Skip,
    Approve,
    ToggleApproval,
}

pub(super) fn review_step(stoat: &mut Stoat, step: ReviewStep) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    let moved = match step {
        ReviewStep::Next => session.next(),
        ReviewStep::Prev => session.prev(),
        ReviewStep::NextCommit => session.next_commit(),
        ReviewStep::PrevCommit => session.prev_commit(),
    };
    if moved.is_none() {
        return UpdateEffect::None;
    }
    let chunk_id = session.cursor.current;
    let editor_id = session.view_editor;
    sync_review_view_and_scroll(ws, editor_id, chunk_id);
    UpdateEffect::Redraw
}

pub(super) fn review_next_unreviewed(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    if session.next_unreviewed().is_none() {
        return UpdateEffect::None;
    }
    let chunk_id = session.cursor.current;
    let editor_id = session.view_editor;
    sync_review_view_and_scroll(ws, editor_id, chunk_id);
    UpdateEffect::Redraw
}

pub(super) fn review_reset_progress(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    session.reset_progress();
    let chunk_id = session.cursor.current;
    let editor_id = session.view_editor;
    let progress = session.progress();
    sync_review_view_and_scroll(ws, editor_id, chunk_id);
    emit_review_progress_badge(ws, &progress);
    UpdateEffect::Redraw
}

/// Flip the active session's follow flag. Follow-driven cursor
/// jumping on external edits is wired in the GUI workspace; the TUI
/// handler only toggles the flag. No-op without an active review.
pub(super) fn review_toggle_follow(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    session.follow = !session.follow;
    UpdateEffect::Redraw
}

pub(super) fn review_mark(stoat: &mut Stoat, mark: ReviewMark) -> UpdateEffect {
    use crate::review_session::ChunkStatus;

    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    let Some(id) = session.cursor.current else {
        return UpdateEffect::None;
    };
    let mut moved_to: Option<crate::review_session::ReviewChunkId> = None;
    match mark {
        ReviewMark::Stage => session.set_status(id, ChunkStatus::Staged),
        ReviewMark::Unstage => session.set_status(id, ChunkStatus::Unstaged),
        ReviewMark::Toggle => session.toggle_stage(id),
        ReviewMark::Skip => session.set_status(id, ChunkStatus::Skipped),
        ReviewMark::Approve => {
            session.set_approved(id, true);
            moved_to = session.next();
        },
        ReviewMark::ToggleApproval => session.toggle_approved(id),
    }
    let editor_id = session.view_editor;
    let progress = session.progress();
    sync_review_view_and_scroll(ws, editor_id, moved_to);
    emit_review_progress_badge(ws, &progress);

    UpdateEffect::Redraw
}

/// Stage or unstage the chunk under the review cursor directly against
/// the git index, bypassing the batch [`review_apply_staged`] flow.
/// With `force_unstage` the chunk is always reversed back out of the
/// index; otherwise a currently-`Staged` chunk is unstaged and any
/// other chunk is staged. The chunk's session status follows: `Staged`
/// on stage, `Pending` on unstage. No-op unless the source is a working
/// tree, a chunk is under the cursor, and the index apply succeeds.
pub(super) fn git_stage_hunk(stoat: &mut Stoat, force_unstage: bool) -> UpdateEffect {
    use crate::{
        host::GitApplyError,
        review_session::{build_chunk_patch, ChunkStatus},
    };

    let (workdir, id, patch, next_status) = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        let workdir = match &session.source {
            ReviewSource::WorkingTree { workdir } => workdir.clone(),
            _ => {
                tracing::warn!("GitStageHunk: only WorkingTree sources stage to the index");
                return UpdateEffect::None;
            },
        };
        let Some(id) = session.cursor.current else {
            return UpdateEffect::None;
        };
        let Some(chunk) = session.chunks.get(&id) else {
            return UpdateEffect::None;
        };
        let unstage = force_unstage || chunk.status == ChunkStatus::Staged;
        let next_status = if unstage {
            ChunkStatus::Pending
        } else {
            ChunkStatus::Staged
        };
        let Some(patch) = build_chunk_patch(session, [id], unstage) else {
            return UpdateEffect::None;
        };
        (workdir, id, patch, next_status)
    };

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        tracing::warn!("GitStageHunk: no git repo at {}", workdir.display());
        return UpdateEffect::None;
    };
    if let Err(GitApplyError::Backend { reason, .. }) = repo.apply_to_index(&patch) {
        tracing::warn!("GitStageHunk: apply_to_index failed: {reason}");
        return UpdateEffect::None;
    }

    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    session.set_status(id, next_status);
    let editor_id = session.view_editor;
    let progress = session.progress();
    sync_review_view_and_scroll(ws, editor_id, None);
    emit_review_progress_badge(ws, &progress);

    UpdateEffect::Redraw
}

/// Toggle the staged state of a single line of the chunk under the review
/// cursor. Adds (or removes) the chunk's first changed row in its
/// staged-row set and rebuilds the chunk's index state -- reverse the old
/// staged subset, then apply the new -- so adjacent-line stages
/// accumulate. Marks the chunk `Staged` when every changed row is staged,
/// `Pending` when none are, otherwise `PartiallyStaged`. Acts on the
/// chunk's first changed row; precise per-line cursor targeting is a
/// follow-up. Only `WorkingTree` sources stage to the index.
pub(super) fn git_stage_line(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{host::GitApplyError, review::ReviewRow};

    let (workdir, id, plan) = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        let workdir = match &session.source {
            ReviewSource::WorkingTree { workdir } => workdir.clone(),
            _ => {
                tracing::warn!("GitToggleStageLine: only WorkingTree sources stage to the index");
                return UpdateEffect::None;
            },
        };
        let Some(id) = session.cursor.current else {
            return UpdateEffect::None;
        };
        let Some(chunk) = session.chunks.get(&id) else {
            return UpdateEffect::None;
        };
        let Some(row) = chunk
            .hunk
            .rows
            .iter()
            .position(|r| matches!(r, ReviewRow::Changed { .. }))
        else {
            return UpdateEffect::None;
        };
        let Some(plan) = session.plan_line_stage(id, row as u32) else {
            return UpdateEffect::None;
        };
        (workdir, id, plan)
    };

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        tracing::warn!("GitToggleStageLine: no git repo at {}", workdir.display());
        return UpdateEffect::None;
    };
    for patch in [plan.reverse.as_ref(), plan.forward.as_ref()]
        .into_iter()
        .flatten()
    {
        if let Err(GitApplyError::Backend { reason, .. }) = repo.apply_to_index(patch) {
            tracing::warn!("GitToggleStageLine: apply_to_index failed: {reason}");
            return UpdateEffect::None;
        }
    }

    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    session.set_chunk_staged_rows(id, plan.rows, plan.status);
    let editor_id = session.view_editor;
    let progress = session.progress();
    sync_review_view_and_scroll(ws, editor_id, None);
    emit_review_progress_badge(ws, &progress);

    UpdateEffect::Redraw
}

/// Apply the reversed patch of the chunk under the review cursor to the
/// working tree, undoing that change on disk. Reuses
/// [`crate::review_session::build_chunk_patch`] with `reverse = true` and
/// applies it via [`crate::host::GitRepo::apply_to_workdir`]. Works for
/// any workdir-bearing source -- the change being reverted lives on disk
/// regardless of whether the review compares the index, a commit, or a
/// range. Does not change chunk status; a subsequent refresh re-extracts.
pub(super) fn review_revert_hunk(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{host::GitApplyError, review_session::build_chunk_patch};

    let (workdir, patch) = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        let workdir = match &session.source {
            ReviewSource::WorkingTree { workdir }
            | ReviewSource::WorkingTreeUnstaged { workdir }
            | ReviewSource::WorkingTreeStaged { workdir }
            | ReviewSource::WorkspaceWatch { workdir }
            | ReviewSource::Commit { workdir, .. }
            | ReviewSource::CommitRange { workdir, .. } => workdir.clone(),
            _ => {
                tracing::warn!("ReviewRevertHunk: source has no working tree to revert against");
                return UpdateEffect::None;
            },
        };
        let Some(id) = session.cursor.current else {
            return UpdateEffect::None;
        };
        let Some(patch) = build_chunk_patch(session, [id], true) else {
            return UpdateEffect::None;
        };
        (workdir, patch)
    };

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        tracing::warn!("ReviewRevertHunk: no git repo at {}", workdir.display());
        return UpdateEffect::None;
    };
    if let Err(GitApplyError::Backend { reason, .. }) = repo.apply_to_workdir(&patch) {
        tracing::warn!("ReviewRevertHunk: apply_to_workdir failed: {reason}");
        return UpdateEffect::None;
    }

    UpdateEffect::Redraw
}

/// Insert or update the [`crate::badge::BadgeSource::Review`] badge to
/// match `progress`. Inserts a [`crate::badge::BadgeState::Complete`]
/// badge with running counters when the review is complete; removes any
/// existing review badge otherwise. Idempotent: callers run this on
/// every progress-affecting transition, including external-edit
/// refreshes, so the badge tracks the latest counters.
fn emit_review_progress_badge(ws: &mut Workspace, progress: &ReviewProgress) {
    use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};

    let existing = ws.badges.find_by_source(BadgeSource::Review);
    if !progress.is_complete() {
        if let Some(bid) = existing {
            ws.badges.remove(bid);
        }
        return;
    }

    let label = format!("review complete: {} chunks", progress.total);
    let detail = format!(
        "{} staged · {} unstaged · {} skipped",
        progress.staged, progress.unstaged, progress.skipped
    );
    match existing {
        Some(bid) => {
            if let Some(badge) = ws.badges.get_mut(bid) {
                badge.state = BadgeState::Complete;
                badge.label = label;
                badge.detail = Some(detail);
            }
        },
        None => {
            ws.badges.insert(Badge {
                source: BadgeSource::Review,
                anchor: Anchor::BottomRight,
                state: BadgeState::Complete,
                label,
                detail: Some(detail),
            });
        },
    }
}

/// Refresh the editor's review view cache from the session and, if a chunk
/// is supplied, scroll so that chunk sits near the top of the pane. Split
/// borrow of `ws.editors` and `ws.review` is done here so callers can drop
/// their `&mut ws.review` borrow before invoking.
fn sync_review_view_and_scroll(
    ws: &mut Workspace,
    editor_id: Option<EditorId>,
    scroll_to_chunk: Option<crate::review_session::ReviewChunkId>,
) {
    let Some(editor_id) = editor_id else { return };
    let Some(editor) = ws.editors.get_mut(editor_id) else {
        return;
    };
    let Some(view) = editor.review_view.as_mut() else {
        return;
    };
    if let Some(session) = ws.review.as_ref() {
        view.refresh_from_session(session);
    }
    if let Some(chunk_id) = scroll_to_chunk {
        if let Some(row) = view.row_of_chunk(chunk_id) {
            editor.scroll_row = row.saturating_sub(3);
        }
    }
}

pub(super) fn review_apply_staged(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{
        badge::{Anchor, Badge, BadgeSource, BadgeState},
        host::GitApplyError,
        review_apply::chunk_to_unified_diff,
        review_session::{ChunkStatus, ReviewChunkId},
    };

    let (staged, workdir): (Vec<(ReviewChunkId, String)>, std::path::PathBuf) = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        let workdir = match &session.source {
            ReviewSource::WorkingTree { workdir }
            | ReviewSource::WorkingTreeUnstaged { workdir }
            | ReviewSource::WorkingTreeStaged { workdir } => workdir.clone(),
            _ => {
                tracing::warn!(
                    "ReviewApplyStaged: only WorkingTree sources are applyable; \
                     other sources are read-only reviews"
                );
                return UpdateEffect::None;
            },
        };
        let staged = session
            .order
            .iter()
            .filter_map(|id| {
                let c = session.chunks.get(id)?;
                if c.status != ChunkStatus::Staged {
                    return None;
                }
                let file = session.files.get(c.file_index)?;
                Some((*id, chunk_to_unified_diff(file, c, &workdir, false)))
            })
            .collect();
        (staged, workdir)
    };

    if staged.is_empty() {
        tracing::info!("ReviewApplyStaged: nothing staged");
        return UpdateEffect::None;
    }

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        tracing::warn!("ReviewApplyStaged: no git repo at {}", workdir.display());
        return UpdateEffect::None;
    };

    let total = staged.len();
    let mut applied = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (_, patch) in &staged {
        match repo.apply_to_index(patch) {
            Ok(()) => applied += 1,
            Err(GitApplyError::Backend { reason, .. }) => failures.push(reason),
        }
    }

    {
        let ws = stoat.active_workspace_mut();
        ws.badges.remove_by_source(BadgeSource::Review);
        let (state, label, detail) = if failures.is_empty() {
            (
                BadgeState::Complete,
                format!("applied {applied} chunk{}", plural(applied)),
                None,
            )
        } else {
            (
                BadgeState::Error,
                format!("applied {applied} of {total} chunks"),
                Some(failures.first().cloned().unwrap_or_default()),
            )
        };
        ws.badges.insert(Badge {
            source: BadgeSource::Review,
            anchor: Anchor::BottomRight,
            state,
            label,
            detail,
        });
    }

    if failures.is_empty() && applied > 0 {
        return review_refresh(stoat);
    }
    UpdateEffect::Redraw
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

pub(super) fn review_external_edit(stoat: &mut Stoat, path: &Path) -> UpdateEffect {
    let watch_workdir = match stoat.active_workspace().review.as_ref().map(|s| &s.source) {
        Some(ReviewSource::WorkspaceWatch { workdir }) => Some(workdir.clone()),
        _ => None,
    };
    if let Some(workdir) = watch_workdir {
        return review_watch_edit(stoat, path, &workdir);
    }

    let in_session = stoat
        .active_workspace()
        .review
        .as_ref()
        .is_some_and(|s| s.files.iter().any(|f| f.path == path));
    if !in_session {
        return UpdateEffect::None;
    }

    let effect = review_refresh(stoat);

    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_ref() else {
        return effect;
    };
    let editor_id = session.view_editor;
    let progress = session.progress();
    let chunk_id = session
        .files
        .iter()
        .position(|f| f.path == path)
        .and_then(|file_index| session.chunk_containing_buffer_byte(file_index, 0));
    sync_review_view_and_scroll(ws, editor_id, chunk_id);
    emit_review_progress_badge(ws, &progress);
    UpdateEffect::Redraw
}

/// Handle one `FsWatchEvent` against a [`ReviewSource::WorkspaceWatch`]
/// session. Re-reads `path` from `fs_host`, re-derives the base from
/// `git_host`'s HEAD, and dispatches an incremental
/// [`ReviewSession::upsert_file`] -- which adds the file when it's
/// new, replaces its chunks when known, or drops the entry when the
/// diff becomes empty. The cursor scrolls to the new chunk with the
/// smallest buffer byte so the user sees the freshest change.
fn review_watch_edit(stoat: &mut Stoat, path: &Path, workdir: &Path) -> UpdateEffect {
    if !path.starts_with(workdir) || stoat.fs_host.is_ignored(workdir, path) {
        return UpdateEffect::None;
    }

    let buffer_text = {
        let mut buf = Vec::new();
        match stoat.fs_host.read(path, &mut buf) {
            Ok(()) => match String::from_utf8(buf) {
                Ok(text) => text,
                Err(err) => {
                    tracing::warn!(
                        target: "stoat::review",
                        ?path,
                        %err,
                        "ReviewExternalEdit (watch): file is not valid UTF-8, skipping",
                    );
                    return UpdateEffect::None;
                },
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                tracing::warn!(
                    target: "stoat::review",
                    ?path,
                    %err,
                    "ReviewExternalEdit (watch): fs read failed, skipping",
                );
                return UpdateEffect::None;
            },
        }
    };

    let Some(repo) = stoat.git_host.discover(workdir) else {
        tracing::warn!(
            target: "stoat::review",
            workdir = %workdir.display(),
            "ReviewExternalEdit (watch): no git repo at workdir, skipping",
        );
        return UpdateEffect::None;
    };
    let base_text = repo.head_content(path).unwrap_or_default();
    let language = stoat.language_registry.for_path(path);
    let rel_path = path
        .strip_prefix(workdir)
        .unwrap_or(path)
        .display()
        .to_string();

    let input = ReviewFileInput {
        path: path.to_path_buf(),
        rel_path,
        language,
        base_text: Arc::new(base_text),
        buffer_text: Arc::new(buffer_text),
    };

    let new_ids = {
        let ws = stoat.active_workspace_mut();
        let Some(session) = ws.review.as_mut() else {
            return UpdateEffect::None;
        };
        session.upsert_file(input)
    };

    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_ref() else {
        return UpdateEffect::Redraw;
    };
    let editor_id = session.view_editor;
    let progress = session.progress();
    let scroll_to = new_ids
        .iter()
        .filter_map(|id| {
            session
                .chunks
                .get(id)
                .map(|c| (c.buffer_byte_range.start, *id))
        })
        .min_by_key(|(start, _)| *start)
        .map(|(_, id)| id);
    sync_review_view_and_scroll(ws, editor_id, scroll_to);
    emit_review_progress_badge(ws, &progress);
    UpdateEffect::Redraw
}

pub(super) fn review_refresh(stoat: &mut Stoat) -> UpdateEffect {
    use crate::review_session::ChunkIdentity;
    use std::collections::HashMap;

    let source = {
        let ws = stoat.active_workspace();
        let Some(old) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        old.source.clone()
    };

    let carried: HashMap<ChunkIdentity, crate::review_session::ChunkStatus> = {
        let ws = stoat.active_workspace();
        let old = ws
            .review
            .as_ref()
            .expect("review session still present (early-returned above when absent)");
        old.order
            .iter()
            .filter_map(|id| {
                let status = old.chunks.get(id)?.status;
                if !status.is_decided() {
                    return None;
                }
                let ident = old.identity_key(*id)?;
                Some((ident, status))
            })
            .collect()
    };

    let Some(mut new_session) = rescan_source(stoat, &source) else {
        return UpdateEffect::None;
    };

    let ids: Vec<_> = new_session.order.clone();
    for id in ids {
        if let Some(ident) = new_session.identity_key(id) {
            if let Some(status) = carried.get(&ident).copied() {
                new_session.set_status(id, status);
            }
        }
    }

    install_review_session(stoat, new_session);
    UpdateEffect::Redraw
}

/// Cycle the active review to the next diff-comparison source
/// ([`ReviewSource::next_comparison`]): WorkingTree -> unstaged-only ->
/// staged-only -> the HEAD commit -> back to WorkingTree. Rebuilds the
/// session from the new source, carrying decided statuses and approval
/// flags across by [`crate::review_session::ChunkFingerprint`] so a chunk
/// whose content matches in the new source keeps its decision. No-op when
/// no review is open or the current source is outside the cycle
/// (`WorkspaceWatch`, `CommitRange`, `AgentEdits`, `InMemory`).
pub(super) fn review_cycle_comparison_mode(stoat: &mut Stoat) -> UpdateEffect {
    let source = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        session.source.clone()
    };

    let workdir = match &source {
        ReviewSource::WorkingTree { workdir }
        | ReviewSource::WorkingTreeUnstaged { workdir }
        | ReviewSource::WorkingTreeStaged { workdir }
        | ReviewSource::Commit { workdir, .. } => workdir.clone(),
        _ => return UpdateEffect::None,
    };

    let head_sha = stoat
        .git_host
        .discover(&workdir)
        .and_then(|repo| repo.log_commits(None, 1).first().map(|c| c.sha.clone()));

    let Some(next) = source.next_comparison(head_sha.as_deref()) else {
        return UpdateEffect::None;
    };

    let (statuses, approvals) = {
        let ws = stoat.active_workspace();
        let session = ws
            .review
            .as_ref()
            .expect("review session still present (early-returned above when absent)");
        (session.snapshot_statuses(), session.snapshot_approvals())
    };

    let mut new_session =
        rescan_source(stoat, &next).unwrap_or_else(|| ReviewSession::new(next.clone()));
    new_session.apply_statuses(&statuses);
    new_session.apply_approvals(&approvals);

    install_review_session(stoat, new_session);
    UpdateEffect::Redraw
}

pub(super) fn enter_line_select(stoat: &mut Stoat) -> UpdateEffect {
    let entered = {
        let Some(session) = stoat.active_workspace_mut().review.as_mut() else {
            return UpdateEffect::None;
        };
        match session.cursor.current {
            Some(id) => session.enter_line_select(id),
            None => false,
        }
    };
    if !entered {
        return UpdateEffect::None;
    }
    stoat.mode = "line_select".to_string();
    UpdateEffect::Redraw
}

pub(super) fn line_select_cancel(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(session) = stoat.active_workspace_mut().review.as_mut() {
        session.cancel_line_select();
    }
    stoat.mode = "review".to_string();
    UpdateEffect::Redraw
}

/// Toggle the selected bit of the line under the review cursor. The TUI
/// review has no per-row editor cursor (see [`git_stage_line`]), so this
/// targets the first changed row of the active selection.
pub(super) fn line_select_toggle(stoat: &mut Stoat) -> UpdateEffect {
    use crate::review::ReviewRow;

    let Some(session) = stoat.active_workspace_mut().review.as_mut() else {
        return UpdateEffect::None;
    };
    let row = {
        let Some(sel) = session.line_selection.as_ref() else {
            return UpdateEffect::None;
        };
        sel.lines.iter().find_map(|r| match r {
            ReviewRow::Changed {
                right: Some(side), ..
            } => Some(side.line_num.saturating_sub(1)),
            _ => None,
        })
    };
    let Some(row) = row else {
        return UpdateEffect::None;
    };
    if session.toggle_line_select(row) {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

/// Select every row of the active line selection.
pub(super) fn line_select_all(stoat: &mut Stoat) -> UpdateEffect {
    let Some(session) = stoat.active_workspace_mut().review.as_mut() else {
        return UpdateEffect::None;
    };
    if session.select_all_lines() {
        UpdateEffect::Redraw
    } else {
        UpdateEffect::None
    }
}

/// Stage (or unstage, when `unstage`) the active line selection's selected
/// rows by applying its partial-hunk patch to the index, then clear the
/// selection and return to review mode. WorkingTree sources only.
pub(super) fn line_select_stage(stoat: &mut Stoat, unstage: bool) -> UpdateEffect {
    use crate::host::GitApplyError;

    let (workdir, id, plan) = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        let workdir = match &session.source {
            ReviewSource::WorkingTree { workdir } => workdir.clone(),
            _ => {
                tracing::warn!(
                    "ReviewLineSelectStage: only WorkingTree sources stage to the index"
                );
                return UpdateEffect::None;
            },
        };
        let Some(id) = session.line_selection.as_ref().map(|s| s.hunk_id) else {
            return UpdateEffect::None;
        };
        let Some(plan) = session.plan_line_select_stage(unstage) else {
            return UpdateEffect::None;
        };
        (workdir, id, plan)
    };

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        tracing::warn!(
            "ReviewLineSelectStage: no git repo at {}",
            workdir.display()
        );
        return UpdateEffect::None;
    };
    for patch in [plan.reverse.as_ref(), plan.forward.as_ref()]
        .into_iter()
        .flatten()
    {
        if let Err(GitApplyError::Backend { reason, .. }) = repo.apply_to_index(patch) {
            tracing::warn!("ReviewLineSelectStage: apply_to_index failed: {reason}");
            return UpdateEffect::None;
        }
    }

    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    session.set_chunk_staged_rows(id, plan.rows, plan.status);
    session.cancel_line_select();
    let editor_id = session.view_editor;
    let progress = session.progress();
    sync_review_view_and_scroll(ws, editor_id, None);
    emit_review_progress_badge(ws, &progress);
    stoat.mode = "review".to_string();
    UpdateEffect::Redraw
}

/// Re-scan the underlying source of a review session. Returns `None` when
/// the source has no re-scannable state (currently `InMemory`) or when the
/// scan produced no hunks.
fn rescan_source(stoat: &Stoat, source: &ReviewSource) -> Option<ReviewSession> {
    match source {
        ReviewSource::WorkingTree { workdir } => scan_working_tree(stoat, workdir),
        ReviewSource::WorkingTreeUnstaged { workdir } => {
            build_working_tree_session(stoat, workdir, Some(false))
        },
        ReviewSource::WorkingTreeStaged { workdir } => {
            build_working_tree_session(stoat, workdir, Some(true))
        },
        ReviewSource::WorkspaceWatch { .. } => None,
        ReviewSource::Commit { workdir, sha } => scan_commit(stoat, workdir, sha),
        ReviewSource::CommitRange { workdir, from, to } => {
            scan_commit_range(stoat, workdir, from, to)
        },
        // Branch review is opened per-commit from the GUI; the TUI rescan
        // path has no per-commit builder, so it is not rescannable here.
        ReviewSource::Branch { .. } => None,
        ReviewSource::AgentEdits { edits } => scan_agent_edits(stoat, edits.as_ref()),
        ReviewSource::InMemory { files } => scan_in_memory(stoat, files.as_ref()),
    }
}

pub(super) fn close_review(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{badge::BadgeSource, review_session::ReviewOrigin};

    let executor = stoat.executor.clone();
    let fs_watch_host = stoat.fs_watch_host.clone();
    let ws = stoat.active_workspace_mut();
    let Some(mut session) = ws.review.take() else {
        return UpdateEffect::None;
    };
    for token in std::mem::take(&mut session.watch_tokens) {
        fs_watch_host.unwatch(token);
    }
    let origin = session.origin;
    ws.badges.remove_by_source(BadgeSource::Review);
    let next_mode = match origin {
        ReviewOrigin::FromCommits if ws.commits.is_some() => "commits",
        _ => "normal",
    };
    let Some(review_editor_id) = session.view_editor else {
        stoat.mode = next_mode.to_string();
        return UpdateEffect::Redraw;
    };

    let (scratch_id, scratch_buffer) = ws.buffers.new_scratch();
    let replacement = EditorState::new(scratch_id, scratch_buffer, executor);
    let replacement_id = ws.editors.insert(replacement);

    let focused = ws.panes.focus();
    let replace_focused = matches!(
        ws.panes.pane(focused).view,
        View::Editor(eid) if eid == review_editor_id
    );
    if replace_focused {
        ws.panes.pane_mut(focused).view = View::Editor(replacement_id);
    } else {
        ws.editors.remove(replacement_id);
    }

    let still_referenced = ws
        .panes
        .split_panes()
        .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == review_editor_id));
    if !still_referenced {
        ws.editors.remove(review_editor_id);
    }

    stoat.mode = next_mode.to_string();
    UpdateEffect::Redraw
}

pub(super) fn open_review(stoat: &mut Stoat) {
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(session) = scan_working_tree(stoat, &git_root) else {
        return;
    };
    install_review_session(stoat, session);
}

/// Open an empty review session that surfaces live edits inside
/// `workdir`. Files and chunks are added incrementally by the
/// workspace-watch event loop; see `ReviewSource::WorkspaceWatch`.
pub(super) fn open_review_watch(stoat: &mut Stoat, workdir: &Path) {
    let session = ReviewSession::new(ReviewSource::WorkspaceWatch {
        workdir: workdir.to_path_buf(),
    });
    install_review_session(stoat, session);
}

/// Build a review session by scanning the git working tree rooted at
/// `git_root`. Returns `None` when the root is not a repository or has no
/// diff hunks. Shared by [`open_review`] and [`review_refresh`].
fn scan_working_tree(stoat: &Stoat, git_root: &Path) -> Option<ReviewSession> {
    build_working_tree_session(stoat, git_root, None)
}

/// Build a working-tree review session, optionally restricted to staged
/// (`Some(true)`) or unstaged (`Some(false)`) changes. `None` scans every
/// changed path. The resulting session's [`ReviewSource`] matches the
/// filter -- `WorkingTree`, `WorkingTreeStaged`, or `WorkingTreeUnstaged`
/// -- so a later refresh re-scans the same subset. Returns `None` when the
/// root is not a repository or the (filtered) scan has no diff hunks.
fn build_working_tree_session(
    stoat: &Stoat,
    git_root: &Path,
    staged_filter: Option<bool>,
) -> Option<ReviewSession> {
    let Some((workdir, inputs)) = crate::diff::scan_working_tree(
        &*stoat.git_host,
        &*stoat.fs_host,
        &stoat.language_registry,
        git_root,
        None,
        staged_filter,
    ) else {
        tracing::warn!("working-tree review: no changes to review");
        return None;
    };

    let source = match staged_filter {
        None => ReviewSource::WorkingTree { workdir },
        Some(false) => ReviewSource::WorkingTreeUnstaged { workdir },
        Some(true) => ReviewSource::WorkingTreeStaged { workdir },
    };
    let mut session = ReviewSession::new(source);
    session.add_files(inputs);

    if session.order.is_empty() {
        tracing::warn!("working-tree review: no diff hunks to display");
        return None;
    }

    Some(session)
}

/// Build a session from a single commit by diffing its tree against its
/// first parent (or the empty tree for a root commit). Returns `None` when
/// the repo or sha is unknown, or when no paths differ.
pub(super) fn scan_commit(stoat: &Stoat, workdir: &Path, sha: &str) -> Option<ReviewSession> {
    let repo = stoat.git_host.discover(workdir)?;
    let workdir = repo.workdir()?;
    let new_tree = repo.commit_tree(sha)?;
    let base_tree = match repo.parent_sha(sha) {
        Some(parent) => repo.commit_tree(&parent).unwrap_or_default(),
        None => std::collections::BTreeMap::new(),
    };
    build_session_from_trees(
        stoat,
        ReviewSource::Commit {
            workdir: workdir.clone(),
            sha: sha.to_string(),
        },
        &workdir,
        &base_tree,
        &new_tree,
    )
}

/// Build a session from a commit range `from..=to` (inclusive of `to`,
/// exclusive of `from` -- same as `git diff from..to`). Pairs each path
/// in either tree to form file-level base/buffer contents.
fn scan_commit_range(stoat: &Stoat, workdir: &Path, from: &str, to: &str) -> Option<ReviewSession> {
    let repo = stoat.git_host.discover(workdir)?;
    let workdir = repo.workdir()?;
    let base_tree = repo.commit_tree(from).unwrap_or_default();
    let new_tree = repo.commit_tree(to)?;
    build_session_from_trees(
        stoat,
        ReviewSource::CommitRange {
            workdir: workdir.clone(),
            from: from.to_string(),
            to: to.to_string(),
        },
        &workdir,
        &base_tree,
        &new_tree,
    )
}

/// Build a per-commit branch review session: resolve the base, enumerate
/// the commits in `merge_base(base, HEAD)..HEAD` oldest-first, and add each
/// commit's diff (commit-tree vs parent) tagged with its sha via
/// [`ReviewSession::add_commit_files`], so `order` groups commit-by-commit.
/// Returns `None` when the repo is unknown, the base does not resolve, or no
/// commit contributes a diff.
fn scan_branch(stoat: &Stoat, workdir: &Path, base: Option<&str>) -> Option<ReviewSession> {
    let repo = stoat.git_host.discover(workdir)?;
    let workdir = repo.workdir()?;
    let base_sha = host::resolve_review_base(repo.as_ref(), base)?;
    let mut session = ReviewSession::new(ReviewSource::Branch {
        workdir: workdir.clone(),
        base: base.map(String::from),
    });
    for commit in repo.branch_commits(&base_sha) {
        let Some(new_tree) = repo.commit_tree(&commit.sha) else {
            continue;
        };
        let base_tree = match repo.parent_sha(&commit.sha) {
            Some(parent) => repo.commit_tree(&parent).unwrap_or_default(),
            None => std::collections::BTreeMap::new(),
        };
        let inputs = review_inputs_from_trees(stoat, &workdir, &base_tree, &new_tree);
        if !inputs.is_empty() {
            session.set_commit_summary(commit.sha.clone(), commit.summary);
            session.add_commit_files(commit.sha, inputs);
        }
    }
    if session.order.is_empty() {
        return None;
    }
    Some(session)
}

/// Build a session from a stored slice of [`crate::review_session::InMemoryFile`].
/// Mirrors [`scan_agent_edits`] for `ReviewSource::InMemory`-built sessions
/// so [`review_refresh`] can re-derive hunks instead of being a silent no-op.
fn scan_in_memory(
    stoat: &Stoat,
    files: &[crate::review_session::InMemoryFile],
) -> Option<ReviewSession> {
    if files.is_empty() {
        return None;
    }
    let mut session = ReviewSession::new(ReviewSource::InMemory {
        files: Arc::new(files.to_vec()),
    });
    let inputs: Vec<ReviewFileInput> = files
        .iter()
        .map(|file| ReviewFileInput {
            path: file.path.clone(),
            rel_path: file.path.display().to_string(),
            language: stoat.language_registry.for_path(&file.path),
            base_text: file.base_text.clone(),
            buffer_text: file.buffer_text.clone(),
        })
        .collect();
    session.add_files(inputs);
    if session.order.is_empty() {
        return None;
    }
    Some(session)
}

/// Build a session from a list of agent edit proposals. No repo access
/// needed; each proposal becomes one file in the session with the given
/// `base_text`/`proposed_text`.
fn scan_agent_edits(
    stoat: &Stoat,
    edits: &[crate::review_session::AgentEditProposal],
) -> Option<ReviewSession> {
    if edits.is_empty() {
        return None;
    }
    let mut session = ReviewSession::new(ReviewSource::AgentEdits {
        edits: Arc::new(edits.to_vec()),
    });
    let inputs: Vec<ReviewFileInput> = edits
        .iter()
        .map(|edit| ReviewFileInput {
            path: edit.path.clone(),
            rel_path: edit.path.display().to_string(),
            language: stoat.language_registry.for_path(&edit.path),
            base_text: edit.base_text.clone(),
            buffer_text: edit.proposed_text.clone(),
        })
        .collect();
    session.add_files(inputs);
    if session.order.is_empty() {
        return None;
    }
    Some(session)
}

/// Common builder used by [`scan_commit`] / [`scan_commit_range`]. Builds
/// the file inputs via [`review_inputs_from_trees`], skipping when no path
/// differs.
fn build_session_from_trees(
    stoat: &Stoat,
    source: ReviewSource,
    workdir: &Path,
    base_tree: &std::collections::BTreeMap<std::path::PathBuf, String>,
    new_tree: &std::collections::BTreeMap<std::path::PathBuf, String>,
) -> Option<ReviewSession> {
    let inputs = review_inputs_from_trees(stoat, workdir, base_tree, new_tree);
    if inputs.is_empty() {
        return None;
    }
    let mut session = ReviewSession::new(source);
    session.add_files(inputs);
    if session.order.is_empty() {
        return None;
    }
    Some(session)
}

/// Build the per-file review inputs for the union of paths across
/// `base_tree` and `new_tree`, skipping any pair whose base and buffer
/// contents are equal. Shared by the single-diff [`build_session_from_trees`]
/// and the per-commit [`scan_branch`].
fn review_inputs_from_trees(
    stoat: &Stoat,
    workdir: &Path,
    base_tree: &std::collections::BTreeMap<std::path::PathBuf, String>,
    new_tree: &std::collections::BTreeMap<std::path::PathBuf, String>,
) -> Vec<ReviewFileInput> {
    let mut paths: std::collections::BTreeSet<&Path> = std::collections::BTreeSet::new();
    for p in base_tree.keys() {
        paths.insert(p.as_path());
    }
    for p in new_tree.keys() {
        paths.insert(p.as_path());
    }
    let mut inputs: Vec<ReviewFileInput> = Vec::new();
    for rel in paths {
        let base = base_tree.get(rel).cloned().unwrap_or_default();
        let buffer = new_tree.get(rel).cloned().unwrap_or_default();
        if base == buffer {
            continue;
        }
        let abs = workdir.join(rel);
        let lang = stoat.language_registry.for_path(&abs);
        inputs.push(ReviewFileInput {
            path: abs,
            rel_path: rel.display().to_string(),
            language: lang,
            base_text: Arc::new(base),
            buffer_text: Arc::new(buffer),
        });
    }
    inputs
}

/// Build a flattened [`ReviewViewState`] and chunk-header [`BlockProperties`]
/// from the session, spawn a placeholder buffer + editor to host the view,
/// and swap it into the focused pane. The session is stored on the
/// workspace; the editor references it indirectly via `review_view`.
pub(crate) fn install_review_session(stoat: &mut Stoat, mut session: ReviewSession) {
    populate_diff_cache(stoat, &session);

    let fs_watch_host = stoat.fs_watch_host.clone();
    let stale_tokens: Vec<WatchToken> = stoat
        .active_workspace_mut()
        .review
        .as_mut()
        .map(|old| std::mem::take(&mut old.watch_tokens))
        .unwrap_or_default();
    for token in stale_tokens {
        fs_watch_host.unwatch(token);
    }

    if matches!(session.source, ReviewSource::WorkingTree { .. }) {
        for file in &session.files {
            match fs_watch_host.watch(&file.path) {
                Ok(token) => session.watch_tokens.push(token),
                Err(err) => tracing::warn!(
                    target: "stoat::review",
                    path = %file.path.display(),
                    error = %err,
                    "fs watch failed; external edits won't refresh this file",
                ),
            }
        }
    } else if let ReviewSource::WorkspaceWatch { workdir } = &session.source {
        match fs_watch_host.watch_dir(workdir) {
            Ok(token) => session.watch_tokens.push(token),
            Err(err) => tracing::warn!(
                target: "stoat::review",
                path = %workdir.display(),
                error = %err,
                "fs watch failed; workspace-watch review won't observe edits",
            ),
        }
    }

    let view = ReviewViewState::from_session(&session);
    let blocks = build_review_blocks(&session, &view);
    let row_count = view.rows.len();

    let placeholder = " \n".repeat(row_count.saturating_sub(1)) + " ";
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let (buffer_id, buffer) = ws.buffers.new_scratch();
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.edit(0..0, &placeholder);
        guard.dirty = false;
    }

    let mut editor = EditorState::new(buffer_id, buffer, executor);
    editor.display_map.insert_blocks(blocks);
    editor.review_view = Some(view);

    let new_editor_id = ws.editors.insert(editor);
    session.view_editor = Some(new_editor_id);
    ws.review = Some(session);

    let focused = ws.panes.focus();
    let old = match ws.panes.pane(focused).view {
        View::Editor(eid) => Some(eid),
        _ => None,
    };
    ws.panes.pane_mut(focused).view = View::Editor(new_editor_id);
    if let Some(old_id) = old {
        let still_referenced = ws
            .panes
            .split_panes()
            .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == old_id));
        if !still_referenced {
            ws.editors.remove(old_id);
        }
    }

    stoat.mode = "review".to_string();
}

/// Mirror this session's freshly-extracted hunks into the diff cache so
/// a `stoat diff` CLI invocation that hashes the same `(base, buffer,
/// language)` tuple gets a cache hit instead of recomputing.
fn populate_diff_cache(stoat: &Stoat, session: &ReviewSession) {
    use crate::{diff_cache::DiffCacheKey, review::ReviewHunk};

    let mut cache = stoat.diff_cache.lock().expect("diff_cache poisoned");
    for file in &session.files {
        let hunks: Vec<ReviewHunk> = file
            .chunks
            .iter()
            .filter_map(|id| session.chunks.get(id).map(|c| c.hunk.clone()))
            .collect();
        let key = DiffCacheKey {
            left_hash: blake3::hash(file.base_text.as_bytes()).into(),
            right_hash: blake3::hash(file.buffer_text.as_bytes()).into(),
            language: file.language.as_ref().map(|l| l.name.to_string()),
        };
        cache.insert(key, Arc::new(hunks));
    }
}

fn build_review_blocks(session: &ReviewSession, view: &ReviewViewState) -> Vec<BlockProperties> {
    let mut blocks: Vec<BlockProperties> = Vec::with_capacity(view.chunk_row_starts.len());
    for (chunk_id, row) in &view.chunk_row_starts {
        let Some(chunk) = session.chunks.get(chunk_id) else {
            continue;
        };
        let Some(file) = session.files.get(chunk.file_index) else {
            continue;
        };
        let file_total = file.chunks.len();
        let lang_str = file
            .language
            .as_ref()
            .map(|l| l.name.to_string())
            .unwrap_or_default();
        let label = format!(
            "{} --- {}/{} --- {}",
            file.rel_path,
            chunk.chunk_index_in_file + 1,
            file_total,
            lang_str
        );
        let render: RenderBlock = {
            let label = label.clone();
            Arc::new(move |_ctx| {
                vec![Line::styled(
                    label.clone(),
                    Style::default().fg(Color::Yellow),
                )]
            })
        };
        blocks.push(BlockProperties {
            placement: BlockPlacement::Above(*row),
            height: Some(1),
            style: BlockStyle::Fixed,
            render,
            diff_status: None,
            priority: 0,
        });
    }
    blocks
}

#[cfg(test)]
mod tests {
    use crate::{
        app::REVIEW_EXTERNAL_EDIT_DEBOUNCE, diff_cache::DiffCacheKey, host::FsEventKind,
        review_session::ChunkStatus, test_harness::TestHarness,
    };
    use std::path::PathBuf;

    #[test]
    fn install_review_session_populates_diff_cache() {
        let mut h = TestHarness::with_size(80, 10);
        let base = "fn a() { 1 }\n";
        let buffer = "fn a() { 2 }\n";
        h.open_review_from_texts(&[("a.rs", base, buffer)]);

        let key = DiffCacheKey {
            left_hash: blake3::hash(base.as_bytes()).into(),
            right_hash: blake3::hash(buffer.as_bytes()).into(),
            language: Some("rust".to_string()),
        };
        let cache = h.stoat.diff_cache();
        let mut guard = cache.lock().expect("diff_cache poisoned");
        let hunks = guard.lookup(&key).expect("cache hit after install");
        assert!(!hunks.is_empty(), "cached hunks should not be empty");
    }

    /// (a) An external edit that introduces a second hunk grows
    /// `ReviewProgress.total` from 1 to 2 and parks the chunk
    /// cursor on the chunk that contains the smallest changed
    /// buffer offset.
    #[test]
    fn external_edit_adds_new_chunk_grows_progress() {
        let head = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let buffer = "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", head, buffer)]);
        h.stoat.open_review();
        h.settle();

        assert_review_total(&h, 1);

        h.external_edit("a.rs", "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n");
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert_review_total(&h, 2);
        let chunk_id = h.current_review_chunk_id();
        assert_eq!(
            h.with_review(|s| s.chunk(chunk_id).unwrap().file_index),
            0,
            "cursor parks on the chunk in the touched file",
        );
    }

    #[test]
    fn git_stage_line_stages_one_line_and_marks_partially_staged() {
        use crate::review_session::ChunkStatus;

        let head = "a\nb\nc\nd\ne\n";
        let buffer = "a\nB\nC\nD\ne\n";
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", head, buffer)]);
        h.stoat.open_review();
        h.settle();

        let id = h.current_review_chunk_id();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GitToggleStageLine);

        let patches = h.fake_git.applied_patches(std::path::Path::new("/work"));
        assert_eq!(patches.len(), 1, "one line produces one patch: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-b\n") && patch.contains("+B\n"),
            "patch must change the first hunk line b -> B: {patch}"
        );
        assert!(
            !patch.contains("-c\n") && !patch.contains("-d\n"),
            "patch must not touch the other changed lines: {patch}"
        );
        assert_eq!(h.chunk_status(id), ChunkStatus::PartiallyStaged);
    }

    #[test]
    fn enter_line_select_sets_mode_then_cancel_returns_to_review() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\na\ny\n", "x\nb\ny\n")]);
        h.stoat.open_review();
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewEnterLineSelect);
        assert_eq!(h.stoat.mode, "line_select");
        assert!(h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .line_selection
            .is_some());

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewLineSelectCancel);
        assert_eq!(h.stoat.mode, "review");
        assert!(h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .line_selection
            .is_none());
    }

    #[test]
    fn line_select_toggle_clears_changed_row_then_select_all_restores() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\na\ny\n", "x\nb\ny\n")]);
        h.stoat.open_review();
        h.settle();

        let bits = |h: &TestHarness| {
            h.stoat
                .active_workspace()
                .review
                .as_ref()
                .unwrap()
                .line_selection
                .as_ref()
                .unwrap()
                .selected
                .clone()
        };

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewEnterLineSelect);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewLineSelectToggle);
        assert_eq!(
            bits(&h),
            vec![true, false, true],
            "toggle clears the changed row's bit"
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewLineSelectAll);
        assert_eq!(
            bits(&h),
            vec![true, true, true],
            "select-all restores every bit"
        );
    }

    #[test]
    fn line_select_stage_applies_selected_rows_and_returns_to_review() {
        let base = "a\nOLD1\nOLD2\nOLD3\nz\n";
        let buffer = "a\nNEW1\nNEW2\nNEW3\nz\n";
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", base, buffer)]);
        h.stoat.open_review();
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewEnterLineSelect);
        // Deselect the middle changed row (NEW2 at buffer row 2).
        h.stoat
            .active_workspace_mut()
            .review
            .as_mut()
            .unwrap()
            .toggle_line_select(2);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewLineSelectStage);

        assert_eq!(h.stoat.mode, "review");
        assert!(h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .line_selection
            .is_none());
        assert_eq!(
            h.fake_git
                .staged_content(std::path::Path::new("/work"), "a.rs"),
            Some("a\nNEW1\nOLD2\nNEW3\nz\n".to_string()),
            "only the selected rows 1 and 3 stage; the deselected middle stays at base"
        );
    }

    #[test]
    fn review_revert_hunk_applies_reversed_patch_to_workdir() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n")]);
        h.open_commit_review("/work", "c2");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewRevertHunk);

        let workdir = std::path::Path::new("/work");
        let reverted = h.fake_git.applied_workdir_patches(workdir);
        assert_eq!(
            reverted.len(),
            1,
            "revert applies one workdir patch: {reverted:?}"
        );
        let patch = &reverted[0];
        assert!(
            patch.contains("-v2\n") && patch.contains("+v1\n"),
            "revert patch must undo v2 back to v1: {patch}"
        );
        assert!(
            h.fake_git.applied_patches(workdir).is_empty(),
            "revert targets the working tree, not the index"
        );
    }

    #[test]
    fn open_review_branch_groups_chunks_by_commit() {
        use crate::review_session::ReviewSource;

        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c0", &[("a.rs", "v0\n")])
            .commit_with_parent("c1", "c0", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n")]);

        let action = stoat_action::OpenReviewBranch {
            workdir: PathBuf::from("/work"),
            base: Some("c0".to_string()),
        };
        crate::action_handlers::dispatch(&mut h.stoat, &action);
        h.settle();

        let order_commits = h.with_review(|s| {
            s.order
                .iter()
                .map(|id| s.chunks[id].commit.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            order_commits,
            vec![Some("c1".to_string()), Some("c2".to_string())],
            "each commit on top of base c0 contributes one grouped chunk",
        );
        assert!(
            matches!(
                h.with_review(|s| s.source.clone()),
                ReviewSource::Branch { .. }
            ),
            "the installed session carries the Branch source",
        );
    }

    #[test]
    fn cycle_comparison_mode_advances_source_and_preserves_approval() {
        use crate::review_session::ReviewSource;

        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        {
            let mut builder = h.fake_git.add_repo("/work").with_fs(&h.fake_fs);
            builder.modified("a.rs", "v1\n", "v2\n");
            builder.staged_file("b.rs", "staged\n");
            builder.commit("c1", &[("z.rs", "z1\n")]);
            builder.commit_with_parent("c2", "c1", &[("z.rs", "z2\n")]);
        }
        h.stoat.open_review();
        h.settle();

        let source = |h: &TestHarness| h.with_review(|s| s.source.clone());
        let approved = |h: &TestHarness, rel: &str| {
            h.with_review(|s| {
                let file = s.files.iter().find(|f| f.rel_path == rel)?;
                let id = file.chunks.first().copied()?;
                Some(s.chunk(id)?.approved)
            })
        };

        assert!(matches!(source(&h), ReviewSource::WorkingTree { .. }));

        let a_chunk = h
            .with_review(|s| {
                s.files
                    .iter()
                    .find(|f| f.rel_path == "a.rs")
                    .and_then(|f| f.chunks.first().copied())
            })
            .expect("a.rs has a chunk in the working-tree view");
        h.stoat
            .active_workspace_mut()
            .review
            .as_mut()
            .expect("review session")
            .set_approved(a_chunk, true);

        let cycle = |h: &mut TestHarness| {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::ReviewCycleComparisonMode,
            );
        };

        cycle(&mut h);
        assert!(matches!(
            source(&h),
            ReviewSource::WorkingTreeUnstaged { .. }
        ));
        assert_eq!(
            approved(&h, "a.rs"),
            Some(true),
            "approval must survive the All -> Unstaged swap (fingerprint match)",
        );

        cycle(&mut h);
        assert!(matches!(source(&h), ReviewSource::WorkingTreeStaged { .. }));

        cycle(&mut h);
        assert!(
            matches!(source(&h), ReviewSource::Commit { sha, .. } if sha == "c2"),
            "staged view advances to the HEAD commit",
        );

        cycle(&mut h);
        assert!(
            matches!(source(&h), ReviewSource::WorkingTree { .. }),
            "the commit view wraps back to the full working tree",
        );
    }

    /// (b) An external edit that shifts a previously-staged chunk
    /// to a different `base_line_range` produces a new chunk whose
    /// `ChunkIdentity` does not match the carried key, so the
    /// status defaults to `Pending` rather than carrying.
    #[test]
    fn external_edit_drops_staged_status_on_identity_mismatch() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\n", "Y\n")]);
        h.stoat.open_review();
        h.settle();
        h.set_review_status(0, ChunkStatus::Staged);

        h.external_edit("a.rs", "x\nZ\n");
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        let surviving = h.current_review_chunk_id();
        assert_eq!(
            h.chunk_status(surviving),
            ChunkStatus::Pending,
            "identity mismatch must drop the carried Staged status",
        );
    }

    /// (c) A watcher event for a path that is not in the session
    /// is a no-op: chunks, cursor, and review badges all stay put.
    #[test]
    fn external_edit_off_session_path_is_noop() {
        let head = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let buffer = "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", head, buffer)]);
        h.stoat.open_review();
        h.settle();

        let before_total = h.with_review(|s| s.progress().total);
        let before_cursor = h.current_review_chunk_id();
        let before_version = h.with_review(|s| s.version);

        h.fake_fs_watcher()
            .inject(PathBuf::from("/work/b.rs"), FsEventKind::Modified);
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert_eq!(h.with_review(|s| s.progress().total), before_total);
        assert_eq!(h.current_review_chunk_id(), before_cursor);
        assert_eq!(
            h.with_review(|s| s.version),
            before_version,
            "off-session path must not bump the session version",
        );
        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(crate::badge::BadgeSource::Review)
                .is_none(),
            "no badge for a no-op event",
        );
    }

    /// (d) Working-tree review opens register one watch token per
    /// file in the session, and `CloseReview` releases them all.
    #[test]
    fn working_tree_watch_tokens_lifecycle() {
        let head = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let buffer = "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n";
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", head, buffer)]);
        h.stoat.open_review();
        h.settle();

        assert_eq!(
            h.fake_fs_watcher().watched_paths(),
            vec![PathBuf::from("/work/a.rs")],
            "watch token registered for the session file",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseReview);

        assert!(
            h.fake_fs_watcher().watched_paths().is_empty(),
            "CloseReview must release every watch token",
        );
    }

    /// (e) `InMemory`-source sessions never start the watcher, so
    /// fake-fs writes do not flow into them.
    #[test]
    fn in_memory_session_does_not_watch() {
        use crate::host::FsHost;

        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.txt", "x\n", "Y\n")]);
        h.settle();

        assert!(
            h.fake_fs_watcher().watched_paths().is_empty(),
            "InMemory source must not register watches",
        );

        let before_version = h.with_review(|s| s.version);
        h.fake_fs()
            .write(std::path::Path::new("a.txt"), b"Z\n")
            .expect("FakeFs::write");
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert_eq!(
            h.with_review(|s| s.version),
            before_version,
            "unwatched write must not refresh the session",
        );
    }

    /// (f) Three rapid writes within the debounce window collapse
    /// into one refresh; the resulting session reflects the
    /// most-recent write only.
    #[test]
    fn external_edit_burst_dispatches_once() {
        let head = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let buffer = "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", head, buffer)]);
        h.stoat.open_review();
        h.settle();

        h.external_edit("a.rs", "A1\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        h.external_edit("a.rs", "A2\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        h.external_edit("a.rs", "A3\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        let buffer_text = h.with_review(|s| s.files[0].buffer_text.as_str().to_string());
        assert_eq!(
            buffer_text, "A3\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n",
            "post-burst session must reflect the latest write only",
        );
        assert_eq!(
            h.with_review(|s| s.order.len()),
            1,
            "single coalesced refresh produces one chunk, not three",
        );
    }

    fn assert_review_total(h: &TestHarness, expected: usize) {
        let progress = h.with_review(|s| s.progress());
        assert_eq!(
            progress.total, expected,
            "progress mismatch: {progress:?} expected total {expected}",
        );
    }

    #[test]
    fn open_review_watch_starts_empty_with_recursive_watch() {
        use crate::review_session::ReviewSource;
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        let action = stoat_action::OpenReviewWatch {
            workdir: workdir.clone(),
        };

        crate::action_handlers::dispatch(&mut h.stoat, &action);

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("watch session installed");
        assert!(
            session.files.is_empty(),
            "watch session opens empty; expected no files, got {}",
            session.files.len(),
        );
        assert_eq!(session.order.len(), 0);
        match &session.source {
            ReviewSource::WorkspaceWatch { workdir: w } => assert_eq!(w, &workdir),
            other => panic!("unexpected source: {other:?}"),
        }
        assert_eq!(
            session.watch_tokens.len(),
            1,
            "recursive watch_dir registers exactly one token",
        );
        assert_eq!(h.fake_fs_watcher().watched_paths(), vec![workdir]);
    }

    /// A watch-mode external write to a path the session has not yet
    /// seen flows through the event loop, lands as a new file entry
    /// via [`ReviewSession::upsert_file`], and parks the cursor on
    /// the first chunk.
    #[test]
    fn workspace_watch_external_create_adds_new_file() {
        use crate::host::FsHost;
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.fake_git().add_repo(workdir.clone());

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenReviewWatch {
                workdir: workdir.clone(),
            },
        );
        assert_eq!(h.with_review(|s| s.files.len()), 0);

        let new_path = workdir.join("new.rs");
        h.fake_fs()
            .write(&new_path, b"fn main() {}\n")
            .expect("FakeFs::write");
        h.fake_fs_watcher().inject(&new_path, FsEventKind::Modified);
        h.stoat.drain_fs_watch_events();
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);
        h.settle();

        let files = h.with_review(|s| s.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>());
        assert_eq!(files, vec![new_path.clone()]);
        let total = h.with_review(|s| s.order.len());
        assert!(
            total >= 1,
            "expected at least one chunk for new file, got {total}",
        );
        let cursor = h.current_review_chunk_id();
        let first = h.with_review(|s| s.files[0].chunks[0]);
        assert_eq!(cursor, first, "cursor parks on the first new chunk");
    }

    /// A watch-mode external write to a path matched by
    /// `.stoatignore` is dropped at the drain edge, never reaches
    /// the upsert path, and leaves the session unchanged.
    #[test]
    fn workspace_watch_stoatignore_path_does_not_enter_session() {
        use crate::host::FsHost;
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.fake_git().add_repo(workdir.clone());
        h.fake_fs()
            .insert_file(workdir.join(".stoatignore"), b"ignored.log\n");

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::OpenReviewWatch {
                workdir: workdir.clone(),
            },
        );
        let version_after_open = h.with_review(|s| s.version);

        let ignored = workdir.join("ignored.log");
        h.fake_fs()
            .write(&ignored, b"noise\n")
            .expect("FakeFs::write");
        h.fake_fs_watcher().inject(&ignored, FsEventKind::Modified);
        h.stoat.drain_fs_watch_events();
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);
        h.settle();

        assert!(
            h.with_review(|s| s.files.is_empty()),
            "ignored path must not enter the session",
        );
        assert_eq!(
            h.with_review(|s| s.version),
            version_after_open,
            "ignored path must not bump the session version",
        );
    }
}
