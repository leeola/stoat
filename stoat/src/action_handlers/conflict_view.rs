use super::focused_editor_mut;
use crate::{
    app::Stoat,
    conflict_session::{ConflictSession, ConflictViewState, FileResolveState},
    display_map::{BlockPlacement, BlockProperties, BlockStyle},
    editor_state::{EditorId, EditorState},
    host::git::GitRepo,
    jumplist::JumpEntry,
    merge_view::{ChunkState, MergeDoc, RowPick},
    pane::View,
};
use ratatui::text::Line;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::Path,
    sync::Arc,
};
use stoat_action::ActionKind;
use stoat_text::{Anchor, Bias, SelectionGoal};

/// Open the three-way conflict resolve view on the repository's conflicted
/// files, swapping a scratch merged-result editor into the focused pane.
///
/// Dispatching while the view is already open closes it (toggle). With no
/// index conflicts, sets a status and leaves the file view in place.
pub(super) fn open_conflict(stoat: &mut Stoat) {
    if stoat.active_workspace().conflict.is_some() {
        close_conflict(stoat);
        return;
    }

    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        stoat.set_status("no git repository");
        return;
    };
    let files = repo.conflicted_paths();
    if files.is_empty() {
        stoat.set_status("no merge conflicts");
        return;
    }

    let focused_buffer = focused_editor_mut(stoat).map(|editor| editor.buffer_id);
    let focused_path = focused_buffer.and_then(|buffer_id| {
        stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)
    });
    let current = focused_path
        .as_deref()
        .and_then(|p| files.iter().position(|c| c == p))
        .unwrap_or(0);

    let saved_editor = {
        let ws = stoat.active_workspace();
        let focused = ws.panes.focus();
        let View::Editor(saved) = ws.panes.pane(focused).view else {
            return;
        };
        saved
    };

    let origin = super::jump::live_entry(stoat);

    let file_count = files.len();
    let Some((file, first_chunk_offset)) = prepare_file(
        stoat,
        &repo,
        &files[current],
        current,
        file_count,
        &git_root,
    ) else {
        stoat.set_status("no merge conflicts");
        return;
    };

    {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        ws.panes.pane_mut(focused).view = View::Editor(file.editor_id);
        ws.panes.widen(focused);

        ws.conflict = Some(ConflictSession {
            workdir: git_root,
            files,
            current,
            file,
            saved_editor,
            pending_clobber: None,
            parked: HashMap::new(),
            applied: HashSet::new(),
        });
    }

    land_first_chunk(stoat, first_chunk_offset, origin);
}

/// Build the scratch center editor and resolve state for one conflicted file.
///
/// Reads the file's index stages through `repo`, seeds a scratch buffer with the
/// initial marker text, and inserts a center editor carrying the render cache.
/// Returns the resolve state and the byte offset of its first chunk, or `None`
/// when the file no longer reports index conflicts. Leaves the pane and session
/// untouched, so the caller decides how the editor is displayed.
fn prepare_file(
    stoat: &mut Stoat,
    repo: &Arc<dyn GitRepo>,
    path: &Path,
    index: usize,
    file_count: usize,
    git_root: &Path,
) -> Option<(FileResolveState, Option<usize>)> {
    let stages = repo.conflict_stages(path)?;
    let language = stoat.language_registry.for_path(path);
    let doc = MergeDoc::build(
        stages.ancestor.as_deref().unwrap_or(""),
        stages.ours.as_deref().unwrap_or(""),
        stages.theirs.as_deref().unwrap_or(""),
        language.as_ref(),
    );
    let (center_text, chunk_ranges) = doc.initial_center_text();
    let first_chunk_offset = chunk_ranges.first().map(|r| r.start);
    let executor = stoat.executor.clone();
    let rel_path = crate::paths::display_relative(path, git_root);

    let ws = stoat.active_workspace_mut();
    let (buffer_id, buffer) = ws.buffers.new_scratch_preview_unseeded();
    {
        let mut guard = buffer.write().expect("buffer poisoned");
        guard.edit(0..0, &center_text);
        guard.mark_clean();
    }
    if let Some(lang) = language {
        ws.buffers.set_language(buffer_id, lang);
    }

    let chunk_anchors: Vec<(Anchor, Anchor)> = {
        let guard = buffer.read().expect("buffer poisoned");
        chunk_ranges
            .iter()
            .map(|r| {
                (
                    guard.anchor_at(r.start, Bias::Left),
                    guard.anchor_at(r.end, Bias::Right),
                )
            })
            .collect()
    };

    let picks: Vec<Vec<RowPick>> = doc
        .chunks
        .iter()
        .map(|chunk| {
            vec![
                RowPick {
                    ours: false,
                    theirs: false
                };
                chunk.row_range.len()
            ]
        })
        .collect();

    let mut editor = EditorState::new(buffer_id, buffer, executor);
    editor.conflict_view = Some(ConflictViewState {
        doc: doc.clone(),
        chunk_anchors: chunk_anchors.clone(),
        picks: picks.clone(),
        file_index: index,
        file_count,
        rel_path,
    });
    let editor_id = ws.editors.insert(editor);

    Some((
        FileResolveState {
            path: path.to_path_buf(),
            doc,
            picks,
            chunk_anchors,
            buffer_id,
            editor_id,
        },
        first_chunk_offset,
    ))
}

/// Close the conflict view, restoring the original editor into the focused pane
/// and disposing the scratch center editor and buffer when unreferenced.
pub(super) fn close_conflict(stoat: &mut Stoat) {
    let ws = stoat.active_workspace_mut();
    let Some(session) = ws.conflict.take() else {
        return;
    };
    let saved = session.saved_editor;
    let scratch_editor = session.file.editor_id;
    let scratch_buffer = session.file.buffer_id;

    let focused = ws.panes.focus();
    let showing_conflict = matches!(
        ws.panes.pane(focused).view,
        View::Editor(eid) if eid == scratch_editor
    );
    if showing_conflict {
        ws.panes.pane_mut(focused).view = View::Editor(saved);
        if ws.panes.widened() == Some(focused) {
            ws.panes.unwiden();
        }
    }

    let editor_referenced = ws
        .panes
        .split_panes()
        .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == scratch_editor));
    if !editor_referenced {
        ws.editors.remove(scratch_editor);
        ws.buffers.remove(scratch_buffer);
    }

    for parked in session.parked.into_values() {
        ws.editors.remove(parked.editor_id);
        ws.buffers.remove(parked.buffer_id);
    }
}

/// Resolve the conflict chunk under the cursor by taking its whole ours side.
pub(super) fn conflict_pick_ours(stoat: &mut Stoat) {
    pick_side(stoat, ActionKind::ConflictPickOurs);
}

/// Resolve the conflict chunk under the cursor by taking its whole theirs side.
pub(super) fn conflict_pick_theirs(stoat: &mut Stoat) {
    pick_side(stoat, ActionKind::ConflictPickTheirs);
}

/// Resolve the conflict chunk under the cursor by taking both sides.
pub(super) fn conflict_pick_both(stoat: &mut Stoat) {
    pick_side(stoat, ActionKind::ConflictPickBoth);
}

/// Reset the conflict chunk under the cursor back to its raw marker block,
/// discarding any pick or hand edit in that region.
pub(super) fn conflict_reset_chunk(stoat: &mut Stoat) {
    let Some((chunk_idx, region, _)) = current_chunk(stoat) else {
        return;
    };
    let (marker, picks) = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return;
        };
        let chunk = &session.file.doc.chunks[chunk_idx];
        let picks = vec![
            RowPick {
                ours: false,
                theirs: false
            };
            chunk.row_range.len()
        ];
        (chunk.marker_text(&session.file.doc.rows), picks)
    };
    apply_resolution(stoat, chunk_idx, region, &marker, picks);
}

/// Land the cursor on the next (`forward`) or previous conflict chunk, stopping
/// at the last or first chunk rather than wrapping.
pub(super) fn conflict_step_chunk(stoat: &mut Stoat, forward: bool) {
    let (editor_id, anchors) = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return;
        };
        (session.file.editor_id, session.file.chunk_anchors.clone())
    };

    let (cursor, starts) = {
        let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) else {
            return;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let cursor = buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().start);
        let starts: Vec<usize> = anchors
            .iter()
            .map(|(start, _)| buffer_snapshot.resolve_anchor(start))
            .collect();
        (cursor, starts)
    };

    let target = if forward {
        starts.into_iter().find(|&start| start > cursor)
    } else {
        starts.into_iter().rev().find(|&start| start < cursor)
    };
    let Some(offset) = target else {
        return;
    };

    land_cursor(stoat, editor_id, offset);
    scroll_cursor_into_view(stoat, editor_id);
}

/// Step to the next (`forward`) or previous conflicted file. Stops at the last
/// or first file rather than wrapping.
pub(super) fn conflict_step_file(stoat: &mut Stoat, forward: bool) {
    let target = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return;
        };
        match (forward, session.current) {
            (true, c) if c + 1 < session.files.len() => c + 1,
            (false, c) if c > 0 => c - 1,
            _ => return,
        }
    };
    switch_to_file(stoat, target);
}

/// Show the file at `target`, parking the outgoing file's resolve state or
/// restoring a parked one, then landing the cursor on its first chunk.
///
/// A no-op when `target` is already current or out of range. Parking keeps the
/// outgoing file's picks and scratch buffer alive for a later return.
fn switch_to_file(stoat: &mut Stoat, target: usize) {
    let (current, file_count, git_root, target_path) = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return;
        };
        if target == session.current || target >= session.files.len() {
            return;
        }
        (
            session.current,
            session.files.len(),
            session.workdir.clone(),
            session.files[target].clone(),
        )
    };

    let parked = stoat
        .active_workspace_mut()
        .conflict
        .as_mut()
        .and_then(|session| session.parked.remove(&target));
    let target_state = match parked {
        Some(state) => state,
        None => {
            let Some(repo) = stoat.git_host.discover(&git_root) else {
                return;
            };
            let Some((state, _)) =
                prepare_file(stoat, &repo, &target_path, target, file_count, &git_root)
            else {
                return;
            };
            state
        },
    };

    let (outgoing_editor, target_editor) = {
        let Some(session) = stoat.active_workspace_mut().conflict.as_mut() else {
            return;
        };
        let target_editor = target_state.editor_id;
        let outgoing = std::mem::replace(&mut session.file, target_state);
        let outgoing_editor = outgoing.editor_id;
        session.parked.insert(current, outgoing);
        session.current = target;
        session.pending_clobber = None;
        (outgoing_editor, target_editor)
    };

    {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        if matches!(ws.panes.pane(focused).view, View::Editor(e) if e == outgoing_editor) {
            ws.panes.pane_mut(focused).view = View::Editor(target_editor);
        }
    }

    land_first_chunk_current(stoat);
}

/// Write the center text to the working file, and when every chunk is resolved
/// mark it resolved in the index and advance to the next unapplied file or close
/// once all are applied.
///
/// The center is always written, so a half-resolved file lands its honest
/// marker blocks and quitting mid-resolve loses nothing. A file with any chunk
/// still on its markers is written but not marked resolved, and stays open.
pub(super) fn conflict_apply(stoat: &mut Stoat) {
    let (current, path, buffer_id, git_root) = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return;
        };
        (
            session.current,
            session.file.path.clone(),
            session.file.buffer_id,
            session.workdir.clone(),
        )
    };

    let text = {
        let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
            return;
        };
        let guard = buffer.read().expect("buffer poisoned");
        guard.rope().to_string()
    };

    if let Err(err) = stoat.fs_host.write_atomic(&path, text.as_bytes()) {
        stoat.set_status(format!("write failed: {err}"));
        return;
    }

    let unresolved = unresolved_chunk_count(stoat);
    if unresolved > 0 {
        stoat.set_status(format!("written with {unresolved} unresolved chunk(s)"));
        return;
    }

    let Some(repo) = stoat.git_host.discover(&git_root) else {
        return;
    };
    if let Err(err) = repo.mark_resolved(&path) {
        stoat.set_status(format!("mark resolved failed: {err}"));
        return;
    }

    let next = {
        let Some(session) = stoat.active_workspace_mut().conflict.as_mut() else {
            return;
        };
        session.applied.insert(current);
        let count = session.files.len();
        (1..count)
            .map(|step| (current + step) % count)
            .find(|index| !session.applied.contains(index))
    };

    match next {
        Some(target) => {
            stoat.set_status("conflict resolved");
            switch_to_file(stoat, target);
        },
        None => {
            stoat.set_status("all conflicts resolved");
            close_conflict(stoat);
        },
    }
}

/// Count the current file's chunks still showing their raw conflict markers,
/// classified against the live center text so hand edits and picks are excluded.
fn unresolved_chunk_count(stoat: &mut Stoat) -> usize {
    let (editor_id, anchors) = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return 0;
        };
        (session.file.editor_id, session.file.chunk_anchors.clone())
    };

    let region_texts: Vec<String> = {
        let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) else {
            return 0;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        anchors
            .iter()
            .map(|(start, end)| {
                let start = buffer_snapshot.resolve_anchor(start);
                let end = buffer_snapshot.resolve_anchor(end);
                buffer_snapshot.rope().chunks_in_range(start..end).collect()
            })
            .collect()
    };

    let Some(session) = stoat.active_workspace().conflict.as_ref() else {
        return 0;
    };
    let doc = &session.file.doc;
    region_texts
        .iter()
        .enumerate()
        .filter(|(index, text)| {
            doc.chunks[*index].classify(&doc.rows, &session.file.picks[*index], text)
                == ChunkState::Unresolved
        })
        .count()
}

/// Apply a whole-side pick of the given kind to the chunk under the cursor.
///
/// A pick over a region hand-edited to text no pick produces first arms the
/// clobber guard and warns. An immediate repeat of the identical pick confirms
/// and overwrites.
fn pick_side(stoat: &mut Stoat, kind: ActionKind) {
    let Some((chunk_idx, region, region_text)) = current_chunk(stoat) else {
        return;
    };

    let (assembly, picks, manual) = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return;
        };
        let doc = &session.file.doc;
        let chunk = &doc.chunks[chunk_idx];
        let picks = match kind {
            ActionKind::ConflictPickTheirs => chunk.all_theirs(),
            ActionKind::ConflictPickBoth => chunk.all_both(),
            _ => chunk.all_ours(),
        };
        let manual = chunk.classify(&doc.rows, &session.file.picks[chunk_idx], &region_text)
            == ChunkState::Manual;
        (chunk.assembly_text(&doc.rows, &picks), picks, manual)
    };

    let armed = stoat
        .active_workspace()
        .conflict
        .as_ref()
        .is_some_and(|s| s.pending_clobber == Some((chunk_idx, kind)));
    if manual && !armed {
        if let Some(session) = stoat.active_workspace_mut().conflict.as_mut() {
            session.pending_clobber = Some((chunk_idx, kind));
        }
        stoat.set_status("chunk was hand-edited, repeat the pick to overwrite");
        return;
    }

    apply_resolution(stoat, chunk_idx, region, &assembly, picks);
}

/// The chunk whose center region contains the cursor, with its current byte
/// range and region text. `None` when no session is open or the cursor sits
/// outside every chunk.
fn current_chunk(stoat: &mut Stoat) -> Option<(usize, Range<usize>, String)> {
    let editor_id = stoat.active_workspace().conflict.as_ref()?.file.editor_id;

    let (snapshot, cursor_anchor) = {
        let editor = stoat.active_workspace_mut().editors.get_mut(editor_id)?;
        (
            editor.display_map.snapshot(),
            editor.selections.newest_anchor().start,
        )
    };
    let buffer_snapshot = snapshot.buffer_snapshot();
    let cursor = buffer_snapshot.resolve_anchor(&cursor_anchor);

    let session = stoat.active_workspace().conflict.as_ref()?;
    let chunk_idx = session.file.chunk_anchors.iter().position(|(start, end)| {
        let start = buffer_snapshot.resolve_anchor(start);
        let end = buffer_snapshot.resolve_anchor(end);
        (start..end).contains(&cursor)
    })?;

    let (start, end) = &session.file.chunk_anchors[chunk_idx];
    let region = buffer_snapshot.resolve_anchor(start)..buffer_snapshot.resolve_anchor(end);
    let region_text = buffer_snapshot
        .rope()
        .chunks_in_range(region.clone())
        .collect();
    Some((chunk_idx, region, region_text))
}

/// Write `text` over the chunk's center region, then mirror `picks` onto the
/// session and the render cache and disarm the clobber guard.
///
/// The buffer edit rides the dispatch-level undo group, so a pick and its
/// revert are single undo steps. Chunk anchors auto-shift so later chunks stay
/// aligned when this region grows or shrinks.
fn apply_resolution(
    stoat: &mut Stoat,
    chunk_idx: usize,
    region: Range<usize>,
    text: &str,
    picks: Vec<RowPick>,
) {
    let (buffer_id, editor_id) = {
        let Some(session) = stoat.active_workspace_mut().conflict.as_mut() else {
            return;
        };
        session.file.picks[chunk_idx] = picks.clone();
        session.pending_clobber = None;
        (session.file.buffer_id, session.file.editor_id)
    };

    let region_start = region.start;
    if let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) {
        buffer.write().expect("buffer poisoned").edit(region, text);
    }

    land_cursor(stoat, editor_id, region_start);

    if let Some(view) = stoat
        .active_workspace_mut()
        .editors
        .get_mut(editor_id)
        .and_then(|editor| editor.conflict_view.as_mut())
    {
        view.picks[chunk_idx] = picks;
    }

    refresh_padding(stoat, editor_id);
}

/// Reinstall the spacer blocks that pad any chunk whose center is now shorter
/// than its taller side, so every ours and theirs line has a display row.
///
/// A pick shrinks the marker block to its resolution, which can leave the
/// non-picked side with more lines than the center. Each such chunk gets a
/// spacer block below its last center row, sized to the overflow, matching the
/// extra rows [`crate::merge_view::MergeDoc::align`] emits.
fn refresh_padding(stoat: &mut Stoat, editor_id: EditorId) {
    let (anchors, side_heights) = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return;
        };
        let doc = &session.file.doc;
        let heights: Vec<(usize, usize)> = doc
            .chunks
            .iter()
            .map(|chunk| {
                (
                    chunk.ours_lines(&doc.rows).len(),
                    chunk.theirs_lines(&doc.rows).len(),
                )
            })
            .collect();
        (session.file.chunk_anchors.clone(), heights)
    };

    let blocks = {
        let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) else {
            return;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();

        let mut blocks = Vec::new();
        for ((start, end), (ours_len, theirs_len)) in anchors.iter().zip(&side_heights) {
            let start_row = rope
                .offset_to_point(buffer_snapshot.resolve_anchor(start))
                .row;
            let end_row = rope
                .offset_to_point(buffer_snapshot.resolve_anchor(end))
                .row;
            let span = end_row.saturating_sub(start_row) as usize;
            let padding = span.max(*ours_len).max(*theirs_len) - span;
            if padding == 0 {
                continue;
            }
            let placement = if span == 0 {
                BlockPlacement::Above(start_row)
            } else {
                BlockPlacement::Below(start_row + span as u32 - 1)
            };
            blocks.push(padding_block(placement, padding as u32));
        }
        blocks
    };

    if let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) {
        editor.display_map.set_conflict_padding_blocks(blocks);
    }
}

/// A spacer block of `height` blank center rows at `placement`.
fn padding_block(placement: BlockPlacement, height: u32) -> BlockProperties {
    BlockProperties {
        placement,
        height: Some(height),
        style: BlockStyle::Spacer,
        render: Arc::new(move |_ctx| vec![Line::raw(String::new()); height as usize]),
        diff_status: None,
        priority: 0,
    }
}

/// Park a block cursor at `offset` on the center editor, keeping it on the chunk
/// just resolved so an immediate re-pick targets the same region rather than
/// falling outside it once the pick shrinks the marker block.
fn land_cursor(stoat: &mut Stoat, editor_id: EditorId, offset: usize) {
    let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) else {
        return;
    };
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    editor.selections.transform(buffer_snapshot, |sel| {
        super::movement::land_block_cursor(
            sel.id,
            offset,
            SelectionGoal::None,
            buffer_snapshot.rope(),
            buffer_snapshot,
        )
    });
}

/// Scroll the center editor so its cursor sits within the configured scrolloff
/// margin after a navigation step lands off-screen.
fn scroll_cursor_into_view(stoat: &mut Stoat, editor_id: EditorId) {
    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) {
        super::movement::ensure_cursor_in_view(editor, scrolloff);
    }
}

/// Land the cursor on the current file's first conflict chunk after a file
/// switch, resolving its start anchor against the now-focused center editor.
fn land_first_chunk_current(stoat: &mut Stoat) {
    let (editor_id, first_anchor) = {
        let Some(session) = stoat.active_workspace().conflict.as_ref() else {
            return;
        };
        let Some((start, _)) = session.file.chunk_anchors.first() else {
            return;
        };
        (session.file.editor_id, *start)
    };

    let offset = {
        let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) else {
            return;
        };
        let snapshot = editor.display_map.snapshot();
        snapshot.buffer_snapshot().resolve_anchor(&first_anchor)
    };

    land_cursor(stoat, editor_id, offset);
    scroll_cursor_into_view(stoat, editor_id);
}

/// Land the cursor on the newly-opened view's first conflict chunk and push the
/// pre-open position to the jumplist so the usual jump-back returns.
fn land_first_chunk(stoat: &mut Stoat, offset: Option<usize>, origin: Option<JumpEntry>) {
    let Some(offset) = offset else {
        return;
    };
    let jumped = focused_editor_mut(stoat).is_some_and(|editor| {
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        editor.selections.transform(buffer_snapshot, |sel| {
            super::movement::land_block_cursor(
                sel.id,
                offset,
                SelectionGoal::None,
                buffer_snapshot.rope(),
                buffer_snapshot,
            )
        });
        true
    });
    if jumped {
        let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
        if let Some(editor) = focused_editor_mut(stoat) {
            super::movement::ensure_cursor_in_view(editor, scrolloff);
        }
        if let Some(entry) = origin {
            super::jump::push_entry(stoat, entry);
        }
    }
}
