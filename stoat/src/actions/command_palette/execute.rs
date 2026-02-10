use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_command_palette_execute(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let command_name = if self.app_state.command_palette.selected
            < self.app_state.command_palette.filtered.len()
        {
            Some(
                self.app_state.command_palette.filtered[self.app_state.command_palette.selected]
                    .name
                    .clone(),
            )
        } else {
            None
        };

        self.handle_command_palette_dismiss(window, cx);

        if let Some(name) = command_name {
            self.dispatch_action_by_name(&name, window, cx);
        }

        cx.notify();
    }
}
