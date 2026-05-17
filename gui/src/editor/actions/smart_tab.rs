//! `SmartTab` and `TriggerCompletion` action handlers.
//!
//! `SmartTab` arbitrates the Tab key across three branches in
//! priority order:
//!
//! 1. **Popup acceptance** -- when the active editor's [`crate::lsp::completion::CompletionPopup`]
//!    is visible, `Tab` accepts the highlighted entry.
//! 2. **Indent insertion** -- otherwise, if the primary cursor sits at the start of its line or
//!    after only whitespace (see [`crate::editor::Editor::cursor_after_only_whitespace`]), `Tab`
//!    inserts a single literal `"\t"` at every cursor.
//! 3. **No-op** -- otherwise the action falls through. The Tab key will then run whatever the
//!    keymap binds it to in this mode (typically nothing for normal mode).
//!
//! `TriggerCompletion` invalidates the popup's signature
//! cache and re-runs its reconcile step so a fresh LSP
//! `textDocument/completion` request fires even when the
//! cursor / prefix have not changed.
//!
//! FIXME: SmartTab's snippet-advance branch (the TUI's first
//! priority -- advance to the next tabstop while an active
//! snippet is installed) is not implemented because the gui
//! does not host a snippet subsystem yet.

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

/// Run the SmartTab arbitration against the active editor.
/// See the module-level docs for the branch ordering.
pub fn handle_smart_tab(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };

    let popup = editor.read(cx).completion_popup().cloned();
    if let Some(popup) = popup {
        if popup.read(cx).is_visible() {
            popup.update(cx, |p, cx| {
                p.accept(cx);
            });
            return;
        }
    }

    if editor.read(cx).cursor_after_only_whitespace(cx) {
        editor.update(cx, |ed, cx| ed.apply_text_to_all_cursors("\t", cx));
    }
}

/// Run the TriggerCompletion handler against the active
/// editor's completion popup. No-op when the editor or popup
/// is absent.
pub fn handle_trigger_completion(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let popup = match editor.read(cx).completion_popup().cloned() {
        Some(p) => p,
        None => return,
    };
    popup.update(cx, |p, cx| p.trigger_request(cx));
}
