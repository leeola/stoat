//! Project-tree delete confirmation modal.
//!
//! Mirrors [`crate::quit_confirm::QuitConfirmModal`]: confirms on `Enter`,
//! cancels on `Escape`. Opened by
//! [`crate::workspace::Workspace::dispatch_action`] handling
//! `ActionKind::DeleteTreeEntry`; confirming defers to
//! [`crate::workspace::Workspace::delete_tree_path`].

use crate::{modal_layer::ModalView, workspace::Workspace};
use gpui::{
    div, App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Styled, WeakEntity, Window,
};
use std::path::PathBuf;
use stoat_action::ActionKind;

/// Modal asking the user to confirm deletion of one project-tree entry.
/// The path and display data are captured at construction so the render
/// and confirm paths do not walk back into the tree.
pub struct DeleteTreeConfirmModal {
    path: PathBuf,
    name: String,
    is_dir: bool,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
}

impl DeleteTreeConfirmModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        path: PathBuf,
        name: String,
        is_dir: bool,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self {
            path,
            name,
            is_dir,
            focus_handle: cx.focus_handle(),
            workspace,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let workspace = self.workspace.clone();
        let path = self.path.clone();
        let is_dir = self.is_dir;
        // Defer past the keystroke observer's outer `Workspace::update`
        // lease so the re-entrant update does not panic.
        window.defer(cx, move |_window, cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.delete_tree_path(path, is_dir, cx);
                });
            }
        });
        cx.emit(DismissEvent);
        true
    }

    fn cancel(&mut self, cx: &mut Context<'_, Self>) -> bool {
        cx.emit(DismissEvent);
        true
    }
}

impl Render for DeleteTreeConfirmModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let kind = if self.is_dir { "directory" } else { "file" };
        let prompt = SharedString::from(format!("Delete {kind} {}?", self.name));
        div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(div().child("delete tree entry"))
            .child(div().child(prompt))
            .child(div().child("Enter to delete, Esc to cancel"))
    }
}

impl Focusable for DeleteTreeConfirmModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for DeleteTreeConfirmModal {}

impl ModalView for DeleteTreeConfirmModal {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match action.kind() {
            ActionKind::DismissModal => self.cancel(cx),
            _ => false,
        }
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.confirm(window, cx)
    }

    fn cancel_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        self.cancel(cx)
    }
}

/// Open the delete-confirm modal as a workspace modal seeded with the
/// selected entry's path and display data.
pub fn open_delete_tree_confirm(
    workspace: &mut Workspace,
    path: PathBuf,
    name: String,
    is_dir: bool,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let weak = cx.weak_entity();
    workspace.toggle_modal::<DeleteTreeConfirmModal, _>(window, cx, move |_window, cx| {
        DeleteTreeConfirmModal::new(weak, path, name, is_dir, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/r"), cx))
    }

    fn open_modal(
        vcx: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
    ) -> Entity<DeleteTreeConfirmModal> {
        let weak = workspace.downgrade();
        vcx.update(|_window, cx| {
            cx.new(|cx| {
                DeleteTreeConfirmModal::new(
                    weak,
                    PathBuf::from("/r/sub"),
                    "sub".to_string(),
                    true,
                    cx,
                )
            })
        })
    }

    #[test]
    fn submit_prompt_emits_dismiss_and_is_handled() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &ws);

        let handled = modal.update_in(vcx, |m, window, cx| m.submit_prompt(window, cx));
        assert!(handled, "SubmitPromptInput must be handled");
    }

    #[test]
    fn dismiss_modal_is_handled() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &ws);

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::DismissModal, window, cx)
        });
        assert!(handled, "DismissModal must be handled");
    }

    #[test]
    fn unrelated_action_falls_through() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &ws);

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::OpenHelp, window, cx)
        });
        assert!(!handled, "Unrelated actions must not be intercepted");
    }
}
