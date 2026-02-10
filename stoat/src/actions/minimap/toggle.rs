use crate::pane_group::view::{MinimapFadeState, MinimapVisibility, PaneGroupView};
use gpui::{Context, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_toggle_minimap(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.minimap_visibility = match self.minimap_visibility {
            MinimapVisibility::AlwaysVisible => MinimapVisibility::AlwaysHidden,
            MinimapVisibility::AlwaysHidden | MinimapVisibility::ScrollHint { .. } => {
                MinimapVisibility::AlwaysVisible
            },
        };

        self.last_editor_scroll_y = None;
        self.minimap_fade_state = MinimapFadeState::Hidden;

        debug!(
            minimap_visibility = ?self.minimap_visibility,
            "Toggled minimap visibility"
        );
        cx.notify();
    }
}
