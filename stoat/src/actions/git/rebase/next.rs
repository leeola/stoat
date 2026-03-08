use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_rebase_next(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.app_state.rebase.selected + 1 < self.app_state.rebase.commits.len() {
            self.app_state.rebase.selected += 1;
            self.load_rebase_preview(cx);
            cx.notify();
        }
    }
}
