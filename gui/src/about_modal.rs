//! GUI About modal showing the build hash and date captured at
//! compile time via `env!("STOAT_BUILD_INFO")`. Wired into the modal
//! layer by [`crate::workspace::Workspace::dispatch_action`] handling
//! `ActionKind::OpenAbout`.

use crate::{modal_layer::ModalView, theme::ActiveTheme, workspace::Workspace};
use gpui::{
    div, App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, Styled, Window,
};

pub struct AboutModal {
    focus_handle: FocusHandle,
}

impl AboutModal {
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for AboutModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .track_focus(&self.focus_handle)
            .text_color(theme.popup_text)
            .child(div().child("stoat"))
            .child(div().child(env!("STOAT_BUILD_INFO")))
            .child(div().text_color(theme.muted_text).child("Esc to dismiss"))
    }
}

impl Focusable for AboutModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for AboutModal {}

impl ModalView for AboutModal {}

/// Open the About modal as a workspace modal. Constructed in
/// `Workspace::dispatch_action` when `OpenAbout` is dispatched.
pub fn open_about(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<'_, Workspace>) {
    workspace.toggle_modal::<AboutModal, _>(window, cx, |_window, cx| AboutModal::new(cx));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::ExecutorGlobal;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_executor(cx: &mut TestAppContext) {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    fn new_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx))
    }

    #[test]
    fn open_about_pushes_modal_onto_layer() {
        let mut cx = TestAppContext::single();
        install_executor(&mut cx);
        let (workspace, vcx) = new_workspace(&mut cx);

        workspace.update_in(vcx, open_about);
        vcx.run_until_parked();

        let has_modal =
            workspace.read_with(vcx, |ws, cx| ws.modal_layer().read(cx).has_active_modal());
        assert!(has_modal, "About modal must be on the modal layer stack");
    }

    #[test]
    fn build_info_env_var_is_non_empty() {
        let build_info = env!("STOAT_BUILD_INFO");
        assert!(!build_info.is_empty(), "STOAT_BUILD_INFO must be set");
    }
}
