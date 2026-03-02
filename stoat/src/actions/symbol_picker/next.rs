use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_symbol_picker_next(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.app_state.symbol_picker.selected + 1 < self.app_state.symbol_picker.filtered.len() {
            self.app_state.symbol_picker.selected += 1;
            cx.notify();
        }
    }
}
