use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_git_status_dismiss(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let (_prev_mode, prev_ctx) = self.app_state.dismiss_git_status();

            if let Some(previous_context) = prev_ctx {
                editor.update(cx, |editor, cx| {
                    editor.stoat.update(cx, |stoat, cx| {
                        stoat.handle_set_key_context(previous_context, cx);
                    });
                });
            }

            cx.notify();
        }
    }
}
