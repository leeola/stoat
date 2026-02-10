use crate::{pane_group::view::PaneGroupView, stoat::StoatEvent};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_command_line_dismiss(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let prev_mode = self.app_state.command_line.previous_mode.take();
            let prev_ctx = self.app_state.command_line.previous_key_context.take();

            self.app_state.command_line.input = None;

            if let (Some(mode), Some(ctx)) = (prev_mode, prev_ctx) {
                let stoat = editor.read(cx).stoat.clone();
                stoat.update(cx, |s, cx| {
                    s.set_key_context(ctx);
                    s.set_mode(&mode);
                    cx.emit(StoatEvent::Changed);
                    cx.notify();
                });
            }
        }

        cx.notify();
    }
}
