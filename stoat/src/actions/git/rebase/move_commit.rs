use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_rebase_move_up(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let idx = self.app_state.rebase.selected;
        if idx > 0 {
            self.app_state.rebase.commits.swap(idx, idx - 1);
            self.app_state.rebase.selected = idx - 1;
            cx.notify();
        }
    }

    pub(crate) fn handle_rebase_move_down(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let idx = self.app_state.rebase.selected;
        if idx + 1 < self.app_state.rebase.commits.len() {
            self.app_state.rebase.commits.swap(idx, idx + 1);
            self.app_state.rebase.selected = idx + 1;
            cx.notify();
        }
    }
}
