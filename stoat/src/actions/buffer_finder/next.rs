use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_buffer_finder_next(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.app_state.buffer_finder.selected + 1 < self.app_state.buffer_finder.filtered.len() {
            self.app_state.buffer_finder.selected += 1;
            cx.notify();
        }
    }
}
