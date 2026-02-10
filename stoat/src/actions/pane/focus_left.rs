use crate::{pane::SplitDirection, pane_group::view::PaneGroupView};
use gpui::{Context, Focusable, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_focus_pane_left(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(new_pane) = self.get_pane_in_direction(SplitDirection::Left) {
            debug!(
                from_pane = self.active_pane,
                to_pane = new_pane,
                direction = "Left",
                "Focusing pane"
            );
            self.active_pane = new_pane;
            if let Some(editor) = self
                .pane_contents
                .get(&new_pane)
                .and_then(|content| content.as_editor())
            {
                window.focus(&editor.read(cx).focus_handle(cx));
            }

            self.update_minimap_to_active_pane(cx);

            self.exit_pane_mode(cx);

            cx.notify();
        } else {
            debug!(
                current_pane = self.active_pane,
                direction = "Left",
                "No pane in direction"
            );
        }
    }
}
