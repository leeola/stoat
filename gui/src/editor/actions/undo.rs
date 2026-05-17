//! Editor history action handlers: undo, redo, and
//! commit-undo-checkpoint.
//!
//! Each `handle_*` function looks up the workspace's active
//! editor and delegates to the matching [`crate::editor::Editor`]
//! helper, which in turn drives the
//! [`stoat::buffer::TextBuffer`] history pipeline and re-anchors
//! selections through [`stoat::selection::SelectionsCollection::transform`].

use crate::{editor::Editor, workspace::Workspace};
use gpui::{Context, Entity};

fn active_editor(workspace: &Workspace, cx: &Context<'_, Workspace>) -> Option<Entity<Editor>> {
    workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
}

/// `Undo` action: undo up to `count` edits in the active buffer.
/// Stops early when the history is exhausted.
pub fn handle_undo(workspace: &mut Workspace, count: u32, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.handle_undo(count, cx);
        });
    }
}

/// `Redo` action: re-apply up to `count` previously undone edits.
pub fn handle_redo(workspace: &mut Workspace, count: u32, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.handle_redo(count, cx);
        });
    }
}

/// `CommitUndoCheckpoint` action: record an unlabeled checkpoint
/// in the active buffer's op log. Future labeled-checkpoint
/// callers can plumb their label through
/// [`Editor::commit_checkpoint`] directly.
pub fn handle_commit_checkpoint(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.commit_checkpoint(None, cx);
        });
    }
}
