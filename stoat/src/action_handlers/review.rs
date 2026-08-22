use super::{amend::AmendRoute, movement::ChangeDir};
use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    diff_cache::{DiffCache, DiffCacheKey},
    display_map::{
        syntax_theme::SyntaxStyles, BlockPlacement, BlockProperties, BlockStyle, RenderBlock,
    },
    editor_state::{EditorId, EditorState, ScrollGlide},
    host::{GitHost, GitRepo, WatchToken},
    pane::View,
    review::{line_count, MoveProvenance, ReviewFileInput, ReviewHunk, ReviewRow},
    review_apply::{
        base_line_range, hunk_rows, hunk_to_patch, line_restricted_rows, rows_to_unified_diff,
        HUNK_CONTEXT,
    },
    review_session::{
        ChunkIdentity, ChunkStatus, ReviewProgress, ReviewSession, ReviewSource, ReviewViewState,
    },
    workspace::{
        diff::{compute_base_highlights, BaseHighlightCache, DiffBase},
        Workspace,
    },
};
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
};
use stoat_language::{Language, LanguageRegistry};
use stoat_scheduler::Task;
use stoat_text::{Bias, Point, SelectionGoal};

/// A message streamed from a running review scan.
///
/// A working-tree open streams one [`First`](Self::First) then a
/// [`File`](Self::File) per remaining file so the session installs before the
/// whole changeset is diffed, and finally a [`Complete`](Self::Complete)
/// carrying the authoritative whole-changeset session with cross-file moves.
/// A commit scan or a refresh sends only [`Complete`](Self::Complete).
enum ReviewScanMsg {
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
    /// This scan armed a "scanning diff" badge on open, so its completion clears
    /// the review badge tray. A refresh or commits-mode open leaves it `false`,
    /// so a progress or apply-result badge already in the tray survives the
    /// scan.
    scanning_badge: bool,
    /// Set true when a newer scan supersedes this one or the review closes. The
    /// scan closure polls it to abandon its diffs and send nothing more.
    cancel: Arc<AtomicBool>,
}

/// Spawn a review scan whose blocking closure streams [`ReviewScanMsg`] values,
/// and park the pending scan on `stoat`.
///
/// `produce` runs on a blocking thread. It sends messages through the given
/// sender and calls `notify_one` on the given redraw handle after each send so
/// the run loop wakes to pump them. It polls the given cancel flag to stop early
/// when a newer scan supersedes it.
///
/// Arming a scan cancels the one it replaces so that superseded scan stops
/// diffing rather than burning the blocking pool to completion.
fn spawn_review_scan(
    stoat: &mut Stoat,
    sync_path: Option<std::path::PathBuf>,
    scanning_badge: bool,
    produce: impl FnOnce(&mpsc::Sender<ReviewScanMsg>, &Arc<tokio::sync::Notify>, &AtomicBool)
        + Send
        + 'static,
) {
    if let Some(old) = stoat.pending_review_scan.as_ref() {
        old.cancel.store(true, Ordering::Relaxed);
    }
    // A real scan supersedes any background warm over the same tree, so cancel
    // it rather than let both diff to completion.
    if let Some(warm) = stoat.pending_diff_warm.as_ref() {
        warm.cancel();
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let redraw = stoat.redraw_notify.clone();
    let task = {
        let cancel = cancel.clone();
        stoat
            .executor
            .spawn_blocking(move || produce(&tx, &redraw, &cancel))
    };
    stoat.pending_review_scan = Some(PendingReviewScan {
        rx,
        _task: task,
        sync_path,
        scanning_badge,
        cancel,
    });
}

/// Cache key matching what [`populate_diff_cache`] writes, so a scan reads back
/// the hunks a prior install stored for an unchanged file.
pub(crate) fn diff_cache_key(
    base: &str,
    buffer: &str,
    language: Option<&Arc<Language>>,
) -> DiffCacheKey {
    DiffCacheKey {
        left_hash: blake3::hash(base.as_bytes()).into(),
        right_hash: blake3::hash(buffer.as_bytes()).into(),
        language: language.map(|l| l.name.to_string()),
    }
}

pub(super) fn open_review_commit(stoat: &mut Stoat, workdir: &Path, sha: &str) {
    emit_review_info_badge(stoat, "scanning diff");
    let git_host = stoat.git_host.clone();
    let langs = stoat.language_registry.clone();
    let workdir = workdir.to_path_buf();
    let sha = sha.to_string();

    spawn_review_scan(stoat, None, true, move |tx, redraw, cancel| {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if let Some(session) = scan_commit_pure(&*git_host, &langs, &workdir, &sha) {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
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
            if let Some(chunk) = session.doc.chunks.get(id)
                && chunk.status == ChunkStatus::Staged
            {
                groups.entry(chunk.file_index).or_default().push(chunk);
            }
        }
        let tree_snapshot: Vec<(usize, String, Arc<String>, Arc<String>)> = session
            .doc
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
    if repo.has_tracked_changes() {
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
    match scan_commit(stoat, workdir, sha) {
        Some(session) => {
            install_review_session(stoat, session);
        },
        _ => {
            // A rewritten commit with no diffs against its parent has nothing
            // to show, so the review goes rather than sitting there empty.
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

pub(super) fn emit_review_info_badge(stoat: &mut Stoat, label: &str) {
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

pub(super) fn emit_review_error_badge(stoat: &mut Stoat, label: &str, detail: Option<String>) {
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
    let Some((workdir, commit)) = stoat.active_workspace().commits.as_ref().and_then(|state| {
        let commit = state.commits.get(state.selected)?;
        Some((state.workdir.clone(), commit.clone()))
    }) else {
        return UpdateEffect::None;
    };
    super::review_walk::walk_one_commit(stoat, workdir, commit)
}

pub(super) fn open_review_commit_range(stoat: &mut Stoat, workdir: &Path, from: &str, to: &str) {
    emit_review_info_badge(stoat, "scanning diff");
    let git_host = stoat.git_host.clone();
    let langs = stoat.language_registry.clone();
    let workdir = workdir.to_path_buf();
    let from = from.to_string();
    let to = to.to_string();

    spawn_review_scan(stoat, None, true, move |tx, redraw, cancel| {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if let Some(session) = scan_commit_range_pure(&*git_host, &langs, &workdir, &from, &to) {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let _ = tx.send(ReviewScanMsg::Complete(session));
            redraw.notify_one();
        }
    });
}

/// Land an agent's proposed edits as unsaved buffer edits over their own base.
///
/// Each proposal replaces its file's buffer content, and the base texts become
/// a [`DiffBase::Memory`] override, so `:diff` shows the proposal against what
/// the agent read. The first edited file opens with the latch armed.
///
/// A proposal exists nowhere in git, which is what separates this from the
/// commit openers. No revision holds what the agent read, so the base travels
/// with the edits instead.
///
/// Landing the proposals as ordinary unsaved edits is what gives the reader the
/// accept and reject verbs for free. Saving writes a file, `:reload` throws it
/// away, and undo backs one file out at a time, because each proposal is a
/// single edit.
///
/// Does nothing when no proposal opens.
pub(super) fn open_review_agent_edits(stoat: &mut Stoat, edits: &[stoat_action::AgentEdit]) {
    let git_root = stoat.active_workspace().git_root.clone();
    let mut base_texts: std::collections::HashMap<std::path::PathBuf, Arc<String>> =
        std::collections::HashMap::new();
    let mut first: Option<std::path::PathBuf> = None;

    for edit in edits {
        // Join rather than test for absoluteness, since an absolute proposal
        // path comes back from the join unchanged.
        let path = git_root.join(&edit.path);
        let Some(buffer_id) = super::file::open_file(stoat, &path) else {
            continue;
        };
        let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
            continue;
        };
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            let len = guard.snapshot.visible_text.len();
            guard.edit(0..len, &edit.proposed_text);
        }

        base_texts.insert(path.clone(), edit.base_text.clone());
        first.get_or_insert(path);
    }

    let Some(first) = first else {
        return;
    };
    stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::Memory { files: base_texts }));
    super::file::open_file(stoat, &first);
    enter_diff_view(stoat);
    stoat.set_status("agent edits: save accepts, :reload rejects");
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
    move_review_cursor_to_chunk(ws, editor_id, chunk_id);
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

/// Move the review editor's text cursor to the first buffer row of `chunk_id`
/// so `n`/`p` chunk navigation carries the single cursor to the chunk.
///
/// Sets only the selection. [`sync_review_view_and_scroll`] owns the scroll, so
/// a chunk-nav key pressed mid-glide does not yank `scroll_row`.
fn move_review_cursor_to_chunk(
    ws: &mut Workspace,
    editor_id: Option<EditorId>,
    chunk_id: Option<crate::review_session::ReviewChunkId>,
) {
    let (Some(editor_id), Some(chunk_id)) = (editor_id, chunk_id) else {
        return;
    };
    let Some(editor) = ws.editors.get_mut(editor_id) else {
        return;
    };
    let Some(buffer_row) = editor
        .review_view
        .as_ref()
        .and_then(|view| view.row_of_chunk(chunk_id))
    else {
        return;
    };

    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let offset = buffer_snapshot
        .rope()
        .point_to_offset(Point::new(buffer_row, 0));
    let anchor = buffer_snapshot.anchor_at(offset, Bias::Left);
    editor.selections = crate::selection::SelectionsCollection::new();
    editor
        .selections
        .insert_cursor(anchor, SelectionGoal::None, buffer_snapshot);
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
        Arc::make_mut(view).refresh_from_session(session);
    }
    // A refresh that lands mid-motion must not yank scroll_row out from under
    // the animation. scroll_row is the fixed target the offset eases toward
    // during any glide, so re-targeting it to the chunk here would jerk the view
    // to the chunk and back. Re-scroll only after the motion settles. The
    // view-state refresh above still runs regardless.
    let mid_motion = editor.scroll_glide != ScrollGlide::None;
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
                let c = session.doc.chunks.get(id)?;
                if c.status != ChunkStatus::Staged {
                    return None;
                }
                let file = session.doc.files.get(c.file_index)?;
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
        .is_some_and(|s| s.doc.files.iter().any(|f| f.path == path));
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
        // An auto_source session re-decides from the working tree on every
        // refresh, so rescan a WorkingTree source built from its workdir rather
        // than the frozen source it currently displays. This is what lets a
        // rebase-fallback Commit session swap back to the working-tree diff when
        // the tree goes dirty, or to the clean view when the rebase finishes.
        match (old.auto_source, old.source.workdir()) {
            (true, Some(workdir)) => ReviewSource::WorkingTree {
                workdir: workdir.to_path_buf(),
            },
            _ => old.source.clone(),
        }
    };

    if is_git_source(&source) {
        let git_host = stoat.git_host.clone();
        let langs = stoat.language_registry.clone();

        // A refresh sends only Complete, never streaming increments: streaming
        // would replace the live session with fresh Pending chunks and flicker
        // the user's decisions away. Leaving the old session up until Complete
        // lets the pump reapply carried statuses from it.
        spawn_review_scan(stoat, sync_path, false, move |tx, redraw, cancel| {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Some(session) = rescan_git_source_pure(&*git_host, &langs, &source) {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let _ = tx.send(ReviewScanMsg::Complete(session));
                redraw.notify_one();
            }
        });
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
            let status = session.doc.chunks.get(id)?.status;
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
    langs: &LanguageRegistry,
    source: &ReviewSource,
) -> Option<ReviewSession> {
    match source {
        ReviewSource::Commit { workdir, sha } => scan_commit_pure(git, langs, workdir, sha),
        ReviewSource::CommitRange { workdir, from, to } => {
            scan_commit_range_pure(git, langs, workdir, from, to)
        },
        ReviewSource::WorkingTree { .. }
        | ReviewSource::AgentEdits { .. }
        | ReviewSource::InMemory { .. } => None,
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
        .doc
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
        ReviewSource::Commit { workdir, sha } => scan_commit(stoat, workdir, sha),
        ReviewSource::CommitRange { workdir, from, to } => {
            scan_commit_range(stoat, workdir, from, to)
        },
        ReviewSource::AgentEdits { edits } => scan_agent_edits(stoat, edits.as_ref()),
        ReviewSource::InMemory { files } => scan_in_memory(stoat, files.as_ref()),
        ReviewSource::WorkingTree { .. } => None,
    }
}

pub(super) fn close_review(stoat: &mut Stoat) -> UpdateEffect {
    use crate::badge::BadgeSource;

    // Cancel and drop any in-flight scan so it stops diffing and never installs
    // a session into the review the user just closed. Clear its scanning badge
    // too, since the cancelled scan will not reach the pump that normally does.
    if let Some(pending) = stoat.pending_review_scan.take() {
        pending.cancel.store(true, Ordering::Relaxed);
        stoat
            .active_workspace_mut()
            .badges
            .remove_by_source(BadgeSource::Review);
    }

    let executor = stoat.executor.clone();
    let fs_watch_host = stoat.fs_watch_host.clone();
    let ws = stoat.active_workspace_mut();
    let Some(mut session) = ws.review.take() else {
        return UpdateEffect::None;
    };
    for token in std::mem::take(&mut session.watch_tokens) {
        fs_watch_host.unwatch(token);
    }
    ws.badges.remove_by_source(BadgeSource::Review);
    let Some(review_editor_id) = session.view_editor else {
        return UpdateEffect::Redraw;
    };

    let (scratch_id, scratch_buffer) = ws.buffers.new_scratch();
    let replacement = EditorState::new(
        scratch_id,
        scratch_buffer,
        executor,
        ws.redraw_notify.clone(),
    );
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

    if !ws.editor_referenced(review_editor_id) {
        ws.editors.remove(review_editor_id);
    }

    UpdateEffect::Redraw
}

/// Leave the diff view, reporting whether there was one to leave.
///
/// Clears the focused editor's flag, the focused pane's latch, and the widen
/// the view took, which is every piece of state entering it set. Returns false
/// and touches nothing when no editor is focused or neither half is set, so a
/// caller can offer the exit unconditionally.
///
/// Either half being set counts as on, so this leaves a latched pane showing a
/// clean file as readily as a diff itself.
/// Open the diff against `rev`, or toggle the working-tree diff when `None`.
///
/// A revision points the whole workspace at that commit, so every buffer diffs
/// against it and the change list spans everything committed since. Opening the
/// view from a buffer with no hunks of its own then crosses into the first file
/// in that list, which is what [`toggle_diff_view`] already does and what makes
/// a revision land somewhere worth reading: with a base further back, the file
/// the reader wants is rarely the one on screen.
///
/// A revision the repository cannot resolve changes nothing at all: it badges
/// and leaves the view, the latch, and the base where they were. A command that
/// half-applies leaves the reader worse off than one that refuses.
pub(super) fn diff(stoat: &mut Stoat, rev: Option<&str>) -> UpdateEffect {
    let Some(rev) = rev else {
        // The bare command also drops any base a revision left installed, so
        // closing the diff always returns to the working tree's own HEAD.
        toggle_diff_view(stoat);
        return UpdateEffect::Redraw;
    };

    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        emit_review_error_badge(stoat, "not in a git repository", None);
        return UpdateEffect::Redraw;
    };
    let Some(sha) = repo.resolve_rev(rev) else {
        emit_review_error_badge(
            stoat,
            "unknown revision",
            Some(format!("no revision named {rev}")),
        );
        return UpdateEffect::Redraw;
    };

    stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::Rev { sha: Some(sha) }));
    // Turned on rather than toggled. Naming a revision asks to look at it, so a
    // second `:diff <rev>` re-targets the view rather than closing it; only the
    // bare command closes.
    enter_diff_view(stoat);
    UpdateEffect::Redraw
}

/// Point the diff view at what `sha` changed, with the tree already on it.
///
/// The base becomes the commit's first parent, so the diff on screen is the
/// commit itself. A root commit has no parent, and the empty tree is the base
/// that makes every one of its lines read as added rather than as nothing.
///
/// The caller is responsible for the tree: this reads the commit to decide what
/// to open, but the buffers it opens are whatever is on disk, so a tree that is
/// not on `sha` yields a diff of something else entirely.
///
/// Returns whether a file was opened. A commit that changed nothing has none to
/// open, so the view is left exactly where it was and only the base moves.
pub(super) fn land_diff_on_commit(
    stoat: &mut Stoat,
    repo: &dyn GitRepo,
    workdir: &Path,
    sha: &str,
) -> bool {
    stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::Rev {
            sha: repo.parent_sha(sha),
        }));

    let Some(change) = repo.commit_file_changes(sha).into_iter().next() else {
        return false;
    };
    super::file::open_file(stoat, &workdir.join(change.rel_path));
    enter_diff_view(stoat);
    true
}

/// Turn the diff view on, whatever it was showing before.
///
/// [`toggle_diff_view`] closes an open view, so a caller that means "show this"
/// rather than "flip this" has to shut the view before opening it. Every such
/// caller arrives with a base it just installed, and the exit half leaves a
/// base alone by design, so the round trip re-reads the new base rather than
/// dropping it.
pub(super) fn enter_diff_view(stoat: &mut Stoat) {
    exit_diff_view(stoat);
    toggle_diff_view(stoat);
}

pub(super) fn exit_diff_view(stoat: &mut Stoat) -> bool {
    let latched = {
        let panes = &stoat.active_workspace().panes;
        panes.pane(panes.focus()).diff_mode
    };
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return false;
    };
    if !(latched || editor.diff_view) {
        return false;
    }
    editor.set_diff_view(false);

    let panes = &mut stoat.active_workspace_mut().panes;
    let focus = panes.focus();
    panes.pane_mut(focus).diff_mode = false;
    if panes.widened() == Some(focus) {
        panes.unwiden();
    }
    true
}

/// Toggle the live per-file diff view on the focused editor, driven by
/// [`stoat_action::Diff`].
///
/// Flips `diff_view` (and the display map's deleted-block splicing) on the
/// focused editor. Unlike the session review there is no scan, no session, and
/// no scratch buffer -- the editor stays the real, editable file buffer, so
/// re-pressing toggles the two columns off again.
///
/// Opening the view lands the cursor on the focused buffer's first change chunk
/// (the hunk `n` reaches from the top of the file) and pushes the pre-jump
/// position to the jumplist, so the usual jump-back returns. When the focused
/// buffer has no changes of its own, opening the view instead crosses into the
/// first changed file and lands on its first hunk. Toggling the view off leaves
/// the cursor untouched.
///
/// See also:
/// - [`exit_diff_view`], the exit half, which other screens reuse.
pub(super) fn toggle_diff_view(stoat: &mut Stoat) {
    let origin = super::jump::live_entry(stoat);
    let Some(buffer_id) = super::focused_editor_mut(stoat).map(|editor| editor.buffer_id) else {
        return;
    };

    if exit_diff_view(stoat) {
        stoat.active_workspace_mut().set_diff_base(None);
        return;
    }

    {
        let Some(editor) = super::focused_editor_mut(stoat) else {
            return;
        };
        editor.set_diff_view(true);

        // Give the diff its own full width. An unwidenable layout stays put and
        // rides the unified fallback.
        let panes = &mut stoat.active_workspace_mut().panes;
        let focus = panes.focus();
        panes.pane_mut(focus).diff_mode = true;
        panes.widen(focus);
    }

    if let Some((editor_id, _)) = stoat.focused_editor_ids() {
        ensure_diff_map(stoat, editor_id, buffer_id);
    }

    let jumped = super::focused_editor_mut(stoat).is_some_and(|editor| {
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        // The first stop rather than the first hunk, so opening over a refined
        // block lands on the row that changed instead of the block's top.
        let target_row = display_snapshot.diff_map().and_then(|diff_map| {
            diff_map
                .live_hunks(buffer_snapshot)
                .change_stops()
                .first()
                .map(|stop| stop.start)
        });
        let Some(target_row) = target_row else {
            return false;
        };
        let target_offset = buffer_snapshot
            .rope()
            .point_to_offset(Point::new(target_row, 0));
        editor.selections.transform(buffer_snapshot, |sel| {
            crate::selection::land_block_cursor(
                sel.id,
                target_offset,
                SelectionGoal::None,
                buffer_snapshot.rope(),
                buffer_snapshot,
            )
        });
        true
    });

    // The jump only moves the selection. Pull the view onto it here so a
    // non-key dispatch (the `stoat review` startup, a mouse palette accept)
    // lands scrolled, not relying on the Key-event epilogue.
    if jumped {
        let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
        if let Some(editor) = super::focused_editor_mut(stoat) {
            super::view::ensure_cursor_in_view(editor, scrolloff);
        }
    }

    if jumped && let Some(entry) = origin {
        super::jump::push_entry(stoat, entry);
    }

    // A buffer with no changes of its own has no hunk to land on, so cross into
    // the first changed file. This makes `:diff` from a scratch or unchanged
    // buffer open a real diff instead of silently toggling empty columns.
    if !jumped {
        let _ = super::movement::goto_change(stoat, ChangeDir::Next);
    }
}

/// Make sure `editor_id`'s display map holds a current diff map for
/// `buffer_id`, and report whether it found any hunk to show.
///
/// The editor is named rather than taken as the focused one, because the pane a
/// navigation targets is not always the focused pane, and an answer from the
/// wrong editor's map opens the wrong view.
///
/// The background diff job usually has the map ready, since it drives the
/// gutter marks, so the synchronous install below runs only when the fast path
/// is empty. A map left over from before an external git mutation counts as
/// empty. Such a map describes a base that has since moved, so trusting it
/// opens the view on hunks measured against a HEAD that no longer exists.
///
/// Only the hunks are computed here. The left column's colors cost a parse of
/// the whole base file, which the background pass takes on and lands a settle
/// later.
///
/// `false` is the answer for a clean or untracked file, which is what tells a
/// latched pane to show it plain.
pub(crate) fn ensure_diff_map(stoat: &mut Stoat, editor_id: EditorId, buffer_id: BufferId) -> bool {
    let map_current = stoat.active_workspace().diff_map_current(buffer_id);
    let has_map = stoat
        .active_workspace_mut()
        .editors
        .get_mut(editor_id)
        .is_some_and(|editor| editor.display_map.snapshot().diff_map().is_some());
    if !has_map || !map_current {
        let git_host = stoat.git_host.clone();
        let language_registry = stoat.language_registry.clone();
        let syntax_styles = stoat.syntax_styles.clone();
        let base_cache = stoat.base_highlights_cache.clone();
        stoat.active_workspace_mut().install_diff_map_now(
            &git_host,
            &language_registry,
            &syntax_styles,
            &base_cache,
            buffer_id,
        );
    }

    stoat
        .active_workspace_mut()
        .editors
        .get_mut(editor_id)
        .is_some_and(|editor| {
            editor
                .display_map
                .snapshot()
                .diff_map()
                .is_some_and(|diff_map| !diff_map.hunks_in_range(0..u32::MAX).is_empty())
        })
}

/// How [`stage_hunk`] acts on the hunk under the cursor.
#[derive(Clone, Copy)]
pub(super) enum HunkStage {
    Stage,
    Unstage,
    Toggle,
}

/// Stage, unstage, or toggle the git-index state of the diff hunk under the
/// cursor in the focused editor.
///
/// The hunk is resolved by diffing the file's HEAD content against the live
/// buffer and taking the one whose buffer rows hold the cursor, which is the
/// rule the gutter marks by, so the staged unit is the one drawn there. The
/// action works in any editor view on a git-tracked file. A missing repo, an
/// untracked file, or a cursor on a row no hunk covers sets a status message
/// and changes nothing.
///
/// [`HunkStage::Toggle`] has no staged-state signal to read yet, so it stages
/// by applying the forward patch and, only when that fails because the hunk is
/// already staged, unstages by applying the reverse patch.
pub(super) fn stage_hunk(stoat: &mut Stoat, mode: HunkStage) -> UpdateEffect {
    let Some((_editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let (cursor_row, buffer_text) = {
        let Some(editor) = super::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let head = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor_row = buffer_snapshot.rope().offset_to_point(head).row;
        (cursor_row, buffer_snapshot.rope().to_string())
    };

    let Some(path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let git_root = stoat.active_workspace().git_root.clone();

    let Some(repo) = stoat.git_host.discover(&git_root) else {
        stoat.set_status("not in a git repository");
        return UpdateEffect::Redraw;
    };

    // Decided before the hunk is resolved, so a refusal says why the transport
    // is unavailable rather than reporting whatever sits under the cursor.
    match super::amend::amend_route(stoat, &*repo) {
        AmendRoute::Index => {},
        AmendRoute::Commit(target) => {
            let site = super::amend::HunkSite {
                buffer_id,
                path: &path,
                cursor_row,
                buffer_text: &buffer_text,
            };
            return super::amend::amend_hunk(
                stoat,
                &*repo,
                &target,
                mode,
                super::amend::AmendUnit::Hunk,
                site,
            );
        },
        AmendRoute::Refused => {
            stoat.set_status(super::amend::REFUSED_BADGE);
            return UpdateEffect::Redraw;
        },
    }

    let Some(base_text) = repo.head_content(&path) else {
        stoat.set_status("no hunk under the cursor");
        return UpdateEffect::Redraw;
    };

    let rel = path.strip_prefix(&git_root).unwrap_or(&path).to_path_buf();
    let hunks = {
        let result = stoat_language::structural_diff::diff(&base_text, &buffer_text);
        crate::diff_map::changes_to_hunks(&result.changes, &base_text, &buffer_text)
    };

    // Resolved by the gutter's own rule, so the staged unit is the one drawn
    // under the cursor. A zero-width range is a deletion or a move, which the
    // gutter marks at its anchor row.
    let Some(k) = hunks.iter().position(|hunk| {
        let rows = &hunk.buffer_line_range;
        match rows.is_empty() {
            true => rows.start == cursor_row,
            false => rows.contains(&cursor_row),
        }
    }) else {
        stoat.set_status("no hunk under the cursor");
        return UpdateEffect::Redraw;
    };

    let (Some(forward), Some(reverse)) = (
        hunk_to_patch(&rel, &base_text, &buffer_text, &hunks, k, false),
        hunk_to_patch(&rel, &base_text, &buffer_text, &hunks, k, true),
    ) else {
        stoat.set_status("no hunk under the cursor");
        return UpdateEffect::Redraw;
    };

    let result = match mode {
        HunkStage::Stage => repo.apply_to_index(&forward).map(|()| "staged hunk"),
        HunkStage::Unstage => repo.apply_to_index(&reverse).map(|()| "unstaged hunk"),
        HunkStage::Toggle => match repo.apply_to_index(&forward) {
            Ok(()) => Ok("staged hunk"),
            Err(_) => repo.apply_to_index(&reverse).map(|()| "unstaged hunk"),
        },
    };

    match result {
        Ok(message) => {
            stoat.active_workspace_mut().invalidate_diff(buffer_id);
            stoat.set_status(message);
        },
        Err(err) => stoat.set_status(format!("could not update staging: {err}")),
    }

    UpdateEffect::Redraw
}

/// Stage, unstage, or toggle the git-index state of only the cursor line's
/// change, in the focused editor on any git-tracked file.
///
/// Unlike [`stage_hunk`], the emitted patch is restricted to the cursor's line
/// via [`line_restricted_rows`], and staging diffs the git index (not HEAD)
/// against the live buffer so sequential single-line stages inside one hunk
/// compose. A missing repo, an untracked file, or a cursor on no change sets a
/// status message and changes nothing.
///
/// The index is not the only target. Under a review base the staged side is the
/// checked-out commit, so the keys amend the cursor's line into or out of it
/// through [`amend::amend_hunk`], leaving the rest of the hunk where it was.
/// [`stage_hunk`] moves the whole hunk the same way.
pub(super) fn stage_line(stoat: &mut Stoat, mode: HunkStage) -> UpdateEffect {
    let Some((_editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let (cursor_row, buffer_text) = {
        let Some(editor) = super::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let head = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor_row = buffer_snapshot.rope().offset_to_point(head).row;
        (cursor_row, buffer_snapshot.rope().to_string())
    };

    let Some(path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let git_root = stoat.active_workspace().git_root.clone();

    let Some(repo) = stoat.git_host.discover(&git_root) else {
        stoat.set_status("not in a git repository");
        return UpdateEffect::Redraw;
    };

    // Read before the line is resolved, so a refusal names the missing
    // transport rather than whatever sits under the cursor.
    match super::amend::amend_route(stoat, &*repo) {
        AmendRoute::Index => {},
        AmendRoute::Commit(target) => {
            let site = super::amend::HunkSite {
                buffer_id,
                path: &path,
                cursor_row,
                buffer_text: &buffer_text,
            };
            return super::amend::amend_hunk(
                stoat,
                &*repo,
                &target,
                mode,
                super::amend::AmendUnit::Line,
                site,
            );
        },
        AmendRoute::Refused => {
            stoat.set_status(super::amend::REFUSED_BADGE);
            return UpdateEffect::Redraw;
        },
    }

    let Some(head_text) = repo.head_content(&path) else {
        stoat.set_status("no line change under the cursor");
        return UpdateEffect::Redraw;
    };
    let index_text = repo
        .index_content(&path)
        .unwrap_or_else(|| head_text.clone());

    let rel = path
        .strip_prefix(&git_root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();

    let stage = || stage_line_patch(&rel, &index_text, &buffer_text, cursor_row);
    let unstage = || unstage_line_patch(&rel, &index_text, &head_text, &buffer_text, cursor_row);
    let patch_and_message = match mode {
        HunkStage::Stage => stage().map(|patch| (patch, "staged line")),
        HunkStage::Unstage => unstage().map(|patch| (patch, "unstaged line")),
        HunkStage::Toggle => stage()
            .map(|patch| (patch, "staged line"))
            .or_else(|| unstage().map(|patch| (patch, "unstaged line"))),
    };

    let Some((patch, message)) = patch_and_message else {
        stoat.set_status("no line change under the cursor");
        return UpdateEffect::Redraw;
    };

    match repo.apply_to_index(&patch) {
        Ok(()) => {
            stoat.active_workspace_mut().invalidate_diff(buffer_id);
            stoat.set_status(message);
        },
        Err(err) => stoat.set_status(format!("could not update staging: {err}")),
    }

    UpdateEffect::Redraw
}

/// Line hunks between `base` and `buffer`, the extents every staging path
/// resolves against.
fn line_hunks(base: &str, buffer: &str) -> Vec<crate::diff_map::DiffHunk> {
    let result = stoat_language::structural_diff::diff(base, buffer);
    crate::diff_map::changes_to_hunks(&result.changes, base, buffer)
}

/// Build the patch that stages only the cursor line by diffing the git index
/// against the live buffer and keeping the cursor row's change.
///
/// `None` when the cursor sits on no index-vs-buffer change.
fn stage_line_patch(
    rel: &str,
    index_text: &str,
    buffer_text: &str,
    cursor_row: u32,
) -> Option<String> {
    let hunks = line_hunks(index_text, buffer_text);
    let k = hunks
        .iter()
        .position(|hunk| hunk.buffer_line_range.contains(&cursor_row))?;
    let hunk_rows = hunk_rows(index_text, buffer_text, &hunks, k, HUNK_CONTEXT)?;
    let rows = line_restricted_rows(&hunk_rows, cursor_row + 1, true)?;
    Some(rows_to_unified_diff(
        Path::new(rel),
        index_text,
        buffer_text,
        &rows,
    ))
}

/// Build the patch that unstages only the cursor line by reverting its index
/// row to HEAD.
///
/// The cursor row is a buffer coordinate, so it is first mapped back to the
/// index by removing the line delta of every index-vs-buffer hunk above it.
/// Diffing the index against HEAD then expresses the staged change as an
/// index-side row, and the forward patch reverts that one row to HEAD. `None`
/// when the mapped row carries no staged change.
fn unstage_line_patch(
    rel: &str,
    index_text: &str,
    head_text: &str,
    buffer_text: &str,
    cursor_row: u32,
) -> Option<String> {
    let index_row = map_buffer_row_to_index(index_text, buffer_text, cursor_row);
    // The emitted patch runs index to HEAD, so the index is this diff's base
    // side and the staged row is a base row. DiffHunk records base bytes rather
    // than base lines, so the range comes from base_line_range.
    let hunks = line_hunks(index_text, head_text);
    let k =
        (0..hunks.len()).find(|&k| base_line_range(index_text, &hunks, k).contains(&index_row))?;
    let hunk_rows = hunk_rows(index_text, head_text, &hunks, k, HUNK_CONTEXT)?;
    let rows = line_restricted_rows(&hunk_rows, index_row + 1, false)?;
    Some(rows_to_unified_diff(
        Path::new(rel),
        index_text,
        head_text,
        &rows,
    ))
}

/// Map a 0-based buffer row to its 0-based row in the git index.
///
/// Each index-vs-buffer hunk fully above the cursor shifts buffer rows past the
/// index by its added-minus-removed line count, so subtracting that shift
/// recovers the index row. The `index_text` slicing keys off each hunk's base
/// byte range to count its index lines.
fn map_buffer_row_to_index(index_text: &str, buffer_text: &str, cursor_row: u32) -> u32 {
    let mut shift: i64 = 0;
    for hunk in line_hunks(index_text, buffer_text) {
        if hunk.buffer_line_range.end <= cursor_row {
            let buffer_len = (hunk.buffer_line_range.end - hunk.buffer_line_range.start) as i64;
            let index_len =
                line_count(index_text.get(hunk.base_byte_range.clone()).unwrap_or("")) as i64;
            shift += buffer_len - index_len;
        }
    }
    (cursor_row as i64 - shift).max(0) as u32
}

/// Toggle the focused pane between the side-by-side diff and a plain editor
/// on the same file, driven by [`stoat_action::ToggleDiff`].
///
/// From the diff (`toggled_off` clear) this hides it; from the parked state
/// it swaps the diff back in. A no-op when no review session is open.
pub(super) fn toggle_diff(stoat: &mut Stoat) -> UpdateEffect {
    match stoat
        .active_workspace()
        .review
        .as_ref()
        .map(|s| s.toggled_off)
    {
        Some(false) => toggle_diff_off(stoat),
        Some(true) => toggle_diff_on(stoat),
        None => UpdateEffect::None,
    }
}

/// Hide the diff by opening the real file at the line under the review
/// cursor, parking the review editor so [`toggle_diff_on`] can restore it.
///
/// The session stays installed with `toggled_off` set and the review scroll
/// row stashed, so the editor GC keeps the parked editor alive.
fn toggle_diff_off(stoat: &mut Stoat) -> UpdateEffect {
    let Some((path, line)) = review_cursor_file_target(stoat) else {
        // An empty diff view (clean tree) has no file under the cursor to swap
        // in, so there is nothing to toggle to.
        stoat.set_status("no file to open in the diff view");
        return UpdateEffect::Redraw;
    };

    park_review_session(stoat);

    let focused = stoat.active_workspace().panes.focus();
    crate::buffer_lifecycle::open_file_in_pane(stoat, focused, &path);
    if let Some(editor) = super::focused_editor_mut(stoat) {
        super::movement::set_cursor_row(editor, line.saturating_sub(1));
    }

    stoat.set_focused_mode("normal".to_string());
    UpdateEffect::Redraw
}

/// Park the installed review session so its editor survives an editor swap.
///
/// Marks the session toggled off and stashes the review editor's current
/// scroll row for a later toggle-back, which is what keeps the editor GC from
/// reclaiming the parked editor. Leaves the pane view and mode to the caller.
/// No-op when no session is open. Shared by [`toggle_diff_off`] and a
/// goto-from-diff jump, which parks the review before opening its target.
pub(super) fn park_review_session(stoat: &mut Stoat) {
    let ws = stoat.active_workspace_mut();
    let scroll_row = ws
        .review
        .as_ref()
        .and_then(|s| s.view_editor)
        .and_then(|id| ws.editors.get(id))
        .map(|e| e.scroll_row);
    if let Some(session) = ws.review.as_mut() {
        session.toggled_off = true;
        session.stashed_display_row = scroll_row;
    }
}

/// Resolve the review cursor to `(file path, 1-based new-side line)`.
///
/// `None` when the focused review editor has no cursor row, the row maps to
/// no chunk, or the chunk's file is missing.
fn review_cursor_file_target(stoat: &mut Stoat) -> Option<(std::path::PathBuf, u32)> {
    let ws = stoat.active_workspace_mut();
    let editor_id = ws.review.as_ref()?.view_editor?;

    let buffer_row = {
        let editor = ws.editors.get_mut(editor_id)?;
        editor.review_view.as_ref()?;
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let offset = stoat_text::cursor_offset(
            buffer_snapshot.rope(),
            buffer_snapshot.resolve_anchor(&sel.tail()),
            buffer_snapshot.resolve_anchor(&sel.head()),
        );
        buffer_snapshot.rope().offset_to_point(offset).row
    };

    let view = ws.editors.get(editor_id)?.review_view.as_ref()?;
    let line = row_line_num(view.rows.get(buffer_row as usize)?);
    let (chunk_id, _) = view.chunk_and_status_at_row(buffer_row)?;

    let session = ws.review.as_ref()?;
    let file_index = session.doc.chunks.get(&chunk_id)?.file_index;
    let path = session.doc.files.get(file_index)?.path.clone();
    Some((path, line))
}

/// The new-side line number for `row`, falling back to the old side on a
/// deletion-only row.
fn row_line_num(row: &ReviewRow) -> u32 {
    match row {
        ReviewRow::Context { right, .. } => right.line_num,
        ReviewRow::Changed { right: Some(r), .. } => r.line_num,
        ReviewRow::Changed { left: Some(l), .. } => l.line_num,
        ReviewRow::Changed {
            left: None,
            right: None,
        } => 1,
    }
}

/// Resolve the review cursor to a working-tree file position for LSP:
/// `(real file path, 0-based new-side line, byte column)`.
///
/// `None` unless the focused review editor is over a
/// [`ReviewSource::WorkingTree`] diff and the cursor sits on a row with a new
/// side (context or an addition). A deletion-only row has no position in the
/// new file. The byte column carries over unchanged because the placeholder
/// row mirrors the new-side line verbatim, so only the line is remapped.
pub(super) fn review_cursor_file_position(
    stoat: &mut Stoat,
) -> Option<(std::path::PathBuf, u32, u32)> {
    let ws = stoat.active_workspace_mut();
    let session = ws.review.as_ref()?;
    if !matches!(session.source, ReviewSource::WorkingTree { .. }) {
        return None;
    }
    let editor_id = session.view_editor?;

    let (buffer_row, col) = {
        let editor = ws.editors.get_mut(editor_id)?;
        editor.review_view.as_ref()?;
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let offset = stoat_text::cursor_offset(
            buffer_snapshot.rope(),
            buffer_snapshot.resolve_anchor(&sel.tail()),
            buffer_snapshot.resolve_anchor(&sel.head()),
        );
        let point = buffer_snapshot.rope().offset_to_point(offset);
        (point.row, point.column)
    };

    let view = ws.editors.get(editor_id)?.review_view.as_ref()?;
    let line = review_row_new_line(view.rows.get(buffer_row as usize)?)?;
    let (chunk_id, _) = view.chunk_and_status_at_row(buffer_row)?;

    let session = ws.review.as_ref()?;
    let file_index = session.doc.chunks.get(&chunk_id)?.file_index;
    let path = session.doc.files.get(file_index)?.path.clone();
    Some((path, line.saturating_sub(1), col))
}

/// The new-side line number for `row`, or `None` for a deletion-only row,
/// which has no position in the new file.
fn review_row_new_line(row: &ReviewRow) -> Option<u32> {
    match row {
        ReviewRow::Context { right, .. } => Some(right.line_num),
        ReviewRow::Changed { right: Some(r), .. } => Some(r.line_num),
        ReviewRow::Changed { right: None, .. } => None,
    }
}

/// Which counterpart of a moved hunk a jump follows.
#[derive(Copy, Clone, Debug)]
pub(super) enum MoveJumpDir {
    /// Jump to the moved content's source, recorded on the added (right)
    /// side of the row under the cursor. Bound to `m`.
    Source,
    /// Jump to the moved content's destination, recorded on the deleted
    /// (left) side of the row under the cursor. Bound to `M`.
    Target,
}

/// Jump the review cursor from a moved hunk to its cross-file counterpart.
///
/// Reads the [`MoveProvenance`] on the side of the cursor's row selected by
/// `dir`, resolves the referenced file and chunk within this session, and
/// moves the cursor and scroll there. The review flattens every file into
/// one editor, so this is an in-editor move rather than a file open.
///
/// When the referenced file has no changes in this diff -- its content only
/// exists at HEAD, so it is not one of the session's files -- emits a "move
/// origin not in this diff" badge instead of jumping. A no-op when the cursor
/// is not on a row with provenance on the selected side (including every
/// intra-file move, whose provenance is not recorded yet).
pub(super) fn jump_to_move(stoat: &mut Stoat, dir: MoveJumpDir) -> UpdateEffect {
    let Some(prov) = cursor_move_provenance(stoat, dir) else {
        return UpdateEffect::None;
    };

    let target = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.review.as_ref() else {
            return UpdateEffect::None;
        };
        session
            .doc
            .files
            .iter()
            .position(|f| f.rel_path == prov.rel_path)
            .and_then(|file_index| chunk_for_line(session, file_index, prov.line))
    };

    let Some(chunk_id) = target else {
        emit_review_info_badge(stoat, "move origin not in this diff");
        return UpdateEffect::Redraw;
    };

    let ws = stoat.active_workspace_mut();
    let editor_id = ws.review.as_ref().and_then(|s| s.view_editor);
    if let Some(session) = ws.review.as_mut() {
        session.cursor.current = Some(chunk_id);
        session.version += 1;
    }
    move_review_cursor_to_chunk(ws, editor_id, Some(chunk_id));
    sync_review_view_and_scroll(ws, editor_id, Some(chunk_id));
    UpdateEffect::Redraw
}

/// The [`MoveProvenance`] on the cursor row's side selected by `dir`, or
/// `None` when the focused editor is not a review editor or the row carries
/// no provenance on that side.
fn cursor_move_provenance(stoat: &mut Stoat, dir: MoveJumpDir) -> Option<MoveProvenance> {
    let ws = stoat.active_workspace_mut();
    let editor_id = ws.review.as_ref()?.view_editor?;

    let buffer_row = {
        let editor = ws.editors.get_mut(editor_id)?;
        editor.review_view.as_ref()?;
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let offset = stoat_text::cursor_offset(
            buffer_snapshot.rope(),
            buffer_snapshot.resolve_anchor(&sel.tail()),
            buffer_snapshot.resolve_anchor(&sel.head()),
        );
        buffer_snapshot.rope().offset_to_point(offset).row
    };

    let view = ws.editors.get(editor_id)?.review_view.as_ref()?;
    let row = view.rows.get(buffer_row as usize)?;
    row_move_provenance(row, dir).cloned()
}

/// The cross-file [`MoveProvenance`] on the row side selected by `dir`: the
/// added (right) side for [`MoveJumpDir::Source`], the deleted (left) side
/// for [`MoveJumpDir::Target`].
///
/// Intra-file provenance is skipped. Its counterpart is in the same file and
/// carries no target path to resolve, so jumping is not wired for it yet.
fn row_move_provenance(row: &ReviewRow, dir: MoveJumpDir) -> Option<&MoveProvenance> {
    let side = match dir {
        MoveJumpDir::Source => match row {
            ReviewRow::Context { right, .. } => Some(right),
            ReviewRow::Changed { right: Some(r), .. } => Some(r),
            ReviewRow::Changed { right: None, .. } => None,
        },
        MoveJumpDir::Target => match row {
            ReviewRow::Context { left, .. } => Some(left),
            ReviewRow::Changed { left: Some(l), .. } => Some(l),
            ReviewRow::Changed { left: None, .. } => None,
        },
    };
    side?.move_provenance.as_ref().filter(|p| !p.intra_file)
}

/// The chunk in `file_index` whose base or buffer line range contains `line`.
///
/// A move provenance line is a source line (base coordinates) for a deletion
/// and a target line (buffer coordinates) for an addition, and the side is
/// not preserved on [`MoveProvenance`], so both ranges are checked.
fn chunk_for_line(
    session: &ReviewSession,
    file_index: usize,
    line: u32,
) -> Option<crate::review_session::ReviewChunkId> {
    let file = session.doc.files.get(file_index)?;
    file.chunks
        .iter()
        .find(|id| {
            session.doc.chunks.get(id).is_some_and(|c| {
                c.base_line_range.contains(&line) || c.buffer_line_range.contains(&line)
            })
        })
        .copied()
}

/// Swap the parked review editor back into the focused pane, positioning it
/// on the chunk the plain-file cursor sits in.
///
/// Falls back to the scroll row stashed at toggle-off when the file cursor
/// maps to no chunk (e.g. the pane now shows a file not in the session). A
/// no-op unless a toggled-off session exists.
fn toggle_diff_on(stoat: &mut Stoat) -> UpdateEffect {
    let review_editor_id = match stoat.active_workspace().review.as_ref() {
        Some(s) if s.toggled_off => s.view_editor,
        _ => return UpdateEffect::None,
    };
    let Some(review_editor_id) = review_editor_id else {
        return UpdateEffect::None;
    };

    let (file_buffer_id, file_byte) = {
        let Some(editor) = super::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        (
            editor.buffer_id,
            stoat_text::cursor_offset(
                buffer_snapshot.rope(),
                buffer_snapshot.resolve_anchor(&sel.tail()),
                buffer_snapshot.resolve_anchor(&sel.head()),
            ),
        )
    };

    let target_chunk = {
        let ws = stoat.active_workspace();
        let path = ws.buffers.path_for(file_buffer_id).map(Path::to_path_buf);
        ws.review.as_ref().and_then(|session| {
            let path = path.as_ref()?;
            let file_index = session.doc.files.iter().position(|f| &f.path == path)?;
            session.chunk_containing_buffer_byte(file_index, file_byte)
        })
    };

    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let file_editor_id = match ws.panes.pane(focused).view {
        View::Editor(eid) => Some(eid),
        _ => None,
    };
    ws.panes.pane_mut(focused).view = View::Editor(review_editor_id);

    if let Some(chunk_id) = target_chunk {
        if let Some(session) = ws.review.as_mut()
            && session.cursor.current != Some(chunk_id)
        {
            session.cursor.current = Some(chunk_id);
            session.version += 1;
        }
        move_review_cursor_to_chunk(ws, Some(review_editor_id), Some(chunk_id));
        sync_review_view_and_scroll(ws, Some(review_editor_id), Some(chunk_id));
    } else if let Some(stashed) = ws.review.as_ref().and_then(|s| s.stashed_display_row)
        && let Some(editor) = ws.editors.get_mut(review_editor_id)
    {
        editor.scroll_row = stashed;
    }

    if let Some(session) = ws.review.as_mut() {
        session.toggled_off = false;
        session.stashed_display_row = None;
    }

    if let Some(file_id) = file_editor_id
        && file_id != review_editor_id
    {
        super::gc_editor_if_unreferenced(ws, file_id);
    }

    UpdateEffect::Redraw
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
/// A channel that closes before any file streamed and installs no session means
/// the root is not a git repository, which badges "not a git repository". A clean
/// tree in a repo instead sends an empty [`ReviewScanMsg::Complete`], keeping the
/// diff view open on an empty session for the watch pipeline to fill.
///
/// Returns whether a message was drained this call, mirroring the other
/// render-time pumps. Driven from [`Stoat::render`] and the test harness
/// `settle` loop.
pub(crate) fn pump_review_scan(stoat: &mut Stoat) -> bool {
    let Some(pending) = stoat.pending_review_scan.take() else {
        return false;
    };

    match pending.rx.try_recv() {
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

            // Clear the scanning badge before installing, so a rebase-fallback
            // badge that install_review_session emits for this session survives
            // rather than being swept away with the scanning badge.
            if pending.scanning_badge {
                use crate::badge::BadgeSource;
                stoat
                    .active_workspace_mut()
                    .badges
                    .remove_by_source(BadgeSource::Review);
            }
            install_review_session(stoat, session);

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
            if stoat.active_workspace().review.is_none() {
                // A scan that installs no session found nothing to review, from
                // an unknown repo or commit. The badge replaces the scanning one.
                emit_review_info_badge(stoat, "nothing to review");
            } else if pending.scanning_badge {
                use crate::badge::BadgeSource;
                stoat
                    .active_workspace_mut()
                    .badges
                    .remove_by_source(BadgeSource::Review);
            }
            true
        },
    }
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
    let parent = repo.parent_sha(sha);
    let changes = changed_or_whole(&*repo, parent.as_deref(), sha)?;
    build_session_from_changes(
        langs,
        ReviewSource::Commit {
            workdir: workdir.clone(),
            sha: sha.to_string(),
        },
        &workdir,
        changes,
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
    let changes = changed_or_whole(&*repo, Some(from), to)?;
    build_session_from_changes(
        langs,
        ReviewSource::CommitRange {
            workdir: workdir.clone(),
            from: from.to_string(),
            to: to.to_string(),
        },
        &workdir,
        changes,
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

/// The files `new` changed against `base`, falling back to what `new` holds
/// outright when `base` cannot be read.
///
/// A base the repository cannot produce is treated as no base at all, so the
/// commit still shows as whole-file additions rather than as nothing. The scans
/// have always been this forgiving, having read the base tree with a default on
/// failure.
pub(super) fn changed_or_whole(
    repo: &dyn GitRepo,
    base: Option<&str>,
    new: &str,
) -> Option<Vec<(std::path::PathBuf, String, String)>> {
    repo.changed_contents(base, new)
        .or_else(|| repo.changed_contents(None, new))
}

/// Common builder used by the commit scans, over the changed files a repo
/// reported as `(repo-relative path, base, buffer)`.
///
/// A pair whose two sides are equal is skipped, so a caller that reports a
/// path without a content change contributes nothing.
///
/// Takes the language registry directly rather than `&Stoat` so it runs
/// inside the off-loop scan closures, including the commit-preview build in
/// the sibling `commits` module.
pub(super) fn build_session_from_changes(
    langs: &LanguageRegistry,
    source: ReviewSource,
    workdir: &Path,
    changes: Vec<(std::path::PathBuf, String, String)>,
) -> Option<ReviewSession> {
    if changes.is_empty() {
        return None;
    }
    let mut session = ReviewSession::new(source);
    let mut inputs: Vec<ReviewFileInput> = Vec::new();
    for (rel, base, buffer) in changes {
        if base == buffer {
            continue;
        }
        let abs = workdir.join(&rel);
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

/// Bake tree-sitter spans for both sides of every file the session holds.
///
/// A preview paints the same token colors the editor does, so the spans have
/// to exist before paint. They are resolved here, on the blocking pool that
/// builds the session, rather than at paint time on the UI thread.
///
/// The memoized pipeline means an unchanged text parses once and resolves once
/// per theme, so a session over files the last preview already read costs hash
/// lookups. A file with no language is skipped and keeps `None`, which paints
/// untokenized, matching a syntax-off editor.
///
/// The styles bake against the theme in force here, so whoever switches themes
/// drops the sessions holding them.
pub(super) fn attach_preview_highlights(
    session: &mut ReviewSession,
    styles: &SyntaxStyles,
    cache: &BaseHighlightCache,
) {
    for file in &mut session.doc.files {
        let Some(language) = file.language.clone() else {
            continue;
        };
        file.base_highlights = Some(compute_base_highlights(
            &file.base_text,
            &language,
            styles,
            cache,
        ));
        file.buffer_highlights = Some(compute_base_highlights(
            &file.buffer_text,
            &language,
            styles,
            cache,
        ));
    }
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
        for file in &session.doc.files {
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

    // A rebase-fallback session names the commit it is showing. When the view
    // leaves that fallback (the tree went dirty, or the rebase finished) the
    // badge is cleared so it never lingers, but only then, so unrelated review
    // badges (an apply or stage result) survive an ordinary refresh.
    let old_was_rebase_fallback =
        stoat.active_workspace().review.as_ref().is_some_and(|old| {
            old.auto_source && matches!(old.source, ReviewSource::Commit { .. })
        });
    let rebase_badge = match &session.source {
        ReviewSource::Commit { sha, .. } if session.auto_source => Some(format!(
            "rebase: showing {}",
            sha.chars().take(7).collect::<String>()
        )),
        _ => None,
    };
    let clear_rebase_badge = old_was_rebase_fallback && rebase_badge.is_none();

    stoat.active_workspace_mut().review = Some(session);
    render_review_editor(stoat);

    if let Some(label) = rebase_badge {
        emit_review_info_badge(stoat, &label);
    } else if clear_rebase_badge {
        stoat
            .active_workspace_mut()
            .badges
            .remove_by_source(crate::badge::BadgeSource::Review);
    }
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
    let watching = stoat.settings.review_follow != Some(false);
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.as_ref() else {
        return;
    };

    let mut view = ReviewViewState::from_session(session);
    view.watching = watching;
    let blocks = build_review_blocks(session, &view);
    // The buffer mirrors the new (right) side of each row so the cursor and
    // motions have real text to move over. Row indices, blocks, and
    // classify_row stay aligned because there is still one line per row.
    let placeholder = view
        .rows
        .iter()
        .map(|row| row.right_text())
        .collect::<Vec<_>>()
        .join("\n");

    let (buffer_id, buffer) = ws.buffers.new_scratch();
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.edit(0..0, &placeholder);
        guard.mark_clean();
    }

    let mut editor = EditorState::new(buffer_id, buffer, executor, ws.redraw_notify.clone());
    editor.display_map.insert_blocks(blocks);
    editor.review_view = Some(Arc::new(view));

    let new_editor_id = ws.editors.insert(editor);
    let prev_view_editor = ws.review.as_ref().and_then(|s| s.view_editor);
    if let Some(session) = ws.review.as_mut() {
        session.view_editor = Some(new_editor_id);
        session.toggled_off = false;
        session.stashed_display_row = None;
    }

    let focused = ws.panes.focus();
    let old = match ws.panes.pane(focused).view {
        View::Editor(eid) => Some(eid),
        _ => None,
    };
    ws.panes.pane_mut(focused).view = View::Editor(new_editor_id);

    // Drop the pane's previous editor and, when a refresh rebuilt the view
    // while the diff was toggled off, the editor that had been parked off-
    // screen. Both are unreferenced now. The gc keeps split-shared editors.
    for stale in [old, prev_view_editor].into_iter().flatten() {
        if stale != new_editor_id {
            crate::action_handlers::gc_editor_if_unreferenced(ws, stale);
        }
    }
}

/// Mirror this session's freshly-extracted hunks into the diff cache so
/// a `stoat diff` CLI invocation that hashes the same `(base, buffer,
/// language)` tuple gets a cache hit instead of recomputing.
fn populate_diff_cache(stoat: &Stoat, session: &ReviewSession) {
    populate_diff_cache_from(&stoat.diff_cache, session, &AtomicBool::new(false));
}

/// Write each of `session`'s files' hunks into `cache` move-aware, so a later
/// review open serves them without re-diffing.
///
/// Locks the cache once per file rather than for the whole session, and checks
/// `cancel` between files, so the background warm ([`crate::diff_warm`]) can be
/// superseded mid-write without blocking a real scan or leaving the lock held.
pub(crate) fn populate_diff_cache_from(
    cache: &Mutex<DiffCache>,
    session: &ReviewSession,
    cancel: &AtomicBool,
) {
    for file in &session.doc.files {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let hunks: Vec<ReviewHunk> = file
            .chunks
            .iter()
            .filter_map(|id| session.doc.chunks.get(id).map(|c| c.hunk.clone()))
            .collect();
        let key = diff_cache_key(&file.base_text, &file.buffer_text, file.language.as_ref());
        cache
            .lock()
            .expect("diff_cache poisoned")
            .insert(key, Arc::new(hunks), true);
    }
}

fn build_review_blocks(session: &ReviewSession, view: &ReviewViewState) -> Vec<BlockProperties> {
    let mut blocks: Vec<BlockProperties> = Vec::with_capacity(view.chunk_row_ranges.len());
    for (chunk_id, range) in &view.chunk_row_ranges {
        let Some(chunk) = session.doc.chunks.get(chunk_id) else {
            continue;
        };
        let Some(file) = session.doc.files.get(chunk.file_index) else {
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
            placement: BlockPlacement::Above(range.start),
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
        badge::BadgeSource,
        debounce::{self, REVIEW_EXTERNAL_EDIT_DEBOUNCE},
        diff_cache::DiffCacheKey,
        editor_state::ScrollGlide,
        host::FsEventKind,
        review_session::ChunkStatus,
        test_harness::TestHarness,
        workspace::diff::DiffBase,
    };
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    /// The text of the buffer open at `path`, which lets a test read a proposal
    /// that landed in a file the pane no longer shows.
    fn buffer_text_at(h: &TestHarness, path: &str) -> Option<String> {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(Path::new(path))?;
        let shared = ws.buffers.get(id)?;
        let text = shared.read().expect("buffer poisoned").rope().to_string();
        Some(text)
    }

    /// The base texts a [`DiffBase::Memory`] override carries, by path.
    fn memory_base(h: &TestHarness) -> Vec<(String, String)> {
        let Some(DiffBase::Memory { files }) = h.stoat.active_workspace().diff_base() else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = files
            .iter()
            .map(|(path, text)| (path.display().to_string(), text.to_string()))
            .collect();
        out.sort();
        out
    }

    /// A proposal lands as an unsaved edit on the real file's buffer rather
    /// than in a screen of its own, so the editor's own verbs accept and reject
    /// it.
    #[test]
    fn agent_edits_land_as_unsaved_buffer_edits() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_fs().insert_file("/work/a.rs", b"old\n");
        h.fake_fs().insert_file("/work/b.rs", b"stale\n");

        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n"), ("b.rs", "stale\n", "fresh\n")]);

        assert_eq!(
            (
                buffer_text_at(&h, "/work/a.rs"),
                buffer_text_at(&h, "/work/b.rs")
            ),
            (Some("new\n".to_string()), Some("fresh\n".to_string())),
            "both proposals are sitting in their files, unsaved",
        );
    }

    /// The base travels with the proposals, since no revision holds what the
    /// agent read. It is keyed by absolute path, which is what the diff reads
    /// back.
    #[test]
    fn agent_edits_install_their_own_base() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_fs().insert_file("/work/a.rs", b"old\n");

        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n")]);

        assert_eq!(
            memory_base(&h),
            vec![("/work/a.rs".to_string(), "old\n".to_string())],
            "the agent's own base is what the proposal diffs against",
        );
    }

    /// The landing matches the commit openers. The first changed file opens
    /// with the latch armed, and a badge names the two verbs.
    #[test]
    fn agent_edits_land_on_the_first_file_with_the_latch_armed() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_fs().insert_file("/work/a.rs", b"old\n");
        h.fake_fs().insert_file("/work/b.rs", b"stale\n");

        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n"), ("b.rs", "stale\n", "fresh\n")]);

        let panes = &h.stoat.active_workspace().panes;
        assert_eq!(
            (
                crate::test_harness::editor::focused_buffer_path(&h.stoat),
                panes.pane(panes.focus()).diff_mode,
                h.stoat.pending_message.as_deref(),
            ),
            (
                PathBuf::from("/work/a.rs"),
                true,
                Some("agent edits: save accepts, :reload rejects"),
            ),
            "the first proposal is on screen, latched, with the verbs named",
        );
    }

    /// Each proposal is one edit, so one undo backs one file out. That is the
    /// fine-grained reject, and it costs no verb of its own.
    #[test]
    fn one_undo_rejects_one_file() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_fs().insert_file("/work/a.rs", b"old\n");

        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);

        assert_eq!(
            buffer_text_at(&h, "/work/a.rs"),
            Some("old\n".to_string()),
            "the file is back to what the agent read",
        );
    }

    /// A proposal for a file that does not exist yet opens empty and reads as a
    /// whole-file addition, which is what its empty base says it is.
    #[test]
    fn a_proposed_new_file_opens_against_an_empty_base() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();

        h.open_agent_edit_review(&[("new.rs", "", "added\n")]);

        assert_eq!(
            (buffer_text_at(&h, "/work/new.rs"), memory_base(&h)),
            (
                Some("added\n".to_string()),
                vec![("/work/new.rs".to_string(), String::new())]
            ),
            "the proposal is in a buffer with nothing behind it",
        );
    }

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
        let (hunks, _move_aware) = guard.lookup(&key).expect("cache hit after install");
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
                editor.scroll_glide = ScrollGlide::Wheel;
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
                editor.scroll_glide = ScrollGlide::None;
            }
            sync_review_view_and_scroll(ws, Some(editor_id), Some(chunk));
        }
        assert_eq!(
            h.editor_scroll_row(editor_id),
            target,
            "a refresh at rest scrolls to the current chunk",
        );
    }

    /// The 1-based new-side line and 0-based buffer row of the review
    /// editor's text cursor.
    fn review_cursor_row(h: &mut TestHarness) -> u32 {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let bs = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        bs.rope().offset_to_point(bs.resolve_anchor(&head)).row
    }

    /// A repo where `b.rs` matches HEAD and differs from commit `base0`, so it
    /// is changed against the revision and against nothing else.
    fn diff_rev_harness() -> (TestHarness, PathBuf) {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/repo");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        {
            let mut builder = h.fake_git().add_repo(&workdir).with_fs(h.fake_fs());
            builder.head_file("b.rs", "fn b() {}\nfn b2() {}\n");
            builder.commit("base0", &[("b.rs", "fn b() {}\n")]);
        }
        h.fake_fs()
            .insert_file(workdir.join("b.rs"), b"fn b() {}\nfn b2() {}\n");
        h.stoat.set_diff_warm_auto(true);
        (h, workdir)
    }

    /// What the three pieces of `:diff <rev>` state currently read: the base
    /// every buffer diffs against, whether the pane is latched into the diff
    /// layout, and which file is open.
    fn diff_state(h: &TestHarness) -> (Option<String>, bool, Option<PathBuf>) {
        let base = match h.stoat.active_workspace().diff_base() {
            Some(DiffBase::Rev { sha }) => sha.clone(),
            _ => None,
        };
        let panes = &h.stoat.active_workspace().panes;
        let latched = panes.pane(panes.focus()).diff_mode;
        let path = h
            .stoat
            .active_workspace()
            .buffers
            .path_for(h.stoat.focused_editor_ids().expect("editor").1)
            .map(Path::to_path_buf);
        (base, latched, path)
    }

    /// A revision points the whole workspace at that commit and opens what
    /// changed since it. Against HEAD `b.rs` is untouched, so nothing but the
    /// revision could have brought the reader here.
    #[test]
    fn diff_with_a_rev_installs_the_base_and_opens_the_changed_file() {
        let (mut h, workdir) = diff_rev_harness();

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::Diff {
                rev: Some("base0".to_string()),
            },
        );
        h.settle();

        assert_eq!(
            diff_state(&h),
            (Some("base0".to_string()), true, Some(workdir.join("b.rs")),),
        );
    }

    /// The bare command closes the diff, and a base a revision installed goes
    /// with it. Leaving it behind would have every later diff silently measured
    /// against a commit the reader thought they had closed.
    #[test]
    fn a_bare_diff_clears_the_base_a_rev_installed() {
        let (mut h, _) = diff_rev_harness();
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::Diff {
                rev: Some("base0".to_string()),
            },
        );
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });
        h.settle();

        let (base, latched, _) = diff_state(&h);
        assert_eq!((base, latched), (None, false));
    }

    /// Naming a revision asks to look at it, so running it again re-targets the
    /// view rather than closing it. Only the bare command closes, which is what
    /// keeps `:diff <rev>` from being a toggle whose meaning depends on what
    /// the reader last did.
    #[test]
    fn a_repeated_diff_rev_keeps_the_view_open() {
        let (mut h, workdir) = diff_rev_harness();
        let open = |h: &mut TestHarness| {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::Diff {
                    rev: Some("base0".to_string()),
                },
            );
            h.settle();
        };

        open(&mut h);
        open(&mut h);

        assert_eq!(
            diff_state(&h),
            (Some("base0".to_string()), true, Some(workdir.join("b.rs")),),
            "the second run lands on the same view rather than closing it",
        );
    }

    /// The base holds the resolved commit, not the text the reader typed. A
    /// revspec is a name for a commit at one moment: `main` moves, a prefix is
    /// not a sha, and either would leave the base naming something that stops
    /// meaning the commit it was chosen for.
    #[test]
    fn diff_stores_the_resolved_sha_rather_than_the_revspec() {
        let (mut h, _) = diff_rev_harness();

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::Diff {
                rev: Some("base".to_string()),
            },
        );
        h.settle();

        let (base, _, _) = diff_state(&h);
        assert_eq!(
            base.as_deref(),
            Some("base0"),
            "the prefix resolved to the commit it names",
        );
    }

    /// A revision the repo cannot resolve changes nothing. Half-applying it
    /// would leave the reader in a diff against a base they never named.
    #[test]
    fn an_unknown_rev_badges_and_changes_nothing() {
        let (mut h, _) = diff_rev_harness();
        let before = diff_state(&h);

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::Diff {
                rev: Some("nosuch".to_string()),
            },
        );
        h.settle();

        assert_eq!(diff_state(&h), before, "no state moved");
        let badge = h
            .stoat
            .active_workspace()
            .badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| h.stoat.active_workspace().badges.get(id))
            .map(|badge| badge.label.clone());
        assert_eq!(
            badge.as_deref(),
            Some("unknown revision"),
            "the refusal is reported rather than silent",
        );
    }

    #[test]
    fn toggle_diff_off_opens_the_real_file_at_the_cursor_line() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);
        h.fake_fs()
            .insert_file(PathBuf::from("a.rs"), b"a\nb\nX\nd\n");
        h.settle();

        // Put the review cursor on the changed row (new-side line 3).
        let review_editor_id = h.with_review(|s| s.view_editor).expect("review editor");
        {
            let ws = h.stoat.active_workspace_mut();
            let editor = ws.editors.get_mut(review_editor_id).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 2);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDiff);

        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "toggling off leaves review mode"
        );
        let (is_review, text) = {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            (
                editor.review_view.is_some(),
                snapshot.buffer_snapshot().rope().to_string(),
            )
        };
        assert!(!is_review, "the pane shows a plain editor, not the diff");
        assert_eq!(text, "a\nb\nX\nd\n", "the real working-tree file is shown");
        assert_eq!(
            review_cursor_row(&mut h),
            2,
            "cursor lands on new-side line 3 (row 2)",
        );
        assert!(h.with_review(|s| s.toggled_off), "the session is parked");
        assert_eq!(
            h.with_review(|s| s.view_editor),
            Some(review_editor_id),
            "the parked review editor is kept alive",
        );
    }

    #[test]
    fn diff_recomputes_a_map_staled_by_an_external_git_mutation() {
        let mut h = TestHarness::with_size(80, 14);
        // a.txt matches HEAD, so its first diff map holds no hunks at all --
        // exactly the state a buffer is left in when it was clean before an
        // external rebase moved HEAD out from under it.
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .head_file("a.txt", "a\nb\nc\n");
        h.fake_fs()
            .insert_file(PathBuf::from("/repo/a.txt"), b"a\nb\nc\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        h.fake_git()
            .add_repo("/repo")
            .head_file("a.txt", "a\nOLD\nc\n");
        h.stoat.active_workspace_mut().invalidate_all_diffs();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });

        assert_eq!(
            review_cursor_row(&mut h),
            1,
            "the stale empty map is recomputed and the cursor lands on the new first hunk",
        );
    }

    #[test]
    fn toggle_diff_back_restores_the_diff_with_staging_intact() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);
        h.settle();

        let chunk = h.current_review_chunk_id();
        h.set_review_status(0, ChunkStatus::Staged);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDiff);
        assert_eq!(h.stoat.focused_mode(), "normal");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleDiff);

        assert_eq!(
            h.stoat.current_view(),
            Some("review"),
            "toggling back re-enters the diff screen"
        );
        assert!(!h.with_review(|s| s.toggled_off), "the session is unparked");
        let is_review = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .review_view
            .is_some();
        assert!(is_review, "the diff is back in the focused pane");
        assert_eq!(
            h.chunk_status(chunk),
            ChunkStatus::Staged,
            "the staged decision survived the round trip",
        );
        assert_eq!(
            h.with_review(|s| s.cursor.current),
            Some(chunk),
            "the review cursor lands on the chunk the file cursor sat in",
        );
    }

    /// Open a two-file review where `migrated` moves from a.rs's base to
    /// b.rs's rhs, producing a cross-file move on both files.
    fn open_cross_file_move_review(h: &mut TestHarness) {
        let a_base = "fn migrated() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n}\n\nfn stays_a() {\n    call_a();\n}\n";
        let a_rhs = "fn stays_a() {\n    call_a();\n}\n";
        let b_base = "fn stays_b() {\n    call_b();\n}\n";
        let b_rhs = "fn stays_b() {\n    call_b();\n}\n\nfn migrated() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n}\n";
        h.open_review_from_texts(&[("a.rs", a_base, a_rhs), ("b.rs", b_base, b_rhs)]);
    }

    /// Move the review cursor to `buffer_row`.
    fn set_review_cursor(h: &mut TestHarness, buffer_row: u32) {
        let editor_id = h.with_review(|s| s.view_editor).expect("review editor");
        let ws = h.stoat.active_workspace_mut();
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, buffer_row);
    }

    /// The buffer row of the first row whose right (`want_right`) or left side
    /// carries move provenance.
    fn moved_row(h: &TestHarness, want_right: bool) -> u32 {
        use crate::review::ReviewRow;
        let editor_id = h.with_review(|s| s.view_editor).expect("review editor");
        let ws = h.stoat.active_workspace();
        let view = ws
            .editors
            .get(editor_id)
            .expect("editor")
            .review_view
            .as_ref()
            .expect("review view");
        view.rows
            .iter()
            .position(|r| {
                let side = match (want_right, r) {
                    (true, ReviewRow::Changed { right: Some(s), .. }) => Some(s),
                    (false, ReviewRow::Changed { left: Some(s), .. }) => Some(s),
                    _ => None,
                };
                side.is_some_and(|s| s.move_provenance.is_some())
            })
            .expect("a moved row") as u32
    }

    /// The file index of the chunk currently under the review cursor.
    fn current_chunk_file(h: &TestHarness) -> usize {
        h.with_review(|s| {
            let id = s.cursor.current.expect("current chunk");
            s.doc.chunks.get(&id).expect("chunk").file_index
        })
    }

    #[test]
    fn move_jump_source_and_target_round_trip() {
        let mut h = TestHarness::with_size(120, 32);
        open_cross_file_move_review(&mut h);

        // Cursor on b.rs's added `migrated` (right-side provenance -> a.rs).
        let dest_row = moved_row(&h, true);
        set_review_cursor(&mut h, dest_row);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveSource);
        assert_eq!(
            current_chunk_file(&h),
            0,
            "m lands on the source file (a.rs) chunk",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveTarget);
        assert_eq!(
            current_chunk_file(&h),
            1,
            "M jumps back to the destination file (b.rs) chunk",
        );
    }

    #[test]
    fn move_jump_to_missing_origin_emits_a_badge() {
        use crate::{
            badge::{BadgeSource, BadgeState},
            review::{MoveProvenance, ReviewRow},
        };

        let mut h = TestHarness::with_size(120, 32);
        open_cross_file_move_review(&mut h);

        // Repoint a moved row's provenance at a file not in this review.
        let ghost_row = {
            let editor_id = h.with_review(|s| s.view_editor).expect("review editor");
            let ws = h.stoat.active_workspace_mut();
            let view = Arc::make_mut(
                ws.editors
                    .get_mut(editor_id)
                    .expect("editor")
                    .review_view
                    .as_mut()
                    .expect("review view"),
            );
            let mut found = None;
            for (i, row) in view.rows.iter_mut().enumerate() {
                if let ReviewRow::Changed { right: Some(s), .. } = row
                    && s.move_provenance.is_some()
                {
                    s.move_provenance = Some(MoveProvenance {
                        rel_path: "ghost.rs".to_string(),
                        line: 0,
                        intra_file: false,
                    });
                    found = Some(i as u32);
                    break;
                }
            }
            found.expect("a moved row")
        };
        set_review_cursor(&mut h, ghost_row);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpToMoveSource);

        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(BadgeSource::Review)
            .expect("a badge is shown when the origin is not in the diff");
        let badge = ws.badges.get(badge_id).expect("badge");
        assert_eq!(badge.label, "move origin not in this diff");
        assert_eq!(badge.state, BadgeState::Active);
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
        debounce::drain_fs_watch_events(&mut h.stoat);
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
            .write(Path::new("a.txt"), b"Z\n")
            .expect("FakeFs::write");
        h.advance_clock(REVIEW_EXTERNAL_EDIT_DEBOUNCE);

        assert_eq!(
            h.with_review(|s| s.version),
            before_version,
            "unwatched write must not refresh the session",
        );
    }

    fn open_git_file_at_cursor(h: &mut TestHarness, row: u32) -> PathBuf {
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, row);
        workdir
    }

    /// Open a file with two modified lines four unchanged lines apart, cursor
    /// on `row`. The gap is under the old chunk extraction's 6-row merge
    /// window, so this is exactly the shape that used to stage both.
    fn open_two_hunk_file_at(h: &mut TestHarness, row: u32) -> PathBuf {
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(
            &workdir,
            &[(
                "a.rs",
                "a\nb\nc\nd\ne\nf\ng\nh\n",
                "a\nZ\nc\nd\ne\nf\nY\nh\n",
            )],
        );
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, row);
        workdir
    }

    /// The staged unit is the hunk the gutter draws. Two hunks close enough to
    /// share a context window still stage one at a time.
    #[test]
    fn staging_one_hunk_leaves_a_nearby_hunk_alone() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_two_hunk_file_at(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-b\n"),
            "removes the first base line: {patch}"
        );
        assert!(
            patch.contains("+Z\n"),
            "adds the first buffer line: {patch}"
        );
        assert!(
            !patch.contains("-g\n") && !patch.contains("+Y\n"),
            "and the second hunk's change is absent: {patch}"
        );
        // Context stops before the neighbor, so the patch never restates the
        // second hunk's row as unchanged base text and silently reverts it.
        assert!(
            patch.contains("@@ -1,5 +1,5 @@"),
            "and the context stops short of the second hunk: {patch}"
        );
    }

    /// Hunks closer together than the context width. Context has to stop at the
    /// neighbor rather than restating its changed row as unchanged base text,
    /// which would silently revert the second change on apply.
    #[test]
    fn context_stops_at_a_hunk_closer_than_the_context_width() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\ne\n", "a\nZ\nc\nd\nY\n")]);
        h.open_file(&workdir.join("a.rs"));
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            !patch.contains("\n e\n"),
            "the neighbor's base line is not carried as context: {patch}"
        );
        assert!(
            patch.contains("@@ -1,4 +1,4 @@"),
            "the patch stops one row short of the neighbor: {patch}"
        );
    }
    /// The other hunk of the same pair stages on its own too, so the split is
    /// the gutter's and not an artifact of which one comes first.
    #[test]
    fn staging_the_second_hunk_leaves_the_first_alone() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_two_hunk_file_at(&mut h, 6);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-g\n"),
            "removes the second base line: {patch}"
        );
        assert!(
            patch.contains("+Y\n"),
            "adds the second buffer line: {patch}"
        );
        assert!(
            !patch.contains("-b\n") && !patch.contains("+Z\n"),
            "and the first hunk's change is absent: {patch}"
        );
    }

    /// A cursor on a plain context row stages nothing. The gutter is empty
    /// there, so reaching for the nearest hunk would stage a unit the user
    /// cannot see under the cursor.
    #[test]
    fn a_cursor_off_every_hunk_stages_nothing() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_two_hunk_file_at(&mut h, 3);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        assert_eq!(
            h.fake_git().applied_patches(&workdir),
            Vec::<String>::new(),
            "nothing was staged"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no hunk under the cursor"),
            "and the message says why"
        );
    }

    /// A pure addition has no base lines of its own, so its base anchor comes
    /// from the lines before it. The header has to name that line, or the
    /// index apply places the hunk somewhere else.
    #[test]
    fn a_mid_file_addition_anchors_its_header_at_the_derived_base_line() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nb\nNEW\nc\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 2);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("+NEW\n"), "adds the new line: {patch}");
        assert!(
            patch.contains("@@ -1,4 +1,5 @@"),
            "the header counts from the file start with no base line consumed: {patch}"
        );
    }

    /// A deletion's rows are zero-width in the buffer, so the gutter marks it
    /// at its anchor row and the patch body carries only the removed lines.
    #[test]
    fn a_deletion_stages_from_its_anchor_row() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nGONE\nc\nd\n", "a\nb\nc\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 2);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("-GONE\n"), "removes the base line: {patch}");
        assert!(
            !patch
                .lines()
                .any(|line| line.starts_with('+') && !line.starts_with("+++")),
            "and adds nothing: {patch}"
        );
    }
    #[test]
    fn stage_hunk_applies_the_forward_patch_for_the_cursor_hunk() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("--- a/a.rs\n+++ b/a.rs\n"),
            "targets a.rs: {patch}"
        );
        assert!(patch.contains("-c\n"), "removes the base line: {patch}");
        assert!(patch.contains("+X\n"), "adds the buffer line: {patch}");
    }

    #[test]
    fn unstage_hunk_applies_the_reverse_patch() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-X\n"),
            "reverse removes the buffer line: {patch}"
        );
        assert!(
            patch.contains("+c\n"),
            "reverse restores the base line: {patch}"
        );
    }

    #[test]
    fn toggle_stage_hunk_stages_when_the_forward_patch_applies() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(
            patches.len(),
            1,
            "toggle stages via the forward patch: {patches:?}"
        );
        assert!(patches[0].contains("-c\n") && patches[0].contains("+X\n"));
    }

    #[test]
    fn stage_hunk_off_a_hunk_is_a_message_only_noop() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        assert!(
            h.fake_git().applied_patches(&workdir).is_empty(),
            "cursor off a hunk applies nothing"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no hunk under the cursor")
        );
    }

    #[test]
    fn stage_line_stages_only_the_cursor_line() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-b\n") && patch.contains("+B\n"),
            "stages the cursor line: {patch}"
        );
        assert!(
            !patch.contains("+C\n"),
            "leaves the other line unstaged: {patch}"
        );
        assert!(
            patch.contains(" c\n"),
            "the other line stays as context: {patch}"
        );
    }

    /// Staging moves the index, which is one of the two sides the gutter reads,
    /// so the map computed before it no longer describes what is on screen.
    #[test]
    fn stage_hunk_stales_the_diff_map() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        h.seed_current_diff_map("a\nb\nc\nd\n");
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "the map starts current, so staling it is the stage's doing",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "the index moved, so the map is owed a recompute",
        );
    }

    /// The line funnel writes the index too, and it is a second call site, so
    /// it stales the map on its own.
    #[test]
    fn stage_line_stales_the_diff_map() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        h.seed_current_diff_map("a\nb\nc\nd\n");
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "the map starts current, so staling it is the stage's doing",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "one staged line moves the index the same way a hunk does",
        );
    }

    /// A review base makes the checked-out commit the staged side, so the line
    /// keys reach it through the amend transport. The index is not on screen
    /// there, and an index write changes a thing the user never sees.
    #[test]
    fn stage_line_under_a_commit_base_amends_rather_than_writing_the_index() {
        let mut h = reviewing_a_commit("a\nP\nQ\nd\n");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        assert_eq!(
            (
                h.fake_git().applied_patches(&PathBuf::from("/work")),
                h.stoat.pending_message.as_deref()
            ),
            (vec![], Some("amended line into the commit")),
            "the line went into the commit and the index was left alone",
        );
    }

    /// Unstaging takes the same route. Both keys read the base before they read
    /// the cursor, so neither reaches the index while a commit is the staged
    /// side.
    #[test]
    fn unstage_line_under_a_commit_base_amends_rather_than_writing_the_index() {
        let mut h = reviewing_a_commit("a\nX\nY\nd\n");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);

        assert_eq!(
            (
                h.fake_git().applied_patches(&PathBuf::from("/work")),
                h.stoat.pending_message.as_deref()
            ),
            (vec![], Some("amended line out of the commit")),
            "the line came out of the commit and the index was left alone",
        );
    }

    /// A rebase stopped on an edit over commit `c1`, with `c0` as the review
    /// base, and the cursor on row 1 of `a.rs`. `working` is what the buffer
    /// holds, which is what separates a worktree edit from the commit's own
    /// change.
    fn reviewing_a_commit(working: &str) -> TestHarness {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .commit("c0", &[("a.rs", "a\nb\nc\nd\n")])
            .commit_with_parent("c1", "c0", &[("a.rs", "a\nX\nY\nd\n")])
            .head_file("a.rs", working);
        h.fake_fs().insert_file("/work/a.rs", working.as_bytes());
        {
            let ws = h.stoat.active_workspace_mut();
            ws.set_diff_base(Some(DiffBase::Rev {
                sha: Some("c0".to_string()),
            }));
            ws.rebase_active = Some(paused_rebase(&workdir));
        }
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);
        h
    }

    /// A base with nothing safe to rewrite under it gets the transport's own
    /// refusal rather than the line-granularity one, since the commit is out of
    /// reach for `s` and `u` as well.
    #[test]
    fn stage_line_reports_the_transport_refusal_below_the_tip() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.fake_git().add_repo(&workdir).commit("c1", &[]);
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::Rev {
                sha: Some("c1".to_string()),
            }));
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        assert_eq!(
            (
                h.fake_git().applied_patches(&workdir),
                h.stoat.pending_message.as_deref()
            ),
            (vec![], Some(crate::action_handlers::amend::REFUSED_BADGE)),
            "no walk and no edit stop, so the transport is closed to both key pairs",
        );
    }

    /// A rebase stopped on an edit, which is one of the two positions that make
    /// rewriting the checked-out commit safe.
    fn paused_rebase(workdir: &Path) -> crate::rebase::ActiveRebase {
        crate::rebase::ActiveRebase {
            workdir: workdir.to_path_buf(),
            onto: "c1".to_string(),
            remaining: std::collections::VecDeque::new(),
            current_head: "c1".to_string(),
            last_pick_sha: Some("c1".to_string()),
            last_message: None,
            pause: Some(crate::rebase::RebasePause::Edit {
                cherry_picked_commit: "c1".to_string(),
            }),
        }
    }

    /// The line stage resolves its hunk from bare extents now, so a second hunk
    /// close enough that the old chunk extraction would have merged the two is
    /// absent from the patch rather than riding along as context.
    #[test]
    fn stage_line_ignores_a_hunk_the_old_chunking_would_have_merged() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(
            &workdir,
            &[(
                "a.rs",
                "a\nb\nc\nd\ne\nf\ng\nh\n",
                "a\nZ\nc\nd\ne\nf\nY\nh\n",
            )],
        );
        h.open_file(&workdir.join("a.rs"));
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-b\n") && patch.contains("+Z\n"),
            "stages the cursor line: {patch}"
        );
        // Context that ran into the far hunk would carry its buffer text as an
        // unchanged line, telling git the index already holds the change.
        assert!(
            !patch.contains("\n Y\n"),
            "the far hunk's text is not carried as context: {patch}"
        );
        assert!(
            patch.contains("@@ -1,5 +1,5 @@"),
            "and the patch stops well short of it: {patch}"
        );
    }
    #[test]
    fn unstage_line_reverts_the_staged_line_to_head() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("-B\n"), "reverts the staged line: {patch}");
        assert!(patch.contains("+b\n"), "restores the HEAD line: {patch}");
    }

    #[test]
    fn toggle_stage_line_stages_an_unstaged_line() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(
            patches.len(),
            1,
            "toggle stages the unstaged line: {patches:?}"
        );
        assert!(patches[0].contains("-b\n") && patches[0].contains("+B\n"));
    }

    #[test]
    fn toggle_stage_line_unstages_a_staged_line() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(
            patches.len(),
            1,
            "toggle unstages the staged line: {patches:?}"
        );
        assert!(patches[0].contains("-B\n") && patches[0].contains("+b\n"));
    }

    #[test]
    fn stage_line_composes_against_the_updated_index() {
        // HEAD a/b/c/d; the index already stages line 1 (b -> B), and the
        // buffer additionally changes line 2 (c -> C). Staging line 2 diffs the
        // index (not HEAD), so the patch touches only line 2.
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(
            &workdir,
            &[("a.rs", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nC\nd\n")],
        );
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-c\n") && patch.contains("+C\n"),
            "stages line 2 against the index: {patch}"
        );
        assert!(
            !patch.contains("+B\n") && !patch.contains("-b\n"),
            "line 1 is already staged in the index and stays context, not re-staged: {patch}"
        );
    }

    #[test]
    fn unstage_line_maps_the_cursor_past_an_unstaged_insertion() {
        // HEAD a/b; the index stages line 1 (b -> B); the buffer inserts NEW
        // above unstaged, so the staged line sits at buffer row 2 but index row
        // 1. Unstaging must map back and revert only the index line.
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\n", "a\nB\n", "NEW\na\nB\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("-B\n"), "reverts the staged line: {patch}");
        assert!(patch.contains("+b\n"), "restores the HEAD line: {patch}");
        assert!(
            !patch.contains("NEW"),
            "the unstaged insertion is untouched: {patch}"
        );
    }

    #[test]
    fn stage_line_off_a_change_is_a_message_only_noop() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        assert!(
            h.fake_git().applied_patches(&workdir).is_empty(),
            "cursor off a change applies nothing"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no line change under the cursor")
        );
    }

    #[test]
    fn space_capital_g_s_stages_the_hunk_from_a_plain_editor() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        h.type_keys("space G s");

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "space G s stages the hunk: {patches:?}");
        assert!(
            patches[0].contains("-c\n") && patches[0].contains("+X\n"),
            "stages the cursor hunk: {}",
            patches[0]
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the one-shot git mode returns to normal after acting"
        );
    }
}
