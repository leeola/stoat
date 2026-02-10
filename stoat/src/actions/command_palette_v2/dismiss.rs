use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Focusable, Window};

impl PaneGroupView {
    pub(crate) fn handle_dismiss_command_palette_v2(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let previous_key_context = self
                .app_state
                .command_palette_v2
                .as_ref()
                .and_then(|p| p.read(cx).previous_key_context());

            self.app_state.command_palette_v2 = None;

            if let Some(prev_context) = previous_key_context {
                editor.update(cx, |editor, cx| {
                    editor.stoat.update(cx, |stoat, _| {
                        stoat.set_key_context(prev_context);
                        stoat.sync_mode_to_context(&self.app_state);
                    });
                });
            }

            let editor_focus = editor.read(cx).focus_handle(cx);
            window.focus(&editor_focus);

            cx.notify();
        }
    }
}
