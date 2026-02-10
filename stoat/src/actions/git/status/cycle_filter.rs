use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_git_status_cycle_filter(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.app_state.git_status.filter = self.app_state.git_status.filter.next();

        self.app_state.git_status.filtered = self
            .app_state
            .git_status
            .files
            .iter()
            .filter(|entry| self.app_state.git_status.filter.matches(entry))
            .cloned()
            .collect();

        self.app_state.git_status.selected = 0;

        self.load_git_status_preview(cx);

        cx.notify();
    }
}
