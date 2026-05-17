//! Multi-cursor action handlers: collapse / flip / select-all /
//! select-line-below / keep-or-remove primary / rotate / trim /
//! align / split-on-newline / add-selection-above-below, plus
//! the regex-prompted SplitSelection / KeepSelections /
//! RemoveSelections. Each `handle_*` function looks up the
//! workspace's active editor and delegates to the matching
//! [`crate::editor::Editor`] helper, except the three regex
//! variants, which open a [`crate::editor::regex_input_modal::RegexInputModal`].
//!
//! `SelectAllSiblings` / `SelectAllChildren` live in
//! [`crate::editor::actions::treesitter`] and are not handled
//! here.

use crate::{
    editor::{
        regex_input_modal::{RegexInputKind, RegexInputModal},
        AddDirection, Editor,
    },
    workspace::Workspace,
};
use gpui::{Context, Entity, Window};

fn active_editor(workspace: &Workspace, cx: &Context<'_, Workspace>) -> Option<Entity<Editor>> {
    workspace
        .input_state_machine()
        .read(cx)
        .active_editor()
        .cloned()
        .and_then(|w| w.upgrade())
}

pub fn handle_collapse_selection(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.collapse_selection(cx));
    }
}

pub fn handle_flip_selections(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.flip_selections(cx));
    }
}

pub fn handle_select_all(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.select_all(cx));
    }
}

pub fn handle_select_line_below(
    workspace: &mut Workspace,
    count: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.select_line_below(count, cx));
    }
}

pub fn handle_keep_primary_selection(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.keep_primary_selection(cx));
    }
}

pub fn handle_remove_primary_selection(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.remove_primary_selection(cx));
    }
}

pub fn handle_rotate_selections_forward(
    workspace: &mut Workspace,
    count: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.rotate_selections(true, count, cx));
    }
}

pub fn handle_rotate_selections_backward(
    workspace: &mut Workspace,
    count: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.rotate_selections(false, count, cx));
    }
}

pub fn handle_trim_selections(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.trim_selections(cx));
    }
}

pub fn handle_split_selection_on_newline(
    workspace: &mut Workspace,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.split_selection_on_newline(cx));
    }
}

pub fn handle_align_selections(workspace: &mut Workspace, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        editor.update(cx, |ed, cx| ed.align_selections(cx));
    }
}

pub fn handle_add_selection_below(
    workspace: &mut Workspace,
    count: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        let target = count.max(1);
        for _ in 0..target {
            let advanced =
                editor.update(cx, |ed, cx| ed.add_selection_step(AddDirection::Below, cx));
            if !advanced {
                break;
            }
        }
    }
}

pub fn handle_add_selection_above(
    workspace: &mut Workspace,
    count: u32,
    cx: &mut Context<'_, Workspace>,
) {
    if let Some(editor) = active_editor(workspace, cx) {
        let target = count.max(1);
        for _ in 0..target {
            let advanced =
                editor.update(cx, |ed, cx| ed.add_selection_step(AddDirection::Above, cx));
            if !advanced {
                break;
            }
        }
    }
}

/// Open the regex-input modal for `kind`. Confirm runs the
/// matching [`Editor`] helper against the active editor.
fn open_regex_modal(
    workspace: &mut Workspace,
    kind: RegexInputKind,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let Some(editor) = active_editor(workspace, cx) else {
        return;
    };
    let weak_editor = editor.downgrade();
    workspace.modal_layer().update(cx, |layer, cx| {
        layer.toggle_modal(window, cx, |window, cx| {
            RegexInputModal::new(weak_editor, kind, window, cx)
        });
    });
}

pub fn handle_split_selection(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_regex_modal(workspace, RegexInputKind::Split, window, cx);
}

pub fn handle_keep_selections(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_regex_modal(workspace, RegexInputKind::Keep, window, cx);
}

pub fn handle_remove_selections(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_regex_modal(workspace, RegexInputKind::Remove, window, cx);
}
