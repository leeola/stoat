use crate::pane_group::view::PaneGroupView;
use gpui::{Context, ScrollStrategy, Window};

impl PaneGroupView {
    pub(crate) fn handle_git_log_prev(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.app_state.git_log.selected > 0 {
            self.app_state.git_log.selected -= 1;
        }

        self.git_log_scroll
            .scroll_to_item(self.app_state.git_log.selected, ScrollStrategy::Nearest);

        self.load_git_log_detail_for_selected(cx);
    }
}
