use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_git_status_prev(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.app_state.git_status.selected > 0 {
            self.app_state.git_status.selected -= 1;
            self.load_git_status_preview(cx);
            cx.notify();
        }
    }
}
