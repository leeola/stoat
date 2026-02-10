use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_command_palette_toggle_hidden(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!(
            "Toggling command palette hidden commands: {} -> {}",
            self.app_state.command_palette.show_hidden, !self.app_state.command_palette.show_hidden
        );

        self.app_state.command_palette.show_hidden = !self.app_state.command_palette.show_hidden;

        let query = self
            .app_state
            .command_palette
            .input
            .as_ref()
            .map(|buffer| buffer.read(cx).text())
            .unwrap_or_default();

        self.filter_command_palette_commands(&query);

        self.app_state.command_palette.selected = 0;

        cx.notify();
    }
}
