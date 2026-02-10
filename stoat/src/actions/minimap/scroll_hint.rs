use crate::pane_group::view::{
    MinimapFadeState, MinimapVisibility, PaneGroupView, SCROLL_HINT_DEFAULT_THRESHOLD,
};
use gpui::{Context, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_show_minimap_on_scroll(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.minimap_visibility = MinimapVisibility::ScrollHint {
            threshold_lines: SCROLL_HINT_DEFAULT_THRESHOLD,
        };

        self.last_editor_scroll_y = None;
        self.minimap_fade_state = MinimapFadeState::Hidden;

        debug!("Enabled minimap scroll hint mode");
        cx.notify();
    }
}
