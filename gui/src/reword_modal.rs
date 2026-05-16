use crate::{buffer::Buffer, editor::Editor, modal_layer::ModalView, workspace::Workspace};
use gpui::{
    div, App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, WeakEntity, Window,
};
use stoat_action::ActionKind;

/// Modal that hosts the commit-message editor opened from a
/// [`stoat::rebase::RebasePause::Reword`] pause. Pre-seeds the
/// editor with the original commit message; confirm and abort flow
/// back into the owning [`Workspace`] via a weak handle so the
/// modal stays unaware of `rebase_active` state.
pub struct RewordModal {
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
}

impl RewordModal {
    /// Build a fresh modal with an auto-height editor seeded with
    /// `original_message`. The editor receives focus on the next
    /// frame via the modal layer's deferred focus call.
    pub fn new(
        workspace: WeakEntity<Workspace>,
        original_message: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let editor = cx.new(|cx| Editor::auto_height(1, 24, window, cx));
        seed_editor_text(&editor, original_message, cx);
        Self {
            editor,
            focus_handle: cx.focus_handle(),
            workspace,
        }
    }

    #[cfg(test)]
    pub(crate) fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let text = editor_text(&self.editor, cx);
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            if let Some(ws) = workspace.upgrade() {
                ws.update(cx, |ws, cx| ws.commit_reword(text, window, cx));
            }
        });
        cx.emit(DismissEvent);
        true
    }

    fn abort(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let workspace = self.workspace.clone();
        window.defer(cx, move |_, cx| {
            if let Some(ws) = workspace.upgrade() {
                ws.update(cx, |ws, cx| ws.abort_reword(cx));
            }
        });
        cx.emit(DismissEvent);
        true
    }
}

fn seed_editor_text(editor: &Entity<Editor>, text: &str, cx: &mut Context<'_, RewordModal>) {
    if text.is_empty() {
        return;
    }
    let buffer: Entity<Buffer> = editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .expect("auto-height editor has a singleton buffer")
        .clone();
    buffer.update(cx, |b, cx| b.edit(0..0, text, cx));
}

fn editor_text(editor: &Entity<Editor>, cx: &App) -> String {
    editor
        .read(cx)
        .multi_buffer()
        .read(cx)
        .as_singleton()
        .map(|b| b.read(cx).text())
        .unwrap_or_default()
}

impl Render for RewordModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(self.editor.clone())
    }
}

impl Focusable for RewordModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RewordModal {}

impl ModalView for RewordModal {
    fn handle_action(
        &mut self,
        action: &dyn stoat_action::Action,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match action.kind() {
            ActionKind::RewordConfirm => self.confirm(window, cx),
            ActionKind::RewordAbort | ActionKind::DismissModal => self.abort(window, cx),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    #[test]
    fn editor_seeded_with_original_message() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let workspace = cx.update(|cx| {
            cx.new(|cx| Workspace::new("main", std::path::PathBuf::from("/tmp/repo"), cx))
        });
        let weak = workspace.downgrade();
        let vcx = cx.add_empty_window();
        let modal = vcx.update(|window, cx| {
            cx.new(|cx| RewordModal::new(weak, "first line\nsecond line", window, cx))
        });
        vcx.run_until_parked();

        let text = modal.read_with(vcx, |m, cx| editor_text(m.editor(), cx));
        assert_eq!(text, "first line\nsecond line");
    }

    #[test]
    fn editor_seeded_with_empty_message_is_empty() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let workspace = cx.update(|cx| {
            cx.new(|cx| Workspace::new("main", std::path::PathBuf::from("/tmp/repo"), cx))
        });
        let weak = workspace.downgrade();
        let vcx = cx.add_empty_window();
        let modal = vcx.update(|window, cx| cx.new(|cx| RewordModal::new(weak, "", window, cx)));
        vcx.run_until_parked();

        let text = modal.read_with(vcx, |m, cx| editor_text(m.editor(), cx));
        assert_eq!(text, "");
    }
}
