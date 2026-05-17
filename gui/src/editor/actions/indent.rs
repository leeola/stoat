//! Indent / unindent / line-comment action handlers.
//!
//! `IndentSelection` and `UnindentSelection` prepend or trim
//! whitespace at the start of every line touched by any
//! selection; the pending count from
//! [`crate::workspace::Workspace::take_count`] is the number of
//! indent groups to apply (default 1). `ToggleComments` looks up
//! the active buffer's language via the global
//! [`crate::globals::LanguageRegistry`], reads its
//! [`stoat_language::Language::line_comment`] prefix, and toggles
//! that prefix on each touched line via
//! [`crate::editor::Editor::toggle_line_comments`].
//!
//! Buffers whose path resolves to no registered language, or
//! whose language has no `line_comment` set, are a `ToggleComments`
//! no-op.

use crate::{editor::Editor, globals::LanguageRegistry, workspace::Workspace};
use gpui::{Context, Entity};

fn active_editor(workspace: &Workspace, cx: &Context<'_, Workspace>) -> Option<Entity<Editor>> {
    workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
}

pub fn handle_indent_selection(
    workspace: &mut Workspace,
    count: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.indent_lines(count, cx));
    }
}

pub fn handle_unindent_selection(
    workspace: &mut Workspace,
    count: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.unindent_lines(count, cx));
    }
}

pub fn handle_toggle_comments(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let Some(path) = editor.read(cx).file_path().map(|p| p.to_path_buf()) else {
        return;
    };
    let Some(language) = cx
        .try_global::<LanguageRegistry>()
        .and_then(|reg| reg.0.for_path(&path))
    else {
        return;
    };
    let Some(prefix) = language.line_comment else {
        return;
    };
    let prefix = prefix.to_string();
    editor.update(cx, |ed, cx| ed.toggle_line_comments(&prefix, cx));
}
