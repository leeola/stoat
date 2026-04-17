use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    command_palette::CommandPalette,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, DisplayPoint, RenderBlock},
    editor_state::EditorState,
    host::FsHost,
    pane::{Axis, Direction, DockSide, DockVisibility, FocusTarget, View},
    review_session::{ReviewSession, ReviewSource, ReviewViewState},
    run::{OutputBlock, RunState},
};
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use std::{path::Path, sync::Arc};
use stoat_action::{
    Action, ActionKind, OpenFile, OpenReviewAgentEdits, OpenReviewCommit, OpenReviewCommitRange,
    Run,
};
use stoat_text::{next_word_end, next_word_start, prev_word_start, Bias, Selection, SelectionGoal};

pub fn dispatch(stoat: &mut Stoat, action: &dyn Action) -> UpdateEffect {
    match action.kind() {
        ActionKind::Quit => UpdateEffect::Quit,
        ActionKind::SplitRight => split_pane(stoat, Axis::Vertical),
        ActionKind::SplitDown => split_pane(stoat, Axis::Horizontal),
        ActionKind::FocusLeft => {
            focus_direction(stoat, Direction::Left);
            UpdateEffect::Redraw
        },
        ActionKind::FocusRight => {
            focus_direction(stoat, Direction::Right);
            UpdateEffect::Redraw
        },
        ActionKind::FocusUp => {
            stoat
                .active_workspace_mut()
                .panes
                .focus_direction(Direction::Up);
            UpdateEffect::Redraw
        },
        ActionKind::FocusDown => {
            stoat
                .active_workspace_mut()
                .panes
                .focus_direction(Direction::Down);
            UpdateEffect::Redraw
        },
        ActionKind::FocusNext => {
            stoat.active_workspace_mut().panes.focus_next();
            UpdateEffect::Redraw
        },
        ActionKind::FocusPrev => {
            stoat.active_workspace_mut().panes.focus_prev();
            UpdateEffect::Redraw
        },
        ActionKind::ClosePane => {
            let ws = stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            match ws.panes.pane(focused).view {
                View::Editor(id) => {
                    ws.panes.close(focused);
                    ws.editors.remove(id);
                },
                View::Run(id) => {
                    ws.panes.close(focused);
                    if let Some(mut state) = ws.runs.remove(id) {
                        if let Some(handle) = &mut state.shell_handle {
                            handle.kill();
                        }
                    }
                },
                View::Label(_) | View::Claude(_) => {
                    ws.panes.close(focused);
                },
            }
            UpdateEffect::Redraw
        },
        ActionKind::OpenFile => {
            let open = action
                .as_any()
                .downcast_ref::<OpenFile>()
                .expect("OpenFile action downcast");
            open_file(stoat, &open.path);
            UpdateEffect::Redraw
        },
        ActionKind::OpenCommandPalette => {
            stoat.command_palette = Some(CommandPalette::new());
            UpdateEffect::Redraw
        },
        ActionKind::OpenReview => {
            open_review(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::AddSelectionBelow => add_selection_below(stoat),
        ActionKind::MoveLeft => move_horizontal(stoat, -1),
        ActionKind::MoveRight => move_horizontal(stoat, 1),
        ActionKind::MoveUp => move_vertical(stoat, -1),
        ActionKind::MoveDown => move_vertical(stoat, 1),
        ActionKind::MoveNextWordStart => move_word(stoat, WordTarget::NextStart),
        ActionKind::MoveNextWordEnd => move_word(stoat, WordTarget::NextEnd),
        ActionKind::MovePrevWordStart => move_word(stoat, WordTarget::PrevStart),
        ActionKind::OpenRun => open_run(stoat),
        ActionKind::RunSubmit => run_submit(stoat),
        ActionKind::RunInterrupt => run_interrupt(stoat),
        ActionKind::Run => {
            let cmd = action
                .as_any()
                .downcast_ref::<Run>()
                .expect("Run action downcast");
            run_command(stoat, &cmd.command)
        },
        ActionKind::OpenClaude => open_claude(stoat),
        ActionKind::ClaudeSubmit => claude_submit(stoat),
        ActionKind::ClaudeToPane => claude_to_pane(stoat),
        ActionKind::ClaudeToDockLeft => claude_to_dock(stoat, DockSide::Left),
        ActionKind::ClaudeToDockRight => claude_to_dock(stoat, DockSide::Right),
        ActionKind::ToggleDockRight => toggle_dock(stoat, DockSide::Right),
        ActionKind::ToggleDockLeft => toggle_dock(stoat, DockSide::Left),
        ActionKind::JumpToMoveSource => move_nav(stoat, MoveNavigation::FirstSource),
        ActionKind::JumpToMoveTarget => move_nav(stoat, MoveNavigation::Target),
        ActionKind::JumpToNextMoveSource => move_nav(stoat, MoveNavigation::NextSource),
        ActionKind::JumpToPrevMoveSource => move_nav(stoat, MoveNavigation::PrevSource),
        ActionKind::QueryMoveRelationships => {
            // Scriptable surface: observes the move metadata under the
            // cursor but does not navigate. A future automation hook
            // will expose this via the action SDK; for now it resolves
            // and logs the relationship count so the action is
            // observable from tests.
            if let Some(summary) = current_move_summary(stoat) {
                tracing::info!(
                    sources = summary.source_count,
                    same_side_target = ?summary.target_line,
                    "move relationships under cursor"
                );
                UpdateEffect::Redraw
            } else {
                UpdateEffect::None
            }
        },
        ActionKind::ReviewNextChunk => review_step(stoat, ReviewStep::Next),
        ActionKind::ReviewPrevChunk => review_step(stoat, ReviewStep::Prev),
        ActionKind::ReviewStageChunk => review_mark(stoat, ReviewMark::Stage),
        ActionKind::ReviewUnstageChunk => review_mark(stoat, ReviewMark::Unstage),
        ActionKind::ReviewToggleStage => review_mark(stoat, ReviewMark::Toggle),
        ActionKind::ReviewSkipChunk => review_mark(stoat, ReviewMark::Skip),
        ActionKind::ReviewRefresh => review_refresh(stoat),
        ActionKind::ReviewApplyStaged => review_apply_staged(stoat),
        ActionKind::CloseReview => close_review(stoat),
        ActionKind::OpenReviewCommit => {
            let a = action
                .as_any()
                .downcast_ref::<OpenReviewCommit>()
                .expect("OpenReviewCommit action downcast");
            open_review_commit(stoat, &a.workdir, &a.sha);
            UpdateEffect::Redraw
        },
        ActionKind::OpenReviewCommitRange => {
            let a = action
                .as_any()
                .downcast_ref::<OpenReviewCommitRange>()
                .expect("OpenReviewCommitRange action downcast");
            open_review_commit_range(stoat, &a.workdir, &a.from, &a.to);
            UpdateEffect::Redraw
        },
        ActionKind::OpenReviewAgentEdits => {
            let a = action
                .as_any()
                .downcast_ref::<OpenReviewAgentEdits>()
                .expect("OpenReviewAgentEdits action downcast");
            open_review_agent_edits(stoat, &a.edits);
            UpdateEffect::Redraw
        },
    }
}

fn open_review_commit(stoat: &mut Stoat, workdir: &Path, sha: &str) {
    let Some(session) = scan_commit(stoat, workdir, sha) else {
        return;
    };
    install_review_session(stoat, session);
}

fn open_review_commit_range(stoat: &mut Stoat, workdir: &Path, from: &str, to: &str) {
    let Some(session) = scan_commit_range(stoat, workdir, from, to) else {
        return;
    };
    install_review_session(stoat, session);
}

fn open_review_agent_edits(stoat: &mut Stoat, edits: &[stoat_action::AgentEdit]) {
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
enum ReviewStep {
    Next,
    Prev,
}

#[derive(Copy, Clone, Debug)]
enum ReviewMark {
    Stage,
    Unstage,
    Toggle,
    Skip,
}

fn review_step(stoat: &mut Stoat, step: ReviewStep) -> UpdateEffect {
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

fn review_mark(stoat: &mut Stoat, mark: ReviewMark) -> UpdateEffect {
    use crate::{
        badge::{Anchor, Badge, BadgeSource, BadgeState},
        review_session::ChunkStatus,
    };

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
    let complete = session.is_complete();
    let progress = session.progress();
    sync_review_view_and_scroll(ws, editor_id, None);

    let existing = ws.badges.find_by_source(BadgeSource::Review);
    if complete {
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
    } else if let Some(bid) = existing {
        ws.badges.remove(bid);
    }

    UpdateEffect::Redraw
}

/// Refresh the editor's review view cache from the session and, if a chunk
/// is supplied, scroll so that chunk sits near the top of the pane. Split
/// borrow of `ws.editors` and `ws.review` is done here so callers can drop
/// their `&mut ws.review` borrow before invoking.
fn sync_review_view_and_scroll(
    ws: &mut crate::workspace::Workspace,
    editor_id: Option<crate::editor_state::EditorId>,
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

fn review_apply_staged(stoat: &mut Stoat) -> UpdateEffect {
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
            Err(GitApplyError::Backend(msg)) => failures.push(msg),
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

fn review_refresh(stoat: &mut Stoat) -> UpdateEffect {
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
        let old = ws.review.as_ref().unwrap();
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

/// Re-scan the underlying source of a review session. Returns `None` when
/// the source has no re-scannable state (currently `InMemory`) or when
/// the scan produced no hunks.
fn rescan_source(stoat: &Stoat, source: &ReviewSource) -> Option<ReviewSession> {
    match source {
        ReviewSource::WorkingTree { workdir } => scan_working_tree(stoat, workdir),
        ReviewSource::Commit { workdir, sha } => scan_commit(stoat, workdir, sha),
        ReviewSource::CommitRange { workdir, from, to } => {
            scan_commit_range(stoat, workdir, from, to)
        },
        ReviewSource::AgentEdits { edits } => scan_agent_edits(stoat, edits.as_ref()),
        ReviewSource::InMemory { .. } => None,
    }
}

fn close_review(stoat: &mut Stoat) -> UpdateEffect {
    use crate::badge::BadgeSource;

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.review.take() else {
        return UpdateEffect::None;
    };
    ws.badges.remove_by_source(BadgeSource::Review);
    let Some(review_editor_id) = session.view_editor else {
        stoat.mode = "normal".to_string();
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

    stoat.mode = "normal".to_string();
    UpdateEffect::Redraw
}

#[derive(Copy, Clone, Debug)]
enum MoveNavigation {
    FirstSource,
    NextSource,
    PrevSource,
    Target,
}

/// Resolved move-provenance summary for the hunk under the editor's
/// cursor. Used by the move-navigation action handlers.
struct MoveSummary {
    /// Line the hunk starts on in the buffer.
    hunk_line: u32,
    /// Candidate source line numbers, zero or more.
    source_lines: Vec<u32>,
    /// If the hunk is the LHS side of a move, the paired RHS target line.
    target_line: Option<u32>,
    /// Number of candidate sources (>1 = ambiguous move).
    source_count: usize,
}

fn current_move_summary(stoat: &mut Stoat) -> Option<MoveSummary> {
    let editor = focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    // Derive cursor row via the display snapshot, which already knows
    // how to convert an Anchor to a buffer point.
    let anchor = editor.selections.newest_anchor().start;
    let buffer_snapshot = snapshot.buffer_snapshot();
    let offset = buffer_snapshot.resolve_anchor(&anchor);
    let cursor_line = buffer_snapshot.rope().offset_to_point(offset).row;

    if snapshot.line_diff_status(cursor_line) != crate::host::DiffStatus::Moved {
        return None;
    }
    let detail = snapshot.token_detail_for_line(cursor_line)?;
    let metadata = detail
        .buffer_spans
        .iter()
        .chain(detail.base_spans.iter())
        .find_map(|s| s.move_metadata.clone())?;
    let source_lines: Vec<u32> = metadata
        .sources
        .iter()
        .map(|s| s.line_range.start)
        .collect();
    let target_line = if detail.buffer_spans.is_empty() && !detail.base_spans.is_empty() {
        metadata.sources.first().map(|s| s.line_range.start)
    } else {
        None
    };
    Some(MoveSummary {
        hunk_line: cursor_line,
        source_count: metadata.sources.len(),
        source_lines,
        target_line,
    })
}

fn move_nav(stoat: &mut Stoat, nav: MoveNavigation) -> UpdateEffect {
    let Some(summary) = current_move_summary(stoat) else {
        return UpdateEffect::None;
    };
    if summary.source_lines.is_empty() && summary.target_line.is_none() {
        return UpdateEffect::None;
    }

    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };

    let target_row = match nav {
        MoveNavigation::FirstSource => {
            editor.move_source_cursor = Some((summary.hunk_line, 0));
            summary.source_lines.first().copied()
        },
        MoveNavigation::NextSource => {
            let idx = match editor.move_source_cursor {
                Some((line, i)) if line == summary.hunk_line => {
                    (i + 1) % summary.source_lines.len().max(1)
                },
                _ => 0,
            };
            editor.move_source_cursor = Some((summary.hunk_line, idx));
            summary.source_lines.get(idx).copied()
        },
        MoveNavigation::PrevSource => {
            let len = summary.source_lines.len().max(1);
            let idx = match editor.move_source_cursor {
                Some((line, i)) if line == summary.hunk_line => (i + len - 1) % len,
                _ => len.saturating_sub(1),
            };
            editor.move_source_cursor = Some((summary.hunk_line, idx));
            summary.source_lines.get(idx).copied()
        },
        MoveNavigation::Target => summary.target_line,
    };

    let Some(row) = target_row else {
        return UpdateEffect::None;
    };
    // Move the cursor to the resolved row. Full cross-file navigation
    // (opening a different buffer when MoveSource.buffer is Some)
    // lands in Phase 9 alongside the workspace-wide move index.
    set_cursor_row(editor, row);
    UpdateEffect::Redraw
}

fn set_cursor_row(editor: &mut EditorState, row: u32) {
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let point = stoat_text::Point::new(row, 0);
    let offset = rope.point_to_offset(point);
    let anchor = buffer_snapshot.anchor_at(offset, Bias::Left);
    editor.selections = crate::selection::SelectionsCollection::new();
    editor
        .selections
        .insert_cursor(anchor, SelectionGoal::None, &buffer_snapshot);
    editor.scroll_row = row.saturating_sub(2);
}

#[derive(Copy, Clone, Debug)]
enum WordTarget {
    NextStart,
    NextEnd,
    PrevStart,
}

fn focused_editor_mut(stoat: &mut Stoat) -> Option<&mut EditorState> {
    let ws = stoat.active_workspace_mut();
    let view = match ws.focus {
        FocusTarget::SplitPane(_) => {
            let focused = ws.panes.focus();
            ws.panes.pane(focused).view.clone()
        },
        FocusTarget::Dock(dock_id) => match ws.docks.get(dock_id) {
            Some(dock) => dock.view.clone(),
            None => return None,
        },
    };
    match view {
        View::Editor(id) => ws.editors.get_mut(id),
        View::Claude(session_id) => {
            let editor_id = ws.chats.get(&session_id)?.input_editor_id;
            ws.editors.get_mut(editor_id)
        },
        _ => None,
    }
}

fn add_selection_below(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();

    let source = editor.selections.newest_anchor().clone();
    let source_head = source.head();
    let source_point = buffer_snapshot.point_for_anchor(&source_head);
    let source_display = display_snapshot.buffer_to_display(source_point);

    let goal_col = match source.goal {
        SelectionGoal::Column(c) => c,
        SelectionGoal::None => source_display.column,
    };

    let max_row = display_snapshot.max_point().row;
    let mut row = source_display.row;
    let target = loop {
        if row >= max_row {
            return UpdateEffect::None;
        }
        row += 1;
        let clamped_col = goal_col.min(display_snapshot.line_len(row));
        let raw = DisplayPoint::new(row, clamped_col);
        let clipped = display_snapshot.clip_point(raw, Bias::Left);
        let Some(buffer_pt) = display_snapshot.display_to_buffer(clipped) else {
            continue;
        };
        let offset = buffer_snapshot.rope().point_to_offset(buffer_pt);
        let anchor = buffer_snapshot.anchor_at(offset, Bias::Right);
        break anchor;
    };

    editor
        .selections
        .insert_cursor(target, SelectionGoal::Column(goal_col), buffer_snapshot);
    UpdateEffect::Redraw
}

fn move_horizontal(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        let new_offset = if delta > 0 {
            match rope.chars_at(head_offset).next() {
                Some(ch) => head_offset + ch.len_utf8(),
                None => head_offset,
            }
        } else {
            match rope.reversed_chars_at(head_offset).next() {
                Some(ch) => head_offset - ch.len_utf8(),
                None => head_offset,
            }
        };
        if new_offset == head_offset {
            return sel.clone();
        }
        let anchor = buffer_snapshot.anchor_at(new_offset, Bias::Right);
        let mut new = sel.clone();
        new.collapse_to(anchor, SelectionGoal::None);
        new
    });
    UpdateEffect::Redraw
}

fn move_vertical(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let max_row = display_snapshot.max_point().row;
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_anchor = sel.head();
        let head_point = buffer_snapshot.point_for_anchor(&head_anchor);
        let head_display = display_snapshot.buffer_to_display(head_point);
        let goal_col = match sel.goal {
            SelectionGoal::Column(c) => c,
            SelectionGoal::None => head_display.column,
        };
        let new_row_i = head_display.row as i64 + delta as i64;
        if new_row_i < 0 || new_row_i > max_row as i64 {
            return sel.clone();
        }
        let new_row = new_row_i as u32;
        let clamped_col = goal_col.min(display_snapshot.line_len(new_row));
        let raw = DisplayPoint::new(new_row, clamped_col);
        let clipped = display_snapshot.clip_point(raw, Bias::Left);
        let Some(buffer_pt) = display_snapshot.display_to_buffer(clipped) else {
            return sel.clone();
        };
        let offset = buffer_snapshot.rope().point_to_offset(buffer_pt);
        let anchor = buffer_snapshot.anchor_at(offset, Bias::Right);
        let mut new = sel.clone();
        new.collapse_to(anchor, SelectionGoal::Column(goal_col));
        new
    });
    UpdateEffect::Redraw
}

fn move_word(stoat: &mut Stoat, target: WordTarget) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let display_snapshot = editor.display_map.snapshot();
    let buffer_snapshot = display_snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    editor.selections.transform(buffer_snapshot, |sel| {
        let head_offset = buffer_snapshot.resolve_anchor(&sel.head());
        let target_offset = match target {
            WordTarget::NextStart => next_word_start(rope, head_offset),
            WordTarget::NextEnd => next_word_end(rope, head_offset),
            WordTarget::PrevStart => prev_word_start(rope, head_offset),
        };
        if target_offset == head_offset {
            return sel.clone();
        }
        if target_offset > head_offset {
            let end_offset = rope
                .reversed_chars_at(target_offset)
                .next()
                .map(|ch| target_offset - ch.len_utf8())
                .unwrap_or(target_offset);
            let tail_anchor = buffer_snapshot.anchor_at(head_offset, Bias::Right);
            let head_anchor = buffer_snapshot.anchor_at(end_offset, Bias::Right);
            Selection {
                id: sel.id,
                start: tail_anchor,
                end: head_anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }
        } else {
            let head_anchor = buffer_snapshot.anchor_at(target_offset, Bias::Right);
            let tail_offset = match rope.chars_at(head_offset).next() {
                Some(ch) => head_offset + ch.len_utf8(),
                None => head_offset,
            };
            let tail_anchor = buffer_snapshot.anchor_at(tail_offset, Bias::Right);
            Selection {
                id: sel.id,
                start: head_anchor,
                end: tail_anchor,
                reversed: true,
                goal: SelectionGoal::None,
            }
        }
    });
    UpdateEffect::Redraw
}

/// Read `path` through the supplied [`FsHost`] as a UTF-8 string.
fn read_string_via_host(fs: &dyn FsHost, path: &Path) -> std::io::Result<String> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn open_file(stoat: &mut Stoat, path: &Path) -> Option<BufferId> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let content = match read_string_via_host(&*stoat.fs_host, &absolute) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::error!("failed to read {}: {}", absolute.display(), e);
            return None;
        },
    };

    let lang = stoat.language_registry.for_path(&absolute);
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();

    let (buffer_id, buffer) = ws.buffers.open(&absolute, &content);
    if let Some(lang) = lang {
        ws.buffers.set_language(buffer_id, lang);
    }
    let new_editor_id = ws
        .editors
        .insert(EditorState::new(buffer_id, buffer, executor));

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

    Some(buffer_id)
}

fn open_review(stoat: &mut Stoat) {
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(session) = scan_working_tree(stoat, &git_root) else {
        return;
    };
    install_review_session(stoat, session);
}

/// Build a review session by scanning the git working tree rooted at
/// `git_root`. Returns `None` when the root is not a repository or has
/// no diff hunks. Shared by [`open_review`] and [`review_refresh`].
fn scan_working_tree(stoat: &Stoat, git_root: &Path) -> Option<ReviewSession> {
    let repo = match stoat.git_host.discover(git_root) {
        Some(r) => r,
        None => {
            tracing::warn!("open_review: not inside a git repository");
            return None;
        },
    };
    let workdir = repo.workdir()?;

    let changed = repo.changed_files();
    if changed.is_empty() {
        tracing::warn!("open_review: no changed files");
        return None;
    }

    let mut session = ReviewSession::new(ReviewSource::WorkingTree {
        workdir: workdir.clone(),
    });

    for file in &changed {
        let buffer_text = match read_string_via_host(&*stoat.fs_host, &file.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                tracing::warn!(
                    path = %file.path.display(),
                    error = %e,
                    "scan_working_tree: skip file",
                );
                continue;
            },
        };
        let base_text = repo.head_content(&file.path).unwrap_or_default();
        let lang = stoat.language_registry.for_path(&file.path);
        let rel_path = file
            .path
            .strip_prefix(&workdir)
            .unwrap_or(&file.path)
            .display()
            .to_string();
        session.add_file(
            file.path.clone(),
            rel_path,
            lang,
            Arc::new(base_text),
            Arc::new(buffer_text),
        );
    }

    if session.order.is_empty() {
        tracing::warn!("open_review: no diff hunks to display");
        return None;
    }

    Some(session)
}

/// Build a session from a single commit by diffing its tree against its
/// first parent (or the empty tree for a root commit). Returns `None`
/// when the repo or sha is unknown, or when no paths differ.
fn scan_commit(stoat: &Stoat, workdir: &Path, sha: &str) -> Option<ReviewSession> {
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
/// exclusive of `from` -- same as `git diff from..to`). Pairs each
/// path in either tree to form file-level base/buffer contents.
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
    for edit in edits {
        let lang = stoat.language_registry.for_path(&edit.path);
        let rel_path = edit.path.display().to_string();
        session.add_file(
            edit.path.clone(),
            rel_path,
            lang,
            edit.base_text.clone(),
            edit.proposed_text.clone(),
        );
    }
    if session.order.is_empty() {
        return None;
    }
    Some(session)
}

/// Common builder used by [`scan_commit`] / [`scan_commit_range`]. Walks
/// the union of paths across `base_tree` and `new_tree`, skipping any
/// pair whose base and buffer contents are equal.
fn build_session_from_trees(
    stoat: &Stoat,
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
    for rel in paths {
        let base = base_tree.get(rel).cloned().unwrap_or_default();
        let buffer = new_tree.get(rel).cloned().unwrap_or_default();
        if base == buffer {
            continue;
        }
        let abs = workdir.join(rel);
        let lang = stoat.language_registry.for_path(&abs);
        session.add_file(
            abs,
            rel.display().to_string(),
            lang,
            Arc::new(base),
            Arc::new(buffer),
        );
    }
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

fn split_pane(stoat: &mut Stoat, axis: Axis) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let new_pane_id = ws.panes.split(axis);
    if let View::Editor(old_editor_id) = ws.panes.pane(new_pane_id).view {
        if let Some(old_editor) = ws.editors.get(old_editor_id) {
            let buffer_id = old_editor.buffer_id;
            if let Some(buffer) = ws.buffers.get(buffer_id) {
                let new_editor_id = ws
                    .editors
                    .insert(EditorState::new(buffer_id, buffer, executor));
                ws.panes.pane_mut(new_pane_id).view = View::Editor(new_editor_id);
            }
        }
    }
    UpdateEffect::Redraw
}

fn open_run(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let cwd = ws.git_root.clone();
    let id = ws.runs.insert(RunState::new(cwd));
    let focused = ws.panes.focus();
    ws.panes.pane_mut(focused).view = View::Run(id);
    stoat.mode = "run".into();
    UpdateEffect::Redraw
}

fn run_submit(stoat: &mut Stoat) -> UpdateEffect {
    let pty_tx = stoat.pty_tx.clone();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let Some(run_state) = ws.runs.get_mut(id) else {
        return UpdateEffect::None;
    };
    let text = run_state.input.take();
    if text.is_empty() {
        return UpdateEffect::None;
    }

    run_state.history.push(text.clone());
    run_state.history_cursor = None;

    let pane_area = ws.panes.pane(focused).area;
    let width = pane_area.width.saturating_sub(2).max(20);
    run_state.blocks.push(OutputBlock::new(text.clone(), width));

    if let Some(handle) = &mut run_state.shell_handle {
        let sentinel = format!("__STOAT_{}__", run_state.blocks.len());
        handle.send_command(&text, &sentinel);
    } else if let Ok(handle) = crate::run::spawn_shell(&run_state.cwd, width, pty_tx, id) {
        let sentinel = format!("__STOAT_{}__", run_state.blocks.len());
        run_state.shell_handle = Some(handle);
        if let Some(h) = &mut run_state.shell_handle {
            h.send_command(&text, &sentinel);
        }
    }

    UpdateEffect::Redraw
}

fn run_interrupt(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let Some(run_state) = ws.runs.get_mut(id) else {
        return UpdateEffect::None;
    };
    if let Some(handle) = &mut run_state.shell_handle {
        handle.send_interrupt();
    }
    UpdateEffect::Redraw
}

fn run_command(stoat: &mut Stoat, command: &str) -> UpdateEffect {
    let pty_tx = stoat.pty_tx.clone();
    let ws = stoat.active_workspace();
    let cwd = ws.git_root.clone();
    let focused_area = ws.panes.pane(ws.panes.focus()).area;
    let width = focused_area.width.saturating_sub(8).max(20);

    let mut state = RunState::new(cwd.clone());
    state.title = Some(command.to_owned());
    state
        .blocks
        .push(OutputBlock::new(command.to_owned(), width));

    let id = stoat.active_workspace_mut().runs.insert(state);

    match crate::run::spawn_oneshot(command, &cwd, width, pty_tx, id) {
        Ok(handle) => {
            let ws = stoat.active_workspace_mut();
            if let Some(run_state) = ws.runs.get_mut(id) {
                run_state.shell_handle = Some(handle);
            }
            stoat.modal_run = Some(id);
            UpdateEffect::Redraw
        },
        Err(e) => {
            tracing::warn!("failed to spawn command: {e}");
            stoat.active_workspace_mut().runs.remove(id);
            UpdateEffect::None
        },
    }
}

fn focus_direction(stoat: &mut Stoat, direction: Direction) {
    let ws = stoat.active_workspace_mut();
    match (ws.focus, direction) {
        (FocusTarget::Dock(dock_id), Direction::Left) => {
            if ws
                .docks
                .get(dock_id)
                .is_some_and(|d| d.side == DockSide::Right)
            {
                ws.focus = FocusTarget::SplitPane(ws.panes.focus());
            }
        },
        (FocusTarget::Dock(dock_id), Direction::Right) => {
            if ws
                .docks
                .get(dock_id)
                .is_some_and(|d| d.side == DockSide::Left)
            {
                ws.focus = FocusTarget::SplitPane(ws.panes.focus());
            }
        },
        (FocusTarget::SplitPane(_), Direction::Right) => {
            if !ws.panes.focus_direction(Direction::Right) {
                if let Some((dock_id, _)) = ws.docks.iter().find(|(_, d)| {
                    d.side == DockSide::Right && !matches!(d.visibility, DockVisibility::Hidden)
                }) {
                    ws.focus = FocusTarget::Dock(dock_id);
                }
            }
        },
        (FocusTarget::SplitPane(_), Direction::Left) => {
            if !ws.panes.focus_direction(Direction::Left) {
                if let Some((dock_id, _)) = ws.docks.iter().find(|(_, d)| {
                    d.side == DockSide::Left && !matches!(d.visibility, DockVisibility::Hidden)
                }) {
                    ws.focus = FocusTarget::Dock(dock_id);
                }
            }
        },
        (FocusTarget::SplitPane(_), _) => {
            ws.panes.focus_direction(direction);
        },
        _ => {},
    }
}

fn open_claude(stoat: &mut Stoat) -> UpdateEffect {
    use stoat_config::ClaudePlacement;

    if let Some(effect) = focus_existing_claude(stoat) {
        return effect;
    }

    let session_id = create_claude_session(stoat);

    let placement = stoat
        .settings
        .claude_default_placement
        .unwrap_or(ClaudePlacement::Pane);
    match placement {
        ClaudePlacement::Pane => place_claude_in_pane(stoat, session_id),
        ClaudePlacement::DockLeft => place_claude_in_dock(stoat, session_id, DockSide::Left),
        ClaudePlacement::DockRight => place_claude_in_dock(stoat, session_id, DockSide::Right),
    }

    stoat.mode = "normal".into();
    UpdateEffect::Redraw
}

fn focus_existing_claude(stoat: &mut Stoat) -> Option<UpdateEffect> {
    use crate::pane::DockVisibility;

    let ws = stoat.active_workspace_mut();

    let pane_match = ws
        .panes
        .split_panes()
        .find(|(_, p)| matches!(&p.view, View::Claude(_)))
        .map(|(id, _)| id);
    if let Some(pid) = pane_match {
        ws.panes.set_focus(pid);
        ws.focus = FocusTarget::SplitPane(pid);
        stoat.mode = "normal".into();
        return Some(UpdateEffect::Redraw);
    }

    for (dock_id, dock) in &mut ws.docks {
        if matches!(&dock.view, View::Claude(_)) {
            if matches!(dock.visibility, DockVisibility::Hidden) {
                dock.visibility = DockVisibility::Open {
                    width: dock.default_width,
                };
            }
            ws.focus = FocusTarget::Dock(dock_id);
            stoat.mode = "normal".into();
            return Some(UpdateEffect::Redraw);
        }
    }

    None
}

fn create_claude_session(stoat: &mut Stoat) -> crate::host::ClaudeSessionId {
    use crate::{claude_chat::ClaudeChatState, editor_state::EditorState};

    let session_id = stoat.claude_sessions_mut().reserve_slot();
    let _ = stoat
        .claude_tx
        .try_send(crate::host::ClaudeNotification::CreateRequested { session_id });

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    ws.claude_chat = Some(session_id);

    let (buffer_id, buffer) = ws.buffers.new_scratch();
    let editor_id = ws
        .editors
        .insert(EditorState::new(buffer_id, buffer, executor));

    ws.chats.insert(
        session_id,
        ClaudeChatState {
            session_id,
            input_editor_id: editor_id,
            input_buffer_id: buffer_id,
            messages: Vec::new(),
            streaming_text: None,
            scroll_offset: 0,
            pending_sends: Vec::new(),
            active_since: None,
        },
    );

    session_id
}

fn place_claude_in_pane(stoat: &mut Stoat, session_id: crate::host::ClaudeSessionId) {
    let ws = stoat.active_workspace_mut();
    let pid = ws.panes.focus();
    ws.panes.pane_mut(pid).view = View::Claude(session_id);
    ws.focus = FocusTarget::SplitPane(pid);
}

fn place_claude_in_dock(
    stoat: &mut Stoat,
    session_id: crate::host::ClaudeSessionId,
    side: DockSide,
) {
    use crate::pane::{DockPanel, DockVisibility};
    let ws = stoat.active_workspace_mut();
    let dock_id = ws.docks.insert(DockPanel {
        view: View::Claude(session_id),
        side,
        visibility: DockVisibility::Open { width: 40 },
        default_width: 40,
        area: ratatui::layout::Rect::default(),
    });
    ws.focus = FocusTarget::Dock(dock_id);
}

fn claude_to_pane(stoat: &mut Stoat) -> UpdateEffect {
    let Some(session_id) = stoat.active_workspace().claude_chat else {
        return UpdateEffect::None;
    };

    {
        let ws = stoat.active_workspace_mut();
        let existing = ws
            .panes
            .split_panes()
            .find(|(_, p)| matches!(&p.view, View::Claude(id) if *id == session_id))
            .map(|(id, _)| id);
        if let Some(pid) = existing {
            ws.panes.set_focus(pid);
            ws.focus = FocusTarget::SplitPane(pid);
            return UpdateEffect::Redraw;
        }
    }

    remove_claude_from_docks(stoat, session_id);
    place_claude_in_pane(stoat, session_id);
    UpdateEffect::Redraw
}

fn claude_to_dock(stoat: &mut Stoat, side: DockSide) -> UpdateEffect {
    use crate::pane::DockVisibility;

    let Some(session_id) = stoat.active_workspace().claude_chat else {
        return UpdateEffect::None;
    };

    {
        let ws = stoat.active_workspace_mut();
        let existing = ws
            .docks
            .iter()
            .find(|(_, d)| matches!(&d.view, View::Claude(id) if *id == session_id))
            .map(|(id, _)| id);
        if let Some(did) = existing {
            if let Some(dock) = ws.docks.get_mut(did) {
                dock.side = side;
                if matches!(dock.visibility, DockVisibility::Hidden) {
                    dock.visibility = DockVisibility::Open {
                        width: dock.default_width,
                    };
                }
            }
            ws.focus = FocusTarget::Dock(did);
            return UpdateEffect::Redraw;
        }
    }

    remove_claude_from_panes(stoat, session_id);
    place_claude_in_dock(stoat, session_id, side);
    UpdateEffect::Redraw
}

fn remove_claude_from_docks(stoat: &mut Stoat, session_id: crate::host::ClaudeSessionId) {
    let ws = stoat.active_workspace_mut();
    let dids: Vec<_> = ws
        .docks
        .iter()
        .filter(|(_, d)| matches!(&d.view, View::Claude(id) if *id == session_id))
        .map(|(id, _)| id)
        .collect();
    for did in dids {
        ws.docks.remove(did);
    }
}

fn remove_claude_from_panes(stoat: &mut Stoat, session_id: crate::host::ClaudeSessionId) {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let pids: Vec<_> = ws
        .panes
        .split_panes()
        .filter(|(_, p)| matches!(&p.view, View::Claude(id) if *id == session_id))
        .map(|(id, _)| id)
        .collect();
    for pid in pids {
        if !ws.panes.close(pid) {
            let (bid, buffer) = ws.buffers.new_scratch();
            let eid = ws
                .editors
                .insert(EditorState::new(bid, buffer, executor.clone()));
            ws.panes.pane_mut(pid).view = View::Editor(eid);
        }
    }
}

fn claude_submit(stoat: &mut Stoat) -> UpdateEffect {
    use crate::claude_chat::{ChatMessage, ChatMessageContent, ChatRole};

    let session_id = match stoat.active_workspace().claude_chat {
        Some(id) => id,
        None => return UpdateEffect::None,
    };

    // Read input text before mutating.
    let text = {
        let ws = stoat.active_workspace();
        let chat = match ws.chats.get(&session_id) {
            Some(c) => c,
            None => return UpdateEffect::None,
        };
        let buffer = match ws.buffers.get(chat.input_buffer_id) {
            Some(b) => b,
            None => return UpdateEffect::None,
        };
        let guard = buffer.read().expect("buffer poisoned");
        guard.snapshot.visible_text.to_string()
    };
    if text.trim().is_empty() {
        return UpdateEffect::None;
    }

    // Mutate chat state: push user message and clear input buffer.
    {
        let ws = stoat.active_workspace_mut();
        let Some(chat) = ws.chats.get_mut(&session_id) else {
            return UpdateEffect::None;
        };
        chat.messages.push(ChatMessage {
            role: ChatRole::User,
            content: ChatMessageContent::Text(text.clone()),
        });
        chat.active_since = Some(std::time::Instant::now());

        let Some(buffer) = ws.buffers.get(chat.input_buffer_id) else {
            return UpdateEffect::None;
        };
        {
            let len = buffer.read().expect("poisoned").snapshot.visible_text.len();
            buffer.write().expect("poisoned").edit(0..len, "");
        }
        let Some(editor) = ws.editors.get_mut(chat.input_editor_id) else {
            return UpdateEffect::None;
        };
        editor.selections = crate::selection::SelectionsCollection::new();
    }

    // Send now if host is ready, otherwise queue for when it becomes available.
    if let Some(host) = stoat.claude_sessions().get(session_id) {
        let host = host.clone();
        stoat
            .executor
            .spawn(async move {
                if let Err(e) = host.send(&text).await {
                    tracing::error!("claude send error: {e}");
                }
            })
            .detach();
    } else {
        let ws = stoat.active_workspace_mut();
        if let Some(chat) = ws.chats.get_mut(&session_id) {
            chat.pending_sends.push(text);
        }
    }

    UpdateEffect::Redraw
}

fn toggle_dock(stoat: &mut Stoat, side: DockSide) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    for (dock_id, dock) in &mut ws.docks {
        if dock.side != side {
            continue;
        }
        dock.visibility = match dock.visibility {
            DockVisibility::Open { .. } => DockVisibility::Minimized,
            DockVisibility::Minimized => DockVisibility::Hidden,
            DockVisibility::Hidden => DockVisibility::Open {
                width: dock.default_width,
            },
        };
        if matches!(dock.visibility, DockVisibility::Hidden)
            && matches!(ws.focus, FocusTarget::Dock(id) if id == dock_id)
        {
            ws.focus = FocusTarget::SplitPane(ws.panes.focus());
        }
        return UpdateEffect::Redraw;
    }
    UpdateEffect::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use stoat_action::{
        AddSelectionBelow, MoveDown, MoveLeft, MoveNextWordEnd, MoveNextWordStart,
        MovePrevWordStart, MoveRight, MoveUp, Quit,
    };
    use stoat_scheduler::TestScheduler;

    fn stoat() -> Stoat {
        let scheduler = Arc::new(TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            stoat_config::Settings::default(),
            std::path::PathBuf::new(),
        );
        stoat.update(crossterm::event::Event::Resize(80, 24));
        stoat
    }

    fn seed_focused_buffer(stoat: &mut Stoat, text: &str) {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer exists");
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.edit(0..0, text);
    }

    fn head_offsets(stoat: &mut Stoat) -> Vec<usize> {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let editor = ws.editors.get_mut(editor_id).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| buffer_snapshot.resolve_anchor(&sel.head()))
            .collect()
    }

    fn selection_spans(stoat: &mut Stoat) -> Vec<(usize, usize, bool)> {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let editor = ws.editors.get_mut(editor_id).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                (
                    buffer_snapshot.resolve_anchor(&sel.start),
                    buffer_snapshot.resolve_anchor(&sel.end),
                    sel.reversed,
                )
            })
            .collect()
    }

    fn cursor_display_positions(stoat: &mut Stoat) -> Vec<(u32, u32)> {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let editor = ws.editors.get_mut(editor_id).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let head = sel.head();
                let point = buffer_snapshot.point_for_anchor(&head);
                let display = snapshot.buffer_to_display(point);
                (display.row, display.column)
            })
            .collect()
    }

    #[test]
    fn dispatch_quit() {
        assert_eq!(dispatch(&mut stoat(), &Quit), UpdateEffect::Quit);
    }

    #[test]
    fn move_left_at_start_is_noop() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "hello");
        dispatch(&mut stoat, &MoveLeft);
        assert_eq!(head_offsets(&mut stoat), vec![0]);
    }

    #[test]
    fn move_right_advances_one_grapheme() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(head_offsets(&mut stoat), vec![1]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(head_offsets(&mut stoat), vec![2]);
    }

    #[test]
    fn move_right_at_end_is_noop() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(head_offsets(&mut stoat), vec![3]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn move_right_across_newline() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "ab\ncd");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn move_right_multibyte() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "héllo");
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(head_offsets(&mut stoat), vec![1]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn move_down_advances_one_row() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc\ndef\n");
        dispatch(&mut stoat, &MoveDown);
        assert_eq!(cursor_display_positions(&mut stoat), vec![(1, 0)]);
    }

    #[test]
    fn move_up_at_first_row_is_noop() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc\ndef");
        dispatch(&mut stoat, &MoveUp);
        assert_eq!(cursor_display_positions(&mut stoat), vec![(0, 0)]);
    }

    #[test]
    fn move_down_at_last_row_is_noop() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &MoveDown);
        assert_eq!(cursor_display_positions(&mut stoat), vec![(0, 0)]);
    }

    #[test]
    fn move_down_preserves_goal_column() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");
        for _ in 0..7 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(cursor_display_positions(&mut stoat), vec![(0, 7)]);
        dispatch(&mut stoat, &MoveDown);
        assert_eq!(cursor_display_positions(&mut stoat), vec![(1, 2)]);
        dispatch(&mut stoat, &MoveDown);
        assert_eq!(cursor_display_positions(&mut stoat), vec![(2, 7)]);
    }

    #[test]
    fn move_next_word_start_creates_selection() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(selection_spans(&mut stoat), vec![(0, 3, false)]);
        assert_eq!(head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn move_next_word_start_repeated_snaps_tail() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "foo bar baz");
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(selection_spans(&mut stoat), vec![(0, 3, false)]);
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(selection_spans(&mut stoat), vec![(3, 7, false)]);
    }

    #[test]
    fn move_next_word_end_creates_selection() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MoveNextWordEnd);
        assert_eq!(selection_spans(&mut stoat), vec![(0, 2, false)]);
    }

    #[test]
    fn move_next_word_end_at_eof_is_noop() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "foo");
        for _ in 0..3 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(head_offsets(&mut stoat), vec![3]);
        dispatch(&mut stoat, &MoveNextWordEnd);
        assert_eq!(selection_spans(&mut stoat), vec![(3, 3, false)]);
    }

    #[test]
    fn move_prev_word_start_creates_reversed_selection() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "foo bar");
        for _ in 0..6 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(head_offsets(&mut stoat), vec![6]);
        dispatch(&mut stoat, &MovePrevWordStart);
        assert_eq!(selection_spans(&mut stoat), vec![(4, 7, true)]);
        assert_eq!(head_offsets(&mut stoat), vec![4]);
    }

    #[test]
    fn move_prev_word_start_at_start_is_noop() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MovePrevWordStart);
        assert_eq!(selection_spans(&mut stoat), vec![(0, 0, false)]);
    }

    #[test]
    fn move_right_with_multiple_cursors_advances_each() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
        dispatch(&mut stoat, &AddSelectionBelow);
        assert_eq!(head_offsets(&mut stoat), vec![0, 4]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(head_offsets(&mut stoat), vec![1, 5]);
    }

    #[test]
    fn move_next_word_start_multi_cursor_independent() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "foo bar\nbaz qux\n");
        dispatch(&mut stoat, &AddSelectionBelow);
        assert_eq!(head_offsets(&mut stoat), vec![0, 8]);
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(
            selection_spans(&mut stoat),
            vec![(0, 3, false), (8, 11, false)]
        );
    }

    #[test]
    fn add_selection_below_with_no_editor_focus_is_noop() {
        let mut stoat = stoat();
        {
            let ws = stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            ws.panes.pane_mut(focused).view = View::Label("nothing".into());
        }
        assert_eq!(dispatch(&mut stoat, &AddSelectionBelow), UpdateEffect::None);
    }

    #[test]
    fn add_selection_below_adds_cursor_on_next_display_row() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );

        let positions = cursor_display_positions(&mut stoat);
        assert_eq!(positions, vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn add_selection_below_at_last_row_is_noop() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "abc");

        assert_eq!(dispatch(&mut stoat, &AddSelectionBelow), UpdateEffect::None);
        assert_eq!(cursor_display_positions(&mut stoat), vec![(0, 0)]);
    }

    #[test]
    fn add_selection_below_preserves_goal_column_on_short_line() {
        let mut stoat = stoat();
        seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");

        {
            let ws = stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            let editor_id = match ws.panes.pane(focused).view {
                View::Editor(id) => id,
                _ => unreachable!(),
            };
            let editor = ws.editors.get_mut(editor_id).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let buffer = snapshot.buffer_snapshot();
            let offset = buffer.rope().point_to_offset(stoat_text::Point::new(0, 7));
            let anchor = buffer.anchor_at(offset, Bias::Right);
            editor
                .selections
                .insert_cursor(anchor, SelectionGoal::Column(7), buffer);
        }

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );
        let after_one = cursor_display_positions(&mut stoat);
        assert_eq!(after_one, vec![(0, 0), (0, 7), (1, 2)]);

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );
        let after_two = cursor_display_positions(&mut stoat);
        assert_eq!(after_two, vec![(0, 0), (0, 7), (1, 2), (2, 7)]);
    }

    #[test]
    fn claude_submit_queues_when_session_not_ready() {
        let mut stoat = stoat();

        dispatch(&mut stoat, &stoat_action::OpenClaude);

        let session_id = stoat
            .active_workspace()
            .claude_chat
            .expect("claude_chat should be set");
        assert!(
            stoat.claude_sessions().get(session_id).is_none(),
            "host slot should be None after reserve_slot"
        );

        {
            let ws = stoat.active_workspace();
            let chat = ws.chats.get(&session_id).expect("chat state exists");
            let buffer = ws.buffers.get(chat.input_buffer_id).expect("buffer");
            buffer.write().expect("poisoned").edit(0..0, "hello claude");
        }

        dispatch(&mut stoat, &stoat_action::ClaudeSubmit);

        let ws = stoat.active_workspace();
        let chat = ws.chats.get(&session_id).expect("chat state");
        assert_eq!(chat.messages.len(), 1, "user message should be in chat");
        assert_eq!(
            chat.pending_sends,
            vec!["hello claude"],
            "message should be queued, not dropped"
        );
    }

    #[test]
    fn type_action_direct() {
        let mut h = Stoat::test();
        h.type_action("SetMode(space)");
        let last = h.frames().last().expect("no frames");
        assert_eq!(last.mode, "space");
    }

    #[test]
    fn open_file_shows_in_focused_pane() {
        let mut h = Stoat::test();
        let path = crate::test_harness::write_file(&h, "test.txt", "hello world");

        h.open_file(&path);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        h.assert_snapshot("open_file_shows_in_focused_pane");
    }

    #[test]
    fn open_file_replaces_focused_pane_does_not_split() {
        let mut h = Stoat::test();
        let a = crate::test_harness::write_file(&h, "a.txt", "file A");
        let b = crate::test_harness::write_file(&h, "b.txt", "file B");

        h.open_file(&a);
        h.open_file(&b);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        h.assert_snapshot("open_file_replaces_focused_pane");
    }

    #[test]
    fn split_then_open_creates_multi_pane_layout() {
        let mut h = Stoat::test();
        let a = crate::test_harness::write_file(&h, "a.txt", "AAA");
        let b = crate::test_harness::write_file(&h, "b.txt", "BBB");
        let c = crate::test_harness::write_file(&h, "c.txt", "CCC");

        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.type_action("SplitRight()");
        h.open_file(&c);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 3);
        h.assert_snapshot("split_then_open_three");
    }

    #[test]
    fn open_missing_file_creates_empty_buffer() {
        let path = std::path::PathBuf::from("/test/does_not_exist.txt");

        let mut h = Stoat::test();
        h.open_file(&path);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
    }

    #[test]
    fn open_file_via_fs_host_reads_from_fake_fs() {
        let mut h = Stoat::test();
        h.fake_fs
            .insert_file("/work/hello.txt", b"greetings from fake fs");
        h.stoat.open_file(std::path::Path::new("/work/hello.txt"));
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let editor = ws.editors.get(editor_id).unwrap();
        let buffer = ws.buffers.get(editor.buffer_id).unwrap();
        let guard = buffer.read().unwrap();
        assert_eq!(
            guard.snapshot.visible_text.to_string(),
            "greetings from fake fs"
        );
    }

    #[test]
    fn type_action_quit_from_space() {
        let mut h = Stoat::test();
        h.type_keys("space");
        h.type_action("Quit");
    }

    #[test]
    #[should_panic(expected = "unreachable")]
    fn type_action_unreachable_panics() {
        let mut h = Stoat::test();
        h.type_action("NonExistentAction");
    }
}
