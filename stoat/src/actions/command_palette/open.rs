use crate::{pane_group::view::PaneGroupView, stoat::KeyContext};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_open_command_palette(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(stoat) = self.active_stoat(cx) else {
            return;
        };

        let (current_mode, current_key_context) = {
            let s = stoat.read(cx);
            (s.mode().to_string(), s.key_context())
        };

        self.app_state
            .open_command_palette(current_mode, current_key_context, cx);

        stoat.update(cx, |s, _cx| {
            s.set_key_context(KeyContext::CommandPalette);
            s.set_mode("command_palette");
            s.command_palette_input_ref = self.app_state.command_palette.input.clone();
        });

        cx.notify();
    }
}
