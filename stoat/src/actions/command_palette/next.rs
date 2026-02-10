use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_command_palette_next(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.app_state.command_palette.selected + 1
            < self.app_state.command_palette.filtered.len()
        {
            self.app_state.command_palette.selected += 1;
            debug!(
                selected = self.app_state.command_palette.selected,
                "Command palette: next"
            );
            cx.notify();
        }
    }
}
