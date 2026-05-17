//! Number adjust action handlers.
//!
//! `Increment` and `Decrement` walk every cursor's head to the
//! nearest number on the same line via
//! [`stoat_text::find_number_seeking`] and apply the delta
//! through
//! [`crate::editor::Editor::handle_number_delta`]. Pending count
//! flows in from `Workspace::take_count` so `5<Increment>` adds
//! 5 to every reachable number. Decimal numbers carry their
//! sign; hex / binary / octal are unsigned and saturate at the
//! `u64` boundary. Two cursors that land on the same number
//! range share a single edit.
//!
//! The dispatch arms live in [`crate::workspace::Workspace`]
//! and call the two functions below; the mutation lives on
//! [`crate::editor::Editor`].

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

pub fn handle_increment(workspace: &mut Workspace, count: u32, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        let delta = count.max(1) as i64;
        editor.update(cx, |ed, cx| ed.handle_number_delta(delta, cx));
    }
}

pub fn handle_decrement(workspace: &mut Workspace, count: u32, cx: &mut Context<'_, Workspace>) {
    if let Some(editor) = active_editor(workspace, cx) {
        let delta = -(count.max(1) as i64);
        editor.update(cx, |ed, cx| ed.handle_number_delta(delta, cx));
    }
}
