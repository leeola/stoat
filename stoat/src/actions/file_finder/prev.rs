use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_file_finder_prev(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.app_state.file_finder.selected > 0 {
            self.app_state.file_finder.selected -= 1;
            self.load_file_finder_preview(cx);
            cx.notify();
        }
    }
}
