use crate::{git::rebase::RebaseOperation, pane_group::view::PaneGroupView};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_rebase_set_operation(
        &mut self,
        op: RebaseOperation,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let idx = self.app_state.rebase.selected;
        if let Some(commit) = self.app_state.rebase.commits.get_mut(idx) {
            commit.operation = op;
            cx.notify();
        }
    }
}
