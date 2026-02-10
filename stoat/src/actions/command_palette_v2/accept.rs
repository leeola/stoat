use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_accept_command_palette_v2(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let command_name = self
            .app_state
            .command_palette_v2
            .as_ref()
            .and_then(|p| p.read(cx).selected_command())
            .map(|cmd| cmd.name.clone());

        self.handle_dismiss_command_palette_v2(window, cx);

        if let Some(name) = command_name {
            self.dispatch_action_by_name(&name, window, cx);
        }

        cx.notify();
    }
}
