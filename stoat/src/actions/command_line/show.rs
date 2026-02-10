use crate::pane_group::view::PaneGroupView;
use gpui::{AppContext, Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_show_command_line(
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

            self.app_state.command_line.previous_mode = Some(current_mode);
            self.app_state.command_line.previous_key_context = Some(current_key_context);

            if self.app_state.command_line.input.is_none() {
                use std::num::NonZeroU64;
                use text::{Buffer, BufferId};

                let buffer_id = BufferId::from(NonZeroU64::new(4).unwrap());
                let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
                self.app_state.command_line.input = Some(input_buffer);
            }
        }

        cx.notify();
    }
}
