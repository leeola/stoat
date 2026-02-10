use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_command_palette_prev(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.app_state.command_palette.selected > 0 {
            self.app_state.command_palette.selected -= 1;
            debug!(
                selected = self.app_state.command_palette.selected,
                "Command palette: prev"
            );
            cx.notify();
        }
    }
}
