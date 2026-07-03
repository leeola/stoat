use crate::{
    app::{Stoat, UpdateEffect},
    display_map::{BlockPlacement, BlockProperties, BlockStyle, RenderBlock},
    editor_state::{EditorId, EditorState},
    host::{FsHost, GitHost, WatchToken},
    pane::View,
    review::{self, ReviewFileInput, ReviewHunk},
    review_session::{
        ChunkIdentity, ChunkStatus, ReviewProgress, ReviewSession, ReviewSource, ReviewViewState,
    },
    workspace::Workspace,
};
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use std::{
    path::Path,
    sync::{mpsc, Arc},
};
use stoat_language::LanguageRegistry;
use stoat_scheduler::Task;
use stoat_text::Point;

/// A message streamed from a running review scan.
///
/// A working-tree open streams one [`First`](Self::First) then a
/// [`File`](Self::File) per remaining file so the session installs before the
/// whole changeset is diffed, and finally a [`Complete`](Self::Complete)
/// carrying the authoritative whole-changeset session with cross-file moves.
/// A commit scan or a refresh sends only [`Complete`](Self::Complete).
enum ReviewScanMsg {
    First {
        source: ReviewSource,
        total: usize,
        file: ReviewFileInput,
        hunks: Vec<ReviewHunk>,
    },
    File {
        file: ReviewFileInput,
        hunks: Vec<ReviewHunk>,
    },
    Complete(ReviewSession),
}

/// An in-flight review scan and the work to run once each message lands.
///
/// The diff runs on a blocking thread and streams [`ReviewScanMsg`] values
/// over `rx`. [`pump_review_scan`] drains one per call, and `_task` keeps the
/// closure scheduled. `sync_path` is set only for an external-edit refresh:
/// after the final install lands the pump scrolls to that path's first change.
pub(crate) struct PendingReviewScan {
    rx: mpsc::Receiver<ReviewScanMsg>,
    _task: Task<()>,
    sync_path: Option<std::path::PathBuf>,
    /// Close the open session when the scan finds nothing, rather than leaving
    /// its now-stale diffs up. Set only by a working-tree refresh, where an
    /// empty result means the tree was fully committed. An open or a commit
    /// scan leaves it `false`.
    close_on_empty: bool,
    /// This scan armed a "scanning diff" badge on open, so its completion clears
    /// the review badge tray. A refresh or commits-mode open leaves it `false`,
    /// so a progress or apply-result badge already in the tray survives the
    /// scan.
    scanning_badge: bool,
    /// Files the scan will stream, learned from the first message. Drives the
    /// "N left" badge count.
    total: usize,
    /// Files streamed and installed so far.
    streamed: usize,
}

/// Spawn a review scan whose blocking closure streams [`ReviewScanMsg`] values,
/// and park the pending scan on `stoat`.
///
/// `produce` runs on a blocking thread. It sends messages through the given
/// sender and calls `notify_one` on the given redraw handle after each send so
/// the run loop wakes to pump them.
fn spawn_review_scan(
    stoat: &mut Stoat,
    sync_path: Option<std::path::PathBuf>,
    close_on_empty: bool,
    scanning_badge: bool,
    produce: impl FnOnce(&mpsc::Sender<ReviewScanMsg>, &Arc<tokio::sync::Notify>) + Send + 'static,
) {
    let (tx, rx) = mpsc::channel();
    let redraw = stoat.redraw_notify.clone();
    let task = stoat.executor.spawn_blocking(move || produce(&tx, &redraw));
    stoat.pending_review_scan = Some(PendingReviewScan {
        rx,
        _task: task,
        sync_path,
        close_on_empty,
        scanning_badge,
        total: 0,
        streamed: 0,
    });
}

/// Stream a working-tree scan's gathered inputs: one [`ReviewScanMsg::First`]
/// then a [`ReviewScanMsg::File`] per remaining file, each diffed on its own,
/// then a [`ReviewScanMsg::Complete`] carrying the whole-changeset session so
/// cross-file moves are restored. Sends nothing for an empty input set.
fn stream_review_inputs(
    tx: &mpsc::Sender<ReviewScanMsg>,
    redraw: &Arc<tokio::sync::Notify>,
    source: ReviewSource,
    inputs: Vec<ReviewFileInput>,
) {
    let total = inputs.len();
    if total == 0 {
        return;
    }

    for (index, input) in inputs.iter().enumerate() {
        let hunks = review::extract_review_hunks_single(input, 3);
        let msg = if index == 0 {
            ReviewScanMsg::First {
                source: source.clone(),
                total,
                file: input.clone(),
                hunks,
            }
        } else {
            ReviewScanMsg::File {
                file: input.clone(),
                hunks,
            }
        };
        if tx.send(msg).is_err() {
            return;
        }
        redraw.notify_one();
    }

    let mut session = ReviewSession::new(source);
    session.add_files(inputs);
    let _ = tx.send(ReviewScanMsg::Complete(session));
    redraw.notify_one();
}

pub(super) fn open_review_commit(stoat: &mut Stoat, workdir: &Path, sha: &str) {
    emit_review_info_badge(stoat, "scanning diff");
    let git_host = stoat.git_host.clone();
    let langs = stoat.language_registry.clone();
    let workdir = workdir.to_path_buf();
    let sha = sha.to_string();

    spawn_review_scan(stoat, None, false, true, move |tx, redraw| {
        if let Some(session) = scan_commit_pure(&*git_host, &langs, &workdir, &sha) {
            let _ = tx.send(ReviewScanMsg::Complete(session));
            redraw.notify_one();
        }
    });
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
            if let Some(chunk) = session.chunks.get(id)
                && chunk.status == ChunkStatus::Staged
            {
                groups.entry(chunk.file_index).or_default().push(chunk);
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
    match scan_commit(stoat, workdir, sha) {
        Some(mut session) => {
            session.origin = origin;
            install_review_session(stoat, session);
        },
        _ => {
            // Rewritten commit has no diffs vs. parent. Drop the review;
            // `close_review` routes back to commits mode if that's where the
            // user launched from.
            close_review(stoat);
        },
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
    let git_host = stoat.git_host.clone();
    let langs = stoat.language_registry.clone();

    spawn_review_scan(stoat, None, false, false, move |tx, redraw| {
        if let Some(mut session) = scan_commit_pure(&*git_host, &langs, &workdir, &sha) {
            session.origin = ReviewOrigin::FromCommits;
            let _ = tx.send(ReviewScanMsg::Complete(session));
            redraw.notify_one();
        }
    });
    UpdateEffect::Redraw
}

pub(super) fn open_review_commit_range(stoat: &mut Stoat, workdir: &Path, from: &str, to: &str) {
    emit_review_info_badge(stoat, "scanning diff");
    let git_host = stoat.git_host.clone();
    let langs = stoat.language_registry.clone();
    let workdir = workdir.to_path_buf();
    let from = from.to_string();
    let to = to.to_string();

    spawn_review_scan(stoat, None, false, true, move |tx, redraw| {
        if let Some(session) = scan_commit_range_pure(&*git_host, &langs, &workdir, &from, &to) {
            let _ = tx.send(ReviewScanMsg::Complete(session));
            redraw.notify_one();
        }
    });
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
}

#[derive(Copy, Clone, Debug)]
pub(super) enum ReviewMark {
    Stage,
    Unstage,
    Toggle,
    Skip,
}

pub(super) fn review_step(stoat: &mut Stoat, step: ReviewStep) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_mut() else {
        return UpdateEffect::None;
    };
    let moved = match step {
        ReviewStep::Next => session.next(),
        ReviewStep::Prev => session.prev(),
    };
    if moved.is_none() {
        return UpdateEffect::None;
    }
    let chunk_id = session.cursor.current;
    let editor_id = session.view_editor;
    sync_review_view_and_scroll(ws, editor_id, chunk_id);
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
    match mark {
        ReviewMark::Stage => session.set_status(id, ChunkStatus::Staged),
        ReviewMark::Unstage => session.set_status(id, ChunkStatus::Unstaged),
        ReviewMark::Toggle => session.toggle_stage(id),
        ReviewMark::Skip => session.set_status(id, ChunkStatus::Skipped),
    }
    let editor_id = session.view_editor;
    let progress = session.progress();
    sync_review_view_and_scroll(ws, editor_id, None);
    emit_review_progress_badge(ws, &progress);

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

/// Refresh the editor's review view cache from the session and, when a chunk
/// is supplied and the editor is not mid-scroll, scroll so that chunk sits near
/// the top of the pane. A refresh that lands during an in-flight glide leaves
/// the scroll position to the animation rather than fighting it.
///
/// Split borrow of `ws.editors` and `ws.review` is done here so callers can
/// drop their `&mut ws.review` borrow before invoking.
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
    // A refresh that lands mid-motion must not yank scroll_row out from under
    // the animation. During a wheel coast the next momentum tick would
    // overwrite it from the eased offset, and during a page glide it is the
    // fixed target the offset eases toward, so re-targeting it here jerks the
    // view to the chunk and back. Re-scroll only after the motion settles. The
    // view-state refresh above still runs regardless.
    let mid_motion = editor.scroll_velocity != 0.0 || editor.scroll_glide;
    if let Some(chunk_id) = scroll_to_chunk
        && !mid_motion
        && let Some(buffer_row) = view.row_of_chunk(chunk_id)
    {
        // row_of_chunk is a buffer row, but scroll_row is a display row and the
        // review display inserts a header block above each chunk. Convert
        // through the display map so the chunk lands at the same offset
        // regardless of how many headers precede it.
        let display_row = editor
            .display_map
            .snapshot()
            .buffer_to_display(Point::new(buffer_row, 0))
            .row;
        editor.scroll_row = display_row.saturating_sub(3);
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
            ReviewSource::WorkingTree { workdir } => workdir.clone(),
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
                Some((*id, chunk_to_unified_diff(file, c, &workdir)))
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
        return review_refresh(stoat, None);
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
    let in_session = stoat
        .active_workspace()
        .review
        .as_ref()
        .is_some_and(|s| s.files.iter().any(|f| f.path == path));
    if !in_session {
        return UpdateEffect::None;
    }

    review_refresh(stoat, Some(path.to_path_buf()))
}

/// Re-scan the current review's source and reinstall it, carrying decided
/// chunk statuses across by identity.
///
/// Git-backed sources run the diff on a blocking thread off the input loop.
/// The carried statuses are reapplied inside the scan closure and the new
/// session is installed by [`pump_review_scan`]. `sync_path`, set only by
/// [`review_external_edit`], scrolls to the edited chunk and refreshes the
/// badge once the install lands. In-memory sources have no git2 to offload,
/// so they re-scan and install inline.
pub(super) fn review_refresh(
    stoat: &mut Stoat,
    sync_path: Option<std::path::PathBuf>,
) -> UpdateEffect {
    let source = {
        let ws = stoat.active_workspace();
        let Some(old) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        old.source.clone()
    };

    if is_git_source(&source) {
        // A committed-away working tree closes the session on refresh. A commit
        // diff is a fixed snapshot, so it never closes on an empty rescan.
        let close_on_empty = matches!(source, ReviewSource::WorkingTree { .. });
        let git_host = stoat.git_host.clone();
        let fs_host = stoat.fs_host.clone();
        let langs = stoat.language_registry.clone();

        // A refresh sends only Complete, never streaming First/File increments:
        // streaming would replace the live session with fresh Pending chunks and
        // flicker the user's decisions away. Leaving the old session up until
        // Complete lets the pump reapply carried statuses from it.
        spawn_review_scan(
            stoat,
            sync_path,
            close_on_empty,
            false,
            move |tx, redraw| {
                if let Some(session) =
                    rescan_git_source_pure(&*git_host, &*fs_host, &langs, &source)
                {
                    let _ = tx.send(ReviewScanMsg::Complete(session));
                    redraw.notify_one();
                }
            },
        );
        return UpdateEffect::Redraw;
    }

    // Non-git sources (agent edits, in-memory) rescan synchronously off no git
    // IO, so they install directly rather than through the scan channel.
    let carried = {
        let ws = stoat.active_workspace();
        let old = ws
            .review
            .as_ref()
            .expect("review session still present (early-returned above when absent)");
        carried_statuses(old)
    };
    let Some(mut new_session) = rescan_source(stoat, &source) else {
        return UpdateEffect::None;
    };
    apply_carried_status(&mut new_session, &carried);
    install_review_session(stoat, new_session);
    if let Some(path) = sync_path {
        post_refresh_sync(stoat, &path);
    }
    UpdateEffect::Redraw
}

/// Decided-chunk statuses keyed by [`ChunkIdentity`], snapshotted before a
/// refresh so a re-scan that reproduces a matching chunk keeps the user's
/// staged/rejected decision instead of resetting it to pending.
fn carried_statuses(
    session: &ReviewSession,
) -> std::collections::HashMap<ChunkIdentity, ChunkStatus> {
    session
        .order
        .iter()
        .filter_map(|id| {
            let status = session.chunks.get(id)?.status;
            if !status.is_decided() {
                return None;
            }
            let ident = session.identity_key(*id)?;
            Some((ident, status))
        })
        .collect()
}

/// Reapply carried decisions to a freshly scanned session by chunk identity.
fn apply_carried_status(
    session: &mut ReviewSession,
    carried: &std::collections::HashMap<ChunkIdentity, ChunkStatus>,
) {
    let ids: Vec<_> = session.order.clone();
    for id in ids {
        if let Some(ident) = session.identity_key(id)
            && let Some(status) = carried.get(&ident).copied()
        {
            session.set_status(id, status);
        }
    }
}

/// True for review sources whose re-scan reads the git repository, and so
/// must run off the input loop.
fn is_git_source(source: &ReviewSource) -> bool {
    matches!(
        source,
        ReviewSource::WorkingTree { .. }
            | ReviewSource::Commit { .. }
            | ReviewSource::CommitRange { .. }
    )
}

/// `&Stoat`-free re-scan dispatcher for the git-backed sources, runnable on
/// a blocking thread. Returns `None` for non-git sources, which
/// [`review_refresh`] handles synchronously.
fn rescan_git_source_pure(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    source: &ReviewSource,
) -> Option<ReviewSession> {
    match source {
        ReviewSource::WorkingTree { workdir } => scan_working_tree_pure(git, fs, langs, workdir),
        ReviewSource::Commit { workdir, sha } => scan_commit_pure(git, langs, workdir, sha),
        ReviewSource::CommitRange { workdir, from, to } => {
            scan_commit_range_pure(git, langs, workdir, from, to)
        },
        ReviewSource::AgentEdits { .. } | ReviewSource::InMemory { .. } => None,
    }
}

/// Scroll the review to the chunk containing `path`'s first change and
/// refresh the progress badge, the post-install work for an external-edit
/// refresh.
fn post_refresh_sync(stoat: &mut Stoat, path: &Path) {
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_ref() else {
        return;
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
}

/// Re-scan the underlying source of a review session. Returns `None` when
/// the source has no re-scannable state (currently `InMemory`) or when the
/// scan produced no hunks.
fn rescan_source(stoat: &Stoat, source: &ReviewSource) -> Option<ReviewSession> {
    match source {
        ReviewSource::WorkingTree { workdir } => scan_working_tree(stoat, workdir),
        ReviewSource::Commit { workdir, sha } => scan_commit(stoat, workdir, sha),
        ReviewSource::CommitRange { workdir, from, to } => {
            scan_commit_range(stoat, workdir, from, to)
        },
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
    emit_review_info_badge(stoat, "scanning diff");
    let git_root = stoat.active_workspace().git_root.clone();
    let git_host = stoat.git_host.clone();
    let fs_host = stoat.fs_host.clone();
    let langs = stoat.language_registry.clone();

    spawn_review_scan(stoat, None, false, true, move |tx, redraw| {
        let Some((workdir, inputs)) =
            crate::diff::scan_working_tree(&*git_host, &*fs_host, &langs, &git_root)
        else {
            return;
        };
        stream_review_inputs(tx, redraw, ReviewSource::WorkingTree { workdir }, inputs);
    });
}

/// Drain one message from the in-flight review scan, advancing the streamed
/// install.
///
/// A working-tree open streams [`ReviewScanMsg::First`] (create and install the
/// session with the first file), then a [`ReviewScanMsg::File`] per remaining
/// file (append and re-render), then [`ReviewScanMsg::Complete`] carrying the
/// whole-changeset session that restores cross-file moves. A commit scan or a
/// refresh sends only [`ReviewScanMsg::Complete`].
///
/// One message is drained per call, re-notifying the redraw channel so the run
/// loop pumps the next. The blocking closure queues every message up front on
/// the test scheduler, so draining one at a time is what makes the install
/// appear file by file.
///
/// A channel that closes before any file streamed means the source had no
/// changes. A working-tree refresh ([`PendingReviewScan::close_on_empty`]) with
/// a session still open closes it with a "working tree clean" badge, otherwise a
/// "no changes to review" badge keeps an empty scan from being silent.
///
/// Returns whether a message was drained this call, mirroring the other
/// render-time pumps. Driven from [`Stoat::render`] and the test harness
/// `settle` loop.
pub(crate) fn pump_review_scan(stoat: &mut Stoat) -> bool {
    let Some(mut pending) = stoat.pending_review_scan.take() else {
        return false;
    };

    match pending.rx.try_recv() {
        Ok(ReviewScanMsg::First {
            source,
            total,
            file,
            hunks,
        }) => {
            let mut session = ReviewSession::new(source);
            session.add_file_streamed(file, hunks);
            install_review_session(stoat, session);

            pending.total = total;
            pending.streamed = 1;
            emit_scanning_progress_badge(stoat, &pending);

            stoat.redraw_notify.notify_one();
            stoat.pending_review_scan = Some(pending);
            true
        },
        Ok(ReviewScanMsg::File { file, hunks }) => {
            append_streamed_file(stoat, file, hunks);

            pending.streamed += 1;
            emit_scanning_progress_badge(stoat, &pending);

            stoat.redraw_notify.notify_one();
            stoat.pending_review_scan = Some(pending);
            true
        },
        Ok(ReviewScanMsg::Complete(mut session)) => {
            // Reapply the current session's decisions to the authoritative
            // whole-changeset session. This is empty for a fresh open, whose
            // streamed chunks are all pending, and carries the user's
            // staged/rejected chunks for a refresh, where the old session is
            // still installed here.
            let carried = stoat
                .active_workspace()
                .review
                .as_ref()
                .map(carried_statuses)
                .unwrap_or_default();
            apply_carried_status(&mut session, &carried);
            install_review_session(stoat, session);

            if pending.scanning_badge {
                use crate::badge::BadgeSource;
                stoat
                    .active_workspace_mut()
                    .badges
                    .remove_by_source(BadgeSource::Review);
            }
            if let Some(path) = pending.sync_path {
                post_refresh_sync(stoat, &path);
            }
            true
        },
        Err(mpsc::TryRecvError::Empty) => {
            stoat.pending_review_scan = Some(pending);
            false
        },
        Err(mpsc::TryRecvError::Disconnected) => {
            if pending.streamed == 0 {
                if pending.close_on_empty && stoat.active_workspace().review.is_some() {
                    let _ = close_review(stoat);
                    emit_review_info_badge(stoat, "working tree clean");
                } else {
                    emit_review_info_badge(stoat, "no changes to review");
                }
            }
            true
        },
    }
}

/// Update the "scanning diff (N left)" badge for a streaming open, or leave the
/// tray untouched for a scan that armed no scanning badge.
fn emit_scanning_progress_badge(stoat: &mut Stoat, pending: &PendingReviewScan) {
    if !pending.scanning_badge {
        return;
    }
    let remaining = pending.total.saturating_sub(pending.streamed);
    let label = if remaining == 0 {
        "scanning diff".to_string()
    } else {
        format!("scanning diff ({remaining} left)")
    };
    emit_review_info_badge(stoat, &label);
}

/// Append one streamed file to the installed session and rebuild the review
/// editor from it. Watchers are set once for the first file and refreshed
/// wholesale by the [`ReviewScanMsg::Complete`] install, so appends only
/// re-render.
fn append_streamed_file(stoat: &mut Stoat, file: ReviewFileInput, hunks: Vec<ReviewHunk>) {
    if let Some(session) = stoat.active_workspace_mut().review.as_mut() {
        session.add_file_streamed(file, hunks);
    }
    render_review_editor(stoat);
}

/// Scan the git working tree rooted at `git_root` into a review session.
/// Returns `None` when the root is not a repository or has no diff hunks.
///
/// Takes no `&Stoat` so the git2 diff can run on a blocking thread off the
/// input loop. [`open_review`] drives it through `spawn_blocking`;
/// [`scan_working_tree`] wraps it for the synchronous [`review_refresh`].
fn scan_working_tree_pure(
    git: &dyn GitHost,
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_root: &Path,
) -> Option<ReviewSession> {
    let Some((workdir, inputs)) = crate::diff::scan_working_tree(git, fs, langs, git_root) else {
        tracing::warn!("open_review: no working-tree changes to review");
        return None;
    };

    let mut session = ReviewSession::new(ReviewSource::WorkingTree { workdir });
    session.add_files(inputs);

    if session.order.is_empty() {
        tracing::warn!("open_review: no diff hunks to display");
        return None;
    }

    Some(session)
}

/// Synchronous [`scan_working_tree_pure`] wrapper reading the hosts off
/// `stoat`, used by [`review_refresh`] to re-scan in place.
fn scan_working_tree(stoat: &Stoat, git_root: &Path) -> Option<ReviewSession> {
    scan_working_tree_pure(
        &*stoat.git_host,
        &*stoat.fs_host,
        &stoat.language_registry,
        git_root,
    )
}

/// Build a session from a single commit by diffing its tree against its
/// first parent (or the empty tree for a root commit). Returns `None` when
/// the repo or sha is unknown, or when no paths differ.
///
/// Takes no `&Stoat` so the git2 reads run on a blocking thread off the
/// input loop. [`open_review_commit`] and [`commits_open_review`] drive it
/// through `spawn_blocking`; [`scan_commit`] wraps it for synchronous
/// callers.
fn scan_commit_pure(
    git: &dyn GitHost,
    langs: &LanguageRegistry,
    workdir: &Path,
    sha: &str,
) -> Option<ReviewSession> {
    let repo = git.discover(workdir)?;
    let workdir = repo.workdir()?;
    let new_tree = repo.commit_tree(sha)?;
    let base_tree = match repo.parent_sha(sha) {
        Some(parent) => repo.commit_tree(&parent).unwrap_or_default(),
        None => std::collections::BTreeMap::new(),
    };
    build_session_from_trees(
        langs,
        ReviewSource::Commit {
            workdir: workdir.clone(),
            sha: sha.to_string(),
        },
        &workdir,
        &base_tree,
        &new_tree,
    )
}

/// Synchronous [`scan_commit_pure`] wrapper reading the hosts off `stoat`,
/// used by the reopen, removal, refresh, and rebase paths that scan inline.
pub(super) fn scan_commit(stoat: &Stoat, workdir: &Path, sha: &str) -> Option<ReviewSession> {
    scan_commit_pure(&*stoat.git_host, &stoat.language_registry, workdir, sha)
}

/// Build a session from a commit range `from..=to` (inclusive of `to`,
/// exclusive of `from`, the same as `git diff from..to`). Pairs each path
/// in either tree to form file-level base/buffer contents.
///
/// `&Stoat`-free for the same off-loop reason as [`scan_commit_pure`].
fn scan_commit_range_pure(
    git: &dyn GitHost,
    langs: &LanguageRegistry,
    workdir: &Path,
    from: &str,
    to: &str,
) -> Option<ReviewSession> {
    let repo = git.discover(workdir)?;
    let workdir = repo.workdir()?;
    let base_tree = repo.commit_tree(from).unwrap_or_default();
    let new_tree = repo.commit_tree(to)?;
    build_session_from_trees(
        langs,
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

/// Synchronous [`scan_commit_range_pure`] wrapper used by [`rescan_source`].
fn scan_commit_range(stoat: &Stoat, workdir: &Path, from: &str, to: &str) -> Option<ReviewSession> {
    scan_commit_range_pure(
        &*stoat.git_host,
        &stoat.language_registry,
        workdir,
        from,
        to,
    )
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

/// Common builder used by the commit scans. Walks the union of paths
/// across `base_tree` and `new_tree`, skipping any pair whose base and
/// buffer contents are equal.
///
/// Takes the language registry directly rather than `&Stoat` so it runs
/// inside the off-loop scan closures, including the commit-preview build in
/// the sibling `commits` module.
pub(super) fn build_session_from_trees(
    langs: &LanguageRegistry,
    source: ReviewSource,
    workdir: &Path,
    base_tree: &std::collections::BTreeMap<std::path::PathBuf, String>,
    new_tree: &std::collections::BTreeMap<std::path::PathBuf, String>,
) -> Option<ReviewSession> {
    let mut paths: std::collections::BTreeSet<&Path> = std::collections::BTreeSet::new();
    for p in base_tree.keys() {
        paths.insert(p.as_path());
    }
    for p in new_tree.keys() {
        paths.insert(p.as_path());
    }
    if paths.is_empty() {
        return None;
    }
    let mut session = ReviewSession::new(source);
    let mut inputs: Vec<ReviewFileInput> = Vec::new();
    for rel in paths {
        let base = base_tree.get(rel).cloned().unwrap_or_default();
        let buffer = new_tree.get(rel).cloned().unwrap_or_default();
        if base == buffer {
            continue;
        }
        let abs = workdir.join(rel);
        let lang = langs.for_path(&abs);
        inputs.push(ReviewFileInput {
            path: abs,
            rel_path: rel.display().to_string(),
            language: lang,
            base_text: Arc::new(base),
            buffer_text: Arc::new(buffer),
        });
    }
    session.add_files(inputs);
    if session.order.is_empty() {
        return None;
    }
    Some(session)
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
    }

    stoat.active_workspace_mut().review = Some(session);
    render_review_editor(stoat);
}

/// Rebuild the review editor from the currently installed session.
///
/// Builds a flattened [`ReviewViewState`] and chunk-header blocks, spawns a
/// placeholder buffer + editor to host them, and swaps it into the focused
/// pane, dropping the pane's previous editor. A streamed append and a whole-
/// session install both land here, so the on-screen review always matches
/// `ws.review`.
fn render_review_editor(stoat: &mut Stoat) {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_ref() else {
        return;
    };

    let view = ReviewViewState::from_session(session);
    let blocks = build_review_blocks(session, &view);
    let row_count = view.rows.len();
    let placeholder = " \n".repeat(row_count.saturating_sub(1)) + " ";

    let (buffer_id, buffer) = ws.buffers.new_scratch();
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.edit(0..0, &placeholder);
        guard.mark_clean();
    }

    let mut editor = EditorState::new(buffer_id, buffer, executor);
    editor.display_map.insert_blocks(blocks);
    editor.review_view = Some(view);

    let new_editor_id = ws.editors.insert(editor);
    if let Some(session) = ws.review.as_mut() {
        session.view_editor = Some(new_editor_id);
    }

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

    #[test]
    fn staging_a_chunk_bumps_the_review_content_version() {
        use crate::review_session::ReviewViewState;

        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[("a.rs", "fn a() { 1 }\n", "fn a() { 2 }\n")]);

        // The pool content version is the session version the review view carries.
        // A status-only edit must bump it even though the row count is unchanged,
        // which the old rows.len() version missed, so pooled pages never refreshed
        // their gutter glyphs on stage.
        let read = |h: &TestHarness| {
            h.with_review(|s| {
                let v = ReviewViewState::from_session(s);
                (v.rows.len(), v.session_version)
            })
        };
        let (rows_before, version_before) = read(&h);
        h.set_review_status(0, ChunkStatus::Staged);
        let (rows_after, version_after) = read(&h);

        assert_eq!(
            rows_after, rows_before,
            "staging leaves the row count unchanged"
        );
        assert!(
            version_after > version_before,
            "staging bumps the content version {version_before} -> {version_after}"
        );
    }

    /// Navigating to a chunk positions the view by the chunk's display row, so
    /// preceding chunk-header blocks do not push it down the pane. Three chunks
    /// (one per file) mean three header blocks; the third chunk's display row
    /// exceeds its buffer row, and scroll_row must track the display row.
    #[test]
    fn chunk_navigation_accounts_for_header_blocks() {
        use super::{review_step, ReviewStep};
        use stoat_text::Point;

        let mut h = TestHarness::with_size(80, 24);
        h.open_review_from_texts(&[
            ("a.rs", "fn a() { 1 }\n", "fn a() { 2 }\n"),
            ("b.rs", "fn b() { 1 }\n", "fn b() { 2 }\n"),
            ("c.rs", "fn c() { 1 }\n", "fn c() { 2 }\n"),
        ]);

        let editor_id = h.with_review(|s| s.view_editor).expect("review editor id");

        review_step(&mut h.stoat, ReviewStep::Next);
        review_step(&mut h.stoat, ReviewStep::Next);

        let current = h.current_review_chunk_id();
        let scroll_row = h.editor_scroll_row(editor_id);

        let (buffer_row, display_row) = {
            let ws = h.stoat.active_workspace_mut();
            let editor = ws.editors.get_mut(editor_id).expect("editor");
            let buffer_row = editor
                .review_view
                .as_ref()
                .expect("review view")
                .row_of_chunk(current)
                .expect("row of current chunk");
            let display_row = editor
                .display_map
                .snapshot()
                .buffer_to_display(Point::new(buffer_row, 0))
                .row;
            (buffer_row, display_row)
        };

        assert!(
            display_row > buffer_row,
            "the third chunk sits below preceding header blocks",
        );
        assert_eq!(
            scroll_row,
            display_row.saturating_sub(3),
            "scroll_row is the chunk's display row minus the 3-row margin",
        );
        assert_ne!(
            scroll_row,
            buffer_row.saturating_sub(3),
            "the old buffer-row computation would land the chunk lower",
        );
    }

    /// A follow-refresh re-centers the review on the current chunk, but must not
    /// do so while a scroll is in flight. Writing scroll_row mid-glide yanks the
    /// animation's target to the chunk and back, so only a refresh at rest
    /// scrolls.
    #[test]
    fn follow_refresh_leaves_scroll_row_alone_mid_glide() {
        use super::{review_step, sync_review_view_and_scroll, ReviewStep};

        let mut h = TestHarness::with_size(80, 24);
        h.open_review_from_texts(&[
            ("a.rs", "fn a() { 1 }\n", "fn a() { 2 }\n"),
            ("b.rs", "fn b() { 1 }\n", "fn b() { 2 }\n"),
            ("c.rs", "fn c() { 1 }\n", "fn c() { 2 }\n"),
        ]);
        let editor_id = h.with_review(|s| s.view_editor).expect("review editor id");

        review_step(&mut h.stoat, ReviewStep::Next);
        review_step(&mut h.stoat, ReviewStep::Next);
        let chunk = h.current_review_chunk_id();
        let target = h.editor_scroll_row(editor_id);
        assert!(
            target > 0,
            "the third chunk settles below the top of the pane"
        );

        {
            let ws = h.stoat.active_workspace_mut();
            {
                let editor = ws.editors.get_mut(editor_id).expect("editor");
                editor.scroll_row = 0;
                editor.scroll_velocity = 50.0;
            }
            sync_review_view_and_scroll(ws, Some(editor_id), Some(chunk));
        }
        assert_eq!(
            h.editor_scroll_row(editor_id),
            0,
            "a mid-glide refresh does not yank scroll_row to the chunk",
        );

        {
            let ws = h.stoat.active_workspace_mut();
            {
                let editor = ws.editors.get_mut(editor_id).expect("editor");
                editor.scroll_velocity = 0.0;
            }
            sync_review_view_and_scroll(ws, Some(editor_id), Some(chunk));
        }
        assert_eq!(
            h.editor_scroll_row(editor_id),
            target,
            "a refresh at rest scrolls to the current chunk",
        );
    }

    #[test]
    fn opening_a_diff_shows_a_scanning_badge_until_the_scan_settles() {
        use crate::badge::{BadgeSource, BadgeState};

        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\n", "Y\n")]);
        h.stoat.open_review();

        {
            let ws = h.stoat.active_workspace();
            let id = ws
                .badges
                .find_by_source(BadgeSource::Review)
                .expect("a scanning badge is shown before the scan settles");
            let badge = ws.badges.get(id).expect("badge");
            assert_eq!(badge.label, "scanning diff");
            assert_eq!(badge.state, BadgeState::Active);
        }
        assert!(
            h.stoat.active_workspace().review.is_none(),
            "the session is not installed until the scan settles",
        );

        h.settle();

        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(BadgeSource::Review)
                .is_none(),
            "the scanning badge clears once the session installs",
        );
        assert!(
            h.stoat.active_workspace().review.is_some(),
            "the session installs after the scan settles",
        );
    }

    #[test]
    fn a_refresh_scan_keeps_a_review_badge_it_did_not_arm() {
        use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};

        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\n", "Y\n")]);
        h.stoat.open_review();
        h.settle();

        // A badge left by a stage or apply action, not armed by the scan.
        h.stoat.active_workspace_mut().badges.insert(Badge {
            source: BadgeSource::Review,
            anchor: Anchor::BottomRight,
            state: BadgeState::Complete,
            label: "applied 1 chunk".to_string(),
            detail: None,
        });

        h.dispatch_review_refresh();

        let ws = h.stoat.active_workspace();
        let id = ws
            .badges
            .find_by_source(BadgeSource::Review)
            .expect("a badge the scan did not arm survives the refresh");
        assert_eq!(ws.badges.get(id).expect("badge").label, "applied 1 chunk");
    }

    #[test]
    fn open_streams_the_session_one_file_at_a_time() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[
                ("a.rs", "fn a() { 1 }\n", "fn a() { 2 }\n"),
                ("b.rs", "fn b() { 1 }\n", "fn b() { 2 }\n"),
                ("c.rs", "fn c() { 1 }\n", "fn c() { 2 }\n"),
            ],
        );
        h.stoat.open_review();
        assert!(
            h.stoat.active_workspace().review.is_none(),
            "nothing installs until the first file is pumped",
        );

        super::pump_review_scan(&mut h.stoat);
        assert_eq!(
            h.with_review(|s| s.files.len()),
            1,
            "the first file installs before the rest are diffed",
        );
        let cursor = h.current_review_chunk_id();

        super::pump_review_scan(&mut h.stoat);
        assert_eq!(
            h.with_review(|s| s.files.len()),
            2,
            "the second file appends"
        );
        assert_eq!(
            h.current_review_chunk_id(),
            cursor,
            "appending a file does not reset the cursor",
        );

        super::pump_review_scan(&mut h.stoat);
        assert_eq!(
            h.with_review(|s| s.files.len()),
            3,
            "the third file appends"
        );
        assert_eq!(
            h.current_review_chunk_id(),
            cursor,
            "the cursor still holds"
        );
    }

    #[test]
    fn the_complete_pass_restores_a_cross_file_move() {
        use crate::review::{ReviewRow, ReviewSide};

        fn has_cross_file_move(session: &crate::review_session::ReviewSession) -> bool {
            session.chunks.values().any(|chunk| {
                chunk.hunk.rows.iter().any(|row| {
                    let sides: [Option<&ReviewSide>; 2] = match row {
                        ReviewRow::Context { left, right } => [Some(left), Some(right)],
                        ReviewRow::Changed { left, right } => [left.as_ref(), right.as_ref()],
                    };
                    sides.iter().flatten().any(|s| s.move_provenance.is_some())
                })
            })
        }

        // `migrated` leaves a.rs and reappears in b.rs -- a relocation only the
        // whole-changeset pass detects, not the per-file diffs.
        let a_base =
            "fn migrated() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n}\n\nfn stays_a() {\n    call_a();\n}\n";
        let a_rhs = "fn stays_a() {\n    call_a();\n}\n";
        let b_base = "fn stays_b() {\n    call_b();\n}\n";
        let b_rhs =
            "fn stays_b() {\n    call_b();\n}\n\nfn migrated() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n}\n";

        let mut h = TestHarness::with_size(140, 32);
        h.stage_review_scenario("/work", &[("a.rs", a_base, a_rhs), ("b.rs", b_base, b_rhs)]);
        h.stoat.open_review();

        super::pump_review_scan(&mut h.stoat);
        super::pump_review_scan(&mut h.stoat);
        assert!(
            !h.with_review(has_cross_file_move),
            "per-file streaming surfaces no cross-file move",
        );

        h.settle();
        assert!(
            h.with_review(has_cross_file_move),
            "the whole-changeset Complete pass restores the cross-file move",
        );
    }

    #[test]
    fn open_on_a_clean_tree_shows_no_changes_to_review() {
        use crate::badge::BadgeSource;

        let mut h = TestHarness::with_size(80, 14);
        h.fake_git.add_repo("/work");
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.stoat.open_review();
        h.settle();

        assert!(
            h.stoat.active_workspace().review.is_none(),
            "a clean tree installs no session",
        );
        let ws = h.stoat.active_workspace();
        let id = ws
            .badges
            .find_by_source(BadgeSource::Review)
            .expect("an empty scan is not silent");
        assert_eq!(
            ws.badges.get(id).expect("badge").label,
            "no changes to review"
        );
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

    /// A `.git` write (e.g. a commit) refreshes an open working-tree
    /// review through the shared debounce. Here the tree is committed
    /// clean, so the refresh finds no hunks and closes the stale session.
    #[test]
    fn git_state_change_refreshes_working_tree_review() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\n", "Y\n")]);
        h.stoat.open_review();
        h.settle();
        assert!(h.stoat.active_workspace().review.is_some());

        h.fake_git.add_repo("/work").clear_changes();
        h.fake_fs_watcher().inject(
            PathBuf::from("/work/.git/refs/heads/main"),
            FsEventKind::Modified,
        );
        h.stoat.drain_fs_watch_events();
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert!(
            h.stoat.active_workspace().review.is_none(),
            "a .git write refreshes the review, which closes on the now-clean tree",
        );
    }

    /// A commit-source review is a fixed snapshot, so a `.git` write must
    /// not refresh it.
    #[test]
    fn git_state_change_leaves_commit_review_untouched() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .commit("c1", &[("a.rs", "v1\n")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2\n")]);
        h.open_commit_review("/work", "c2");
        let before = h.with_review(|s| s.version);

        h.fake_fs_watcher().inject(
            PathBuf::from("/work/.git/refs/heads/main"),
            FsEventKind::Modified,
        );
        h.stoat.drain_fs_watch_events();
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert!(
            h.stoat.active_workspace().review.is_some(),
            "the commit review stays open",
        );
        assert_eq!(
            h.with_review(|s| s.version),
            before,
            "a commit-source review does not refresh on a git-state change",
        );
    }

    /// A change to a working-tree file not yet in the session pulls it
    /// into the review on the next refresh.
    #[test]
    fn non_session_change_pulls_the_file_into_the_review() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\n", "Y\n")]);
        h.stoat.open_review();
        h.settle();
        assert_eq!(h.with_review(|s| s.files.len()), 1);

        h.stage_review_scenario("/work", &[("b.rs", "p\n", "Q\n")]);
        h.fake_fs_watcher()
            .inject(PathBuf::from("/work/b.rs"), FsEventKind::Modified);
        h.stoat.drain_fs_watch_events();
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert!(
            h.with_review(|s| s.files.iter().any(|f| f.rel_path == "b.rs")),
            "the newly-changed file is pulled into the session",
        );
    }

    /// A gitignored path (build churn) arms no refresh even under the
    /// git root.
    #[test]
    fn gitignored_change_arms_no_refresh() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\n", "Y\n")]);
        h.stoat.open_review();
        h.settle();
        h.fake_git.add_repo("/work").ignored("target/out.o");
        let before = h.with_review(|s| s.version);

        h.fake_fs_watcher()
            .inject(PathBuf::from("/work/target/out.o"), FsEventKind::Modified);
        h.stoat.drain_fs_watch_events();
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert_eq!(
            h.with_review(|s| s.version),
            before,
            "a gitignored change must not refresh the review",
        );
    }

    /// With `review.follow` off, an external edit to a session file does
    /// not auto-refresh. A manual `r` is still required.
    #[test]
    fn review_follow_off_suppresses_auto_refresh() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/work", &[("a.rs", "x\n", "Y\n")]);
        h.stoat.open_review();
        h.settle();
        h.stoat.settings.review_follow = Some(false);
        let before = h.with_review(|s| s.version);

        h.external_edit("a.rs", "x\nZ\n");
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert_eq!(
            h.with_review(|s| s.version),
            before,
            "review.follow off must suppress the automatic refresh",
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
}
