use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_select_prev_command_v2(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(palette) = &self.app_state.command_palette_v2 {
            palette.update(cx, |p, _| {
                p.select_prev();
            });
            cx.notify();
        }
    }
}
