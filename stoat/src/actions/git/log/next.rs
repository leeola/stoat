use crate::pane_group::view::PaneGroupView;
use gpui::{Context, ScrollStrategy, Window};

const LOG_LOAD_THRESHOLD: usize = 50;

impl PaneGroupView {
    pub(crate) fn handle_git_log_next(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let len = self.app_state.git_log.commits.len();
        if len == 0 {
            return;
        }

        if self.app_state.git_log.selected < len - 1 {
            self.app_state.git_log.selected += 1;
        }

        self.git_log_scroll
            .scroll_to_item(self.app_state.git_log.selected, ScrollStrategy::Nearest);

        if self.app_state.git_log.selected >= len.saturating_sub(LOG_LOAD_THRESHOLD) {
            self.load_more_git_log_commits(cx);
        }

        if self.app_state.git_log.detail_visible {
            self.load_git_log_detail_for_selected(cx);
        }
    }
}
