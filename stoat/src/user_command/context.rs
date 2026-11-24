use super::{ContextValue, TextDefault};
use crate::{
    action_handlers::yank, app::Stoat, command_palette, editor_state::EditorId, pane::View,
};

/// The text that `default` gives an empty argument.
///
/// A literal is its own text. A context value reads the editor as [`resolve`]
/// reads it. The palette's ghost and the run both call this, so the value on
/// show is the value that runs.
pub(crate) fn default_text(stoat: &mut Stoat, default: &TextDefault) -> String {
    match default {
        TextDefault::Literal(text) => text.clone(),
        TextDefault::Context(value) => resolve(stoat, *value),
    }
}

/// The text of the context value `value`, or the empty string when there is
/// nothing to read.
///
/// The selection, the file, and the line come from the editor of the
/// workspace's focused split pane, the pane under the palette. The palette's
/// own input takes the keys while the palette is open, and it never answers.
pub(crate) fn resolve(stoat: &mut Stoat, value: ContextValue) -> String {
    let text = match value {
        ContextValue::GitLastSubject => last_subject(stoat),
        ContextValue::Selection => {
            pane_editor(stoat).and_then(|editor| yank::selection_text(stoat, editor))
        },
        ContextValue::File => pane_editor(stoat).and_then(|editor| pane_file(stoat, editor)),
        ContextValue::Line => pane_editor(stoat).and_then(|editor| cursor_line(stoat, editor)),
    };
    text.unwrap_or_default()
}

/// The summary of the newest commit of the workspace's repository.
fn last_subject(stoat: &Stoat) -> Option<String> {
    let ws = stoat.active_workspace();
    let repo = stoat.git_host.discover(&ws.git_root)?;
    let commit = repo.log_commits(None, 1).into_iter().next()?;
    Some(commit.summary)
}

/// The editor of the workspace's focused split pane, or `None` for a pane
/// that shows no editor.
fn pane_editor(stoat: &Stoat) -> Option<EditorId> {
    let ws = stoat.active_workspace();
    match &ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => Some(*id),
        _ => None,
    }
}

/// The path of the buffer that `editor` shows, relative to the workspace root
/// when it lies under the root.
fn pane_file(stoat: &Stoat, editor: EditorId) -> Option<String> {
    let ws = stoat.active_workspace();
    let buffer = ws.editors.get(editor)?.buffer_id;
    let path = ws.buffers.path_for(buffer)?;
    Some(command_palette::shell_path(path, &ws.git_root))
}

/// The 1-based line of the primary cursor of `editor`.
///
/// A forward selection draws its cursor on its last char, one char before
/// its head. So a selection that ends at a line break reads the line that it
/// covers.
fn cursor_line(stoat: &mut Stoat, editor: EditorId) -> Option<String> {
    let editor = stoat.active_workspace_mut().editors.get_mut(editor)?;
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let primary = editor.selections.newest_anchor();
    let tail = buffer.resolve_anchor(&primary.tail());
    let head = buffer.resolve_anchor(&primary.head());
    let cursor = stoat_text::cursor_offset(buffer.rope(), tail, head);
    Some((buffer.rope().offset_to_point(cursor).row + 1).to_string())
}
