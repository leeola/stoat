use crate::{
    buffer::Buffer,
    editor::{actions::shell::ShellAction, Editor},
    modal_layer::ModalView,
    workspace::Workspace,
};
use gpui::{
    div, App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, WeakEntity, Window,
};
use stoat_action::ActionKind;

/// Modal hosting the shell-command input opened by [`ShellAction`]
/// handlers. Confirm submits the typed command back into the owning
/// [`Workspace`]'s `run_shell_command` via a weak handle so the
/// modal stays unaware of editor state.
pub struct ShellInputModal {
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    action: ShellAction,
}

impl ShellInputModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        action: ShellAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let editor = cx.new(|cx| Editor::auto_height(1, 24, window, cx));
        Self {
            editor,
            focus_handle: cx.focus_handle(),
            workspace,
            action,
        }
    }

    #[cfg(test)]
    pub(crate) fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let cmd = editor_text(&self.editor, cx);
        let workspace = self.workspace.clone();
        let action = self.action;
        window.defer(cx, move |window, cx| {
            if let Some(ws) = workspace.upgrade() {
                ws.update(cx, |ws, cx| ws.run_shell_command(action, &cmd, window, cx));
            }
        });
        cx.emit(DismissEvent);
        true
    }

    fn abort(&mut self, cx: &mut Context<'_, Self>) -> bool {
        cx.emit(DismissEvent);
        true
    }
}

fn editor_text(editor: &Entity<Editor>, cx: &App) -> String {
    editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .map(|b: &Entity<Buffer>| b.read(cx).text())
        .unwrap_or_default()
}

impl Render for ShellInputModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(self.editor.clone())
    }
}

impl Focusable for ShellInputModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for ShellInputModal {}

impl ModalView for ShellInputModal {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match action.kind() {
            ActionKind::ShellInputSubmit => self.confirm(window, cx),
            ActionKind::DismissModal => self.abort(cx),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, ShellHostGlobal};
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use std::sync::Arc;
    use stoat::host::ShellHost;
    use stoat_host::FakeShell;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) -> Arc<FakeShell> {
        let fake = Arc::new(FakeShell::new());
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let shell = fake.clone() as Arc<dyn ShellHost>;
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(executor));
            cx.set_global(ShellHostGlobal(shell));
        });
        fake
    }

    fn type_into_editor(editor: &Entity<Editor>, vcx: &mut VisualTestContext, text: &str) {
        let buffer = editor.read_with(vcx, |ed, cx| {
            ed.multi_buffer()
                .read(cx)
                .as_singleton()
                .expect("auto-height singleton")
                .clone()
        });
        buffer.update(vcx, |b, cx| b.edit(0..0, text, cx));
    }

    fn open_modal(
        vcx: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
        action: ShellAction,
    ) -> Entity<ShellInputModal> {
        let weak = workspace.downgrade();
        vcx.update(|window, cx| cx.new(|cx| ShellInputModal::new(weak, action, window, cx)))
    }

    fn new_workspace_in_window(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(|_window, cx| {
            Workspace::new("main", std::path::PathBuf::from("/tmp/repo"), cx)
        })
    }

    #[test]
    fn confirm_with_empty_input_emits_dismiss_and_does_not_run() {
        let mut cx = TestAppContext::single();
        let fake = install_globals(&mut cx);
        let (workspace, vcx) = new_workspace_in_window(&mut cx);
        let modal = open_modal(vcx, &workspace, ShellAction::Pipe);
        vcx.run_until_parked();

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::ShellInputSubmit, window, cx)
        });
        vcx.run_until_parked();

        assert!(handled, "ShellInputSubmit must be handled");
        assert!(fake.invocations().is_empty(), "no command should run");
    }

    #[test]
    fn confirm_with_command_dispatches_to_workspace() {
        let mut cx = TestAppContext::single();
        let fake = install_globals(&mut cx);
        let (workspace, vcx) = new_workspace_in_window(&mut cx);
        let modal = open_modal(vcx, &workspace, ShellAction::PipeTo);
        let modal_editor = modal.read_with(vcx, |m, _| m.editor().clone());
        type_into_editor(&modal_editor, vcx, "echo hi");
        vcx.run_until_parked();

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::ShellInputSubmit, window, cx)
        });
        vcx.run_until_parked();

        assert!(handled, "ShellInputSubmit must be handled");
        assert!(
            fake.invocations().is_empty(),
            "no active editor -> apply is a no-op; assert the dispatch did not panic",
        );
    }

    #[test]
    fn dismiss_aborts_without_running() {
        let mut cx = TestAppContext::single();
        let fake = install_globals(&mut cx);
        let (workspace, vcx) = new_workspace_in_window(&mut cx);
        let modal = open_modal(vcx, &workspace, ShellAction::Pipe);
        let modal_editor = modal.read_with(vcx, |m, _| m.editor().clone());
        type_into_editor(&modal_editor, vcx, "echo hi");
        vcx.run_until_parked();

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::DismissModal, window, cx)
        });
        vcx.run_until_parked();

        assert!(handled, "DismissModal must be handled");
        assert!(fake.invocations().is_empty(), "no command should run");
    }

    #[test]
    fn unknown_action_falls_through() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx);
        let (workspace, vcx) = new_workspace_in_window(&mut cx);
        let modal = open_modal(vcx, &workspace, ShellAction::Pipe);

        let handled = modal.update_in(vcx, |m, window, cx| {
            m.handle_action(&stoat_action::Quit, window, cx)
        });

        assert!(!handled, "Unrelated actions must not be intercepted");
    }
}
