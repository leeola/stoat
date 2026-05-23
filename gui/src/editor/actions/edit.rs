//! Editor edit action handlers: yank, paste, delete, insert, append.
//!
//! Each `handle_*` free function takes a `Workspace` plus the
//! workspace's gpui context and routes the work through:
//!
//! - the workspace's [`stoat::register::RegisterStore`] for unnamed and named register operations,
//! - [`crate::globals::ClipboardHostGlobal`] for system-clipboard variants, and
//! - the active editor's mutation helpers ([`crate::editor::Editor::delete_selections`],
//!   [`crate::editor::Editor::paste_at_selections`], etc.) for the actual buffer edits.
//!
//! Workspace dispatch arms call these in their
//! `ActionKind::Yank` / `PasteAfter` / etc. branches.

use crate::{
    editor::{DeleteDirection, Editor, OpenLineDir, PastePosition},
    globals::ClipboardHostGlobal,
    workspace::Workspace,
};
use gpui::{Context, Entity, Window};
use stoat::register::Register;

/// Look up the currently active editor for the workspace, returning
/// `None` when no editor is focused (e.g. an empty pane tree or a
/// non-editor item like a Run pane).
fn active_editor(workspace: &Workspace, cx: &Context<'_, Workspace>) -> Option<Entity<Editor>> {
    workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
}

/// `Yank` action: copy each selection's covered text into the
/// workspace's pending register (defaults to the unnamed register).
pub fn handle_yank(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let Some(payload) = editor.read(cx).yank_payload(cx) else {
        return;
    };
    let register = workspace.consume_selected_register();
    write_register(workspace, register, payload, cx);
}

/// `PasteAfter` action: insert the pending register's contents
/// immediately after each selection's end offset.
pub fn handle_paste_after(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let register = workspace.consume_selected_register();
    let Some(payload) = read_register(workspace, register, cx) else {
        return;
    };
    editor.update(cx, |ed, cx| {
        ed.paste_at_selections(&payload, PastePosition::After, cx)
    });
}

/// `PasteBefore` action: insert the pending register's contents
/// immediately before each selection's start offset.
pub fn handle_paste_before(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let register = workspace.consume_selected_register();
    let Some(payload) = read_register(workspace, register, cx) else {
        return;
    };
    editor.update(cx, |ed, cx| {
        ed.paste_at_selections(&payload, PastePosition::Before, cx)
    });
}

/// `YankToClipboard` action: copy every selection's text (joined by
/// newline) to the system clipboard via
/// [`ClipboardHostGlobal`]. No-op when no selection has non-empty
/// content.
pub fn handle_yank_to_clipboard(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let Some(payload) = editor.read(cx).yank_payload(cx) else {
        return;
    };
    let Some(clipboard) = cx.try_global::<ClipboardHostGlobal>().map(|g| g.0.clone()) else {
        return;
    };
    if let Err(err) = clipboard.set(&payload) {
        tracing::warn!(target: "stoat_gui::actions::edit", ?err, "clipboard set failed");
    }
}

/// `YankMainToClipboard` action: copy the primary selection's
/// covered text to the system clipboard. The primary is the
/// newest selection per
/// [`stoat::selection::SelectionsCollection::all_anchors`] ordering.
pub fn handle_yank_main_to_clipboard(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let payload = {
        let ed = editor.read(cx);
        let snapshot = ed.multi_buffer().read(cx).snapshot();
        let primary = ed
            .selections()
            .all_anchors()
            .iter()
            .max_by_key(|s| s.id)
            .cloned();
        primary.and_then(|sel| {
            let start = snapshot.resolve_anchor(&sel.start);
            let end = snapshot.resolve_anchor(&sel.end);
            let (lo, hi) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            (lo != hi).then(|| snapshot.rope().slice(lo..hi).to_string())
        })
    };
    let Some(payload) = payload else {
        return;
    };
    let Some(clipboard) = cx.try_global::<ClipboardHostGlobal>().map(|g| g.0.clone()) else {
        return;
    };
    if let Err(err) = clipboard.set(&payload) {
        tracing::warn!(target: "stoat_gui::actions::edit", ?err, "clipboard set failed");
    }
}

/// `PasteClipboardAfter` action: read the system clipboard and
/// insert its contents after every selection. No-op when the
/// clipboard is empty or unreadable.
pub fn handle_paste_clipboard_after(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(payload) = read_clipboard(cx) {
        if let Some(editor) = active_editor(workspace, cx) {
            editor.update(cx, |ed, cx| {
                ed.paste_at_selections(&payload, PastePosition::After, cx)
            });
        }
    }
}

/// `PasteClipboardBefore` action: read the system clipboard and
/// insert its contents before every selection.
pub fn handle_paste_clipboard_before(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(payload) = read_clipboard(cx) {
        if let Some(editor) = active_editor(workspace, cx) {
            editor.update(cx, |ed, cx| {
                ed.paste_at_selections(&payload, PastePosition::Before, cx)
            });
        }
    }
}

/// `DeleteSelection` action: delete every non-empty selection and
/// collapse it to a cursor at the deletion's start.
pub fn handle_delete_selection(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.delete_selections(cx));
    }
}

/// `DeleteForward` action: delete the UTF-8 character at or after
/// each cursor. Non-empty selections delegate to
/// `DeleteSelection`.
pub fn handle_delete_forward(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.delete_around_cursors(DeleteDirection::Forward, cx)
        });
    }
}

/// `DeleteBackward` action: delete the UTF-8 character before each
/// cursor.
pub fn handle_delete_backward(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.delete_around_cursors(DeleteDirection::Backward, cx)
        });
    }
}

/// `DeleteWordForward` action: delete from each cursor forward to the
/// next word boundary. Non-empty selections delegate to
/// `DeleteSelection`.
pub fn handle_delete_word_forward(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.delete_word_around_cursors(DeleteDirection::Forward, cx)
        });
    }
}

/// `DeleteWordBackward` action: delete from each cursor back to the
/// previous word boundary. Non-empty selections delegate to
/// `DeleteSelection`.
pub fn handle_delete_word_backward(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.delete_word_around_cursors(DeleteDirection::Backward, cx)
        });
    }
}

/// `Insert` action: collapse selections to their start anchor and
/// switch the workspace into insert mode.
pub fn handle_insert(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.collapse_selections_to_start(cx));
    }
    workspace
        .input_state_machine()
        .update(cx, |sm, cx| sm.transition_mode("insert", window, cx));
}

/// `Append` action: collapse selections to their end anchor and
/// switch the workspace into insert mode.
pub fn handle_append(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.collapse_selections_to_end(cx));
    }
    workspace
        .input_state_machine()
        .update(cx, |sm, cx| sm.transition_mode("insert", window, cx));
}

/// `InsertNewline` action: insert a line-feed at every cursor /
/// selection range.
pub fn handle_insert_newline(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.apply_text_to_all_cursors("\n", cx));
    }
}

/// `OpenBelow` action: insert a blank line after each selection's
/// head row and collapse every selection to a cursor on that new
/// blank line. The keymap's `SetMode(insert)` step that follows
/// switches the workspace into insert mode.
pub fn handle_open_below(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.open_line(OpenLineDir::Below, cx));
    }
}

/// `OpenAbove` action: insert a blank line before each selection's
/// head row and collapse every selection to a cursor on that new
/// blank line. The keymap's `SetMode(insert)` step that follows
/// switches the workspace into insert mode.
pub fn handle_open_above(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.open_line(OpenLineDir::Above, cx));
    }
}

/// `SwitchCase` action: flip the case of every alphabetic char in
/// each non-empty selection (lowercase to uppercase and vice
/// versa). Selections that are collapsed cursors are skipped.
pub fn handle_switch_case(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.transform_selections_text(toggle_case, cx));
    }
}

/// `SwitchToUppercase` action: uppercase the text of every
/// non-empty selection.
pub fn handle_switch_to_uppercase(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.transform_selections_text(|s: &str| s.to_uppercase(), cx)
        });
    }
}

/// `SwitchToLowercase` action: lowercase the text of every
/// non-empty selection.
pub fn handle_switch_to_lowercase(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| {
            ed.transform_selections_text(|s: &str| s.to_lowercase(), cx)
        });
    }
}

/// `ApplyInsertRegisterChar` action: resolve `ch` to a register
/// via [`stoat::action_handlers::yank::register_for_char`] and
/// insert that register's text at every cursor on the active
/// editor. No-op when the char does not map to a register or the
/// register has no readable content.
pub fn handle_insert_register_char(
    workspace: &mut Workspace,
    ch: char,
    cx: &mut Context<'_, Workspace>,
) {
    let Some(register) = stoat::action_handlers::yank::register_for_char(ch) else {
        return;
    };
    let Some(text) = read_register(workspace, register, cx) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.apply_text_to_all_cursors(&text, cx));
    }
}

fn toggle_case(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_lowercase() {
                c.to_uppercase().collect::<Vec<_>>()
            } else if c.is_uppercase() {
                c.to_lowercase().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// Resolve `register` to its backing text and return a copy
/// suitable for paste. Clipboard reads route through the global
/// clipboard host; in-process registers (unnamed, named) come
/// from the workspace store.
fn read_register(
    workspace: &Workspace,
    register: Register,
    cx: &Context<'_, Workspace>,
) -> Option<String> {
    match register {
        Register::Unnamed | Register::Named(_) => {
            workspace.registers().read(register).map(str::to_string)
        },
        Register::Clipboard => read_clipboard(cx),
        Register::Search | Register::Blackhole => None,
        Register::SelectionIndex | Register::LastInsert => None,
    }
}

/// Write `payload` to `register`'s backing store. Clipboard
/// writes route through the global clipboard host; in-process
/// registers update the workspace store.
fn write_register(
    workspace: &mut Workspace,
    register: Register,
    payload: String,
    cx: &Context<'_, Workspace>,
) {
    match register {
        Register::Unnamed | Register::Named(_) => {
            workspace.registers_mut().write(register, payload);
        },
        Register::Clipboard => {
            if let Some(clipboard) = cx.try_global::<ClipboardHostGlobal>().map(|g| g.0.clone()) {
                if let Err(err) = clipboard.set(&payload) {
                    tracing::warn!(target: "stoat_gui::actions::edit", ?err, "clipboard set failed");
                }
            }
        },
        Register::Search | Register::Blackhole => {},
        Register::SelectionIndex | Register::LastInsert => {},
    }
}

fn read_clipboard(cx: &Context<'_, Workspace>) -> Option<String> {
    let clipboard = cx
        .try_global::<ClipboardHostGlobal>()
        .map(|g| g.0.clone())?;
    match clipboard.get() {
        Ok(Some(s)) if !s.is_empty() => Some(s),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(target: "stoat_gui::actions::edit", ?err, "clipboard get failed");
            None
        },
    }
}
