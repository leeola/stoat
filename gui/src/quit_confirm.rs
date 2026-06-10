//! GUI quit-all confirmation modal.
//!
//! Mirrors the former TUI quit-all-confirm surface:
//! lists every dirty buffer, confirms on `Enter`, cancels on `Escape`.
//! Constructed by [`open_quit_confirm`] only when at least one buffer
//! is dirty -- the dispatch path quits immediately when none are.
//!
//! Wired into the modal layer by
//! [`crate::workspace::Workspace::dispatch_action`] handling
//! `ActionKind::QuitAll`.

use crate::{modal_layer::ModalView, workspace::Workspace};
use gpui::{
    div, App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Styled, WeakEntity, Window,
};
use std::path::Path;
use stoat::{buffer_registry::DirtyBuffer, paths::display_relative};
use stoat_action::ActionKind;

/// Action taken when the user confirms the modal. `QuitApp`
/// closes the whole gpui app; `CloseWindow` removes only the
/// hosting window (the workspace's release observer then runs
/// `save_state_to_default_path` as the entity drops).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    QuitApp,
    CloseWindow,
}

/// Modal listing the dirty buffers staged for discard on `QuitAll`
/// or `CloseWorkspace`. Display strings are captured at construction
/// so the render path does not need to walk back into the workspace.
pub struct QuitConfirmModal {
    entries: Vec<String>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    action: ConfirmAction,
}

impl QuitConfirmModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        dirty: &[DirtyBuffer],
        git_root: &Path,
        action: ConfirmAction,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let entries = dirty
            .iter()
            .map(|d| match &d.path {
                Some(p) => display_relative(p, git_root),
                None => "<scratch>".to_string(),
            })
            .collect();
        Self {
            entries,
            focus_handle: cx.focus_handle(),
            workspace,
            action,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let workspace = self.workspace.clone();
        let action = self.action;
        window.defer(cx, move |window, cx| {
            if let Some(_ws) = workspace.upgrade() {
                match action {
                    ConfirmAction::QuitApp => cx.quit(),
                    ConfirmAction::CloseWindow => window.remove_window(),
                }
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

impl Render for QuitConfirmModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        let prompt = if self.entries.len() == 1 {
            SharedString::from("1 buffer has unsaved changes:")
        } else {
            SharedString::from(format!(
                "{} buffers have unsaved changes:",
                self.entries.len()
            ))
        };
        let rows: Vec<gpui::AnyElement> = self
            .entries
            .iter()
            .map(|entry| div().child(format!(" * {entry}")).into_any_element())
            .collect();
        div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(div().child("unsaved buffers"))
            .child(div().child(prompt))
            .child(div().flex().flex_col().children(rows))
            .child(div().child("Enter to quit, Esc to cancel"))
    }
}

impl Focusable for QuitConfirmModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for QuitConfirmModal {}

impl ModalView for QuitConfirmModal {
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

/// Open the quit-confirm modal as a workspace modal seeded with the
/// dirty buffer list. Caller is expected to have already verified
/// `dirty` is non-empty; this function does not re-check.
pub fn open_quit_confirm(
    workspace: &mut Workspace,
    dirty: &[DirtyBuffer],
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_with_action(workspace, dirty, ConfirmAction::QuitApp, window, cx);
}

/// Open the same modal in close-window mode: confirm removes the
/// hosting window instead of quitting the app.
pub fn open_close_workspace_confirm(
    workspace: &mut Workspace,
    dirty: &[DirtyBuffer],
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    open_with_action(workspace, dirty, ConfirmAction::CloseWindow, window, cx);
}

fn open_with_action(
    workspace: &mut Workspace,
    dirty: &[DirtyBuffer],
    action: ConfirmAction,
    window: &mut Window,
    cx: &mut Context<'_, Workspace>,
) {
    let weak = cx.weak_entity();
    let git_root = workspace.git_root().clone();
    let dirty = dirty.to_vec();
    workspace.toggle_modal::<QuitConfirmModal, _>(window, cx, move |_window, cx| {
        QuitConfirmModal::new(weak, &dirty, &git_root, action, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/r"), cx))
    }

    fn dirty(id: u64, path: Option<&str>) -> DirtyBuffer {
        DirtyBuffer {
            id: BufferId::new(id),
            path: path.map(PathBuf::from),
        }
    }

    fn open_modal(
        vcx: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
        dirty: Vec<DirtyBuffer>,
    ) -> Entity<QuitConfirmModal> {
        let weak = workspace.downgrade();
        vcx.update(|_window, cx| {
            cx.new(|cx| {
                QuitConfirmModal::new(weak, &dirty, Path::new("/r"), ConfirmAction::QuitApp, cx)
            })
        })
    }

    #[test]
    fn new_lists_dirty_paths_relative_to_git_root() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);
        let modal = open_modal(
            vcx,
            &ws,
            vec![dirty(1, Some("/r/a.rs")), dirty(2, Some("/r/sub/b.rs"))],
        );
        let entries: Vec<String> = modal.read_with(vcx, |m, _| m.entries.clone());
        assert_eq!(entries, vec!["a.rs".to_string(), "sub/b.rs".to_string()]);
    }

    #[test]
    fn new_renders_scratch_as_placeholder() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &ws, vec![dirty(1, None)]);
        let entries: Vec<String> = modal.read_with(vcx, |m, _| m.entries.clone());
        assert_eq!(entries, vec!["<scratch>".to_string()]);
    }

    #[test]
    fn submit_prompt_input_emits_dismiss_and_is_handled() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &ws, vec![dirty(1, Some("/r/a.rs"))]);

        let handled = modal.update_in(vcx, |m, window, cx| m.submit_prompt(window, cx));
        assert!(handled, "SubmitPromptInput must be handled");
    }

    #[test]
    fn dismiss_modal_is_handled() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (ws, vcx) = new_workspace(&mut cx);
        let modal = open_modal(vcx, &ws, vec![dirty(1, Some("/r/a.rs"))]);

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
        let modal = open_modal(vcx, &ws, vec![dirty(1, Some("/r/a.rs"))]);

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::OpenHelp, window, cx)
        });
        assert!(!handled, "Unrelated actions must not be intercepted");
    }
}
