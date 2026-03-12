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

        self.load_git_log_detail_for_selected(cx);
    }

    pub(crate) fn handle_git_log_page_down(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let len = self.app_state.git_log.commits.len();
        if len == 0 {
            return;
        }

        let new_selected = (self.app_state.git_log.selected + 15).min(len - 1);
        self.app_state.git_log.selected = new_selected;

        self.git_log_scroll
            .scroll_to_item(new_selected, ScrollStrategy::Nearest);

        if new_selected >= len.saturating_sub(LOG_LOAD_THRESHOLD) {
            self.load_more_git_log_commits(cx);
        }

        self.load_git_log_detail_for_selected(cx);
    }

    pub(crate) fn handle_git_log_page_up(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let len = self.app_state.git_log.commits.len();
        if len == 0 {
            return;
        }

        let new_selected = self.app_state.git_log.selected.saturating_sub(15);
        self.app_state.git_log.selected = new_selected;

        self.git_log_scroll
            .scroll_to_item(new_selected, ScrollStrategy::Nearest);

        self.load_git_log_detail_for_selected(cx);
    }
}
