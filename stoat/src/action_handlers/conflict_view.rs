use super::focused_editor_mut;
use crate::{
    app::Stoat,
    conflict_session::{ConflictSession, FileResolveState},
    editor_state::EditorState,
    jumplist::JumpEntry,
    merge_view::{MergeDoc, RowPick},
    pane::View,
};
use std::path::Path;
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
    let path = files[current].clone();

    let Some(stages) = repo.conflict_stages(&path) else {
        stoat.set_status("no merge conflicts");
        return;
    };
    let language = stoat.language_registry.for_path(&path);
    let doc = MergeDoc::build(
        stages.ancestor.as_deref().unwrap_or(""),
        stages.ours.as_deref().unwrap_or(""),
        stages.theirs.as_deref().unwrap_or(""),
        language.as_ref(),
    );
    let (center_text, chunk_ranges) = doc.initial_center_text();
    let first_chunk_offset = chunk_ranges.first().map(|r| r.start);

    let origin = super::jump::live_entry(stoat);
    let executor = stoat.executor.clone();

    {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let View::Editor(saved_editor) = ws.panes.pane(focused).view else {
            return;
        };

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

        let mut editor = EditorState::new(buffer_id, buffer, executor);
        editor.conflict_view = true;
        let editor_id = ws.editors.insert(editor);

        ws.panes.pane_mut(focused).view = View::Editor(editor_id);
        ws.panes.widen(focused);

        let picks = doc
            .chunks
            .iter()
            .map(|chunk| {
                vec![
                    RowPick {
                        ours: false,
                        theirs: false
                    };
                    chunk.row_range.end - chunk.row_range.start
                ]
            })
            .collect();

        ws.conflict = Some(ConflictSession {
            workdir: git_root,
            files,
            current,
            file: FileResolveState {
                path,
                doc,
                picks,
                chunk_anchors,
                buffer_id,
                editor_id,
            },
            saved_editor,
        });
    }

    land_first_chunk(stoat, first_chunk_offset, origin);
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
