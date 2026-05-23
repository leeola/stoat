//! Single-line modal that renames the active workspace.
//!
//! Opened by `Workspace::dispatch_action` for
//! `ActionKind::RenameWorkspace` when the action carries an empty
//! name (the parameterless keymap invocation). Submitting applies
//! the typed text via `Workspace::set_name`, which updates the
//! status-bar label and emits `WorkspaceEvent::NameChanged`.
//!
//! Modeled on the LSP `RenameModal`: a single-line `Editor`
//! seeded with the current workspace name, `PickerConfirm` to
//! commit, `DismissModal` to cancel.

use crate::{
    buffer::Buffer,
    editor::{Editor, EditorEvent},
    modal_layer::ModalView,
    workspace::Workspace,
};
use gpui::{
    div, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Subscription, WeakEntity, Window,
};
use stoat_action::ActionKind;

pub struct RenameWorkspaceModal {
    input: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl RenameWorkspaceModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::single_line(window, cx));
        if !placeholder.is_empty() {
            seed_editor_text(&input, placeholder, cx);
        }
        let forward_changed = cx.subscribe(&input, |_this, _ed, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Changed) {
                cx.notify();
            }
        });
        Self {
            input,
            workspace,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![forward_changed],
        }
    }

    #[cfg(test)]
    pub fn input_editor_for_test(&self) -> Entity<Editor> {
        self.input.clone()
    }

    fn current_text(&self, cx: &gpui::App) -> String {
        let editor = self.input.read(cx);
        editor
            .multi_buffer()
            .read(cx)
            .as_singleton()
            .map(|b| b.read(cx).text())
            .unwrap_or_default()
    }

    fn confirm(&mut self, cx: &mut Context<'_, Self>) {
        let name = self.current_text(cx);
        let Some(workspace) = self.workspace.upgrade() else {
            cx.emit(DismissEvent);
            return;
        };
        workspace.update(cx, |w, cx| {
            w.set_name(name, cx);
        });
        cx.emit(DismissEvent);
    }
}

fn seed_editor_text(input: &Entity<Editor>, text: &str, cx: &mut gpui::App) {
    let Some(buffer) = input
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .cloned()
    else {
        return;
    };
    let len = buffer.read(cx).read(|tb| tb.rope().len());
    buffer.update(cx, |b: &mut Buffer, cx| {
        b.edit(0..len, text, cx);
    });
}

impl Render for RenameWorkspaceModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .child(self.input.clone())
    }
}

impl Focusable for RenameWorkspaceModal {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RenameWorkspaceModal {}

impl ModalView for RenameWorkspaceModal {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match action.kind() {
            ActionKind::PickerConfirm => {
                self.confirm(cx);
                true
            },
            ActionKind::DismissModal => {
                cx.emit(DismissEvent);
                true
            },
            _ => false,
        }
    }
}

/// Open the rename-workspace modal seeded with the current name.
pub fn open_rename_workspace(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let weak = cx.weak_entity();
    let placeholder = workspace.name().to_string();
    workspace.toggle_modal::<RenameWorkspaceModal, _>(window, cx, move |window, cx| {
        RenameWorkspaceModal::new(weak, &placeholder, window, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/r"), cx))
    }

    #[test]
    fn confirm_applies_typed_name_to_workspace() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);

        ws.update_in(vcx, |w, window, cx| {
            open_rename_workspace(w, window, cx);
        });

        let modal: Entity<RenameWorkspaceModal> = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<RenameWorkspaceModal>()
            })
            .expect("rename modal active");

        let editor = modal.read_with(vcx, |m, _| m.input_editor_for_test());
        vcx.update(|_, cx| seed_editor_text(&editor, "foo", cx));

        modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::PickerConfirm, window, cx);
        });
        vcx.run_until_parked();

        let name = ws.read_with(vcx, |w, _| w.name().clone());
        assert_eq!(name.as_ref(), "foo");
    }

    #[test]
    fn open_seeds_input_with_current_workspace_name() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);

        ws.update_in(vcx, |w, window, cx| {
            open_rename_workspace(w, window, cx);
        });

        let modal: Entity<RenameWorkspaceModal> = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<RenameWorkspaceModal>()
            })
            .expect("rename modal active");

        let text = modal.read_with(vcx, |m, cx| m.current_text(cx));
        assert_eq!(text, "main");
    }
}
