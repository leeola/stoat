use crate::{pane_group::view::PaneGroupView, stoat::KeyContext};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_open_command_palette(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let (current_mode, current_key_context) = {
                let stoat = editor.read(cx).stoat.read(cx);
                (stoat.mode().to_string(), stoat.key_context())
            };

            self.app_state
                .open_command_palette(current_mode, current_key_context, cx);

            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, _cx| {
                    stoat.set_key_context(KeyContext::CommandPalette);
                    stoat.set_mode("command_palette");
                    stoat.command_palette_input_ref = self.app_state.command_palette.input.clone();
                });
            });

            cx.notify();
        }
    }
}
