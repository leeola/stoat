use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_open_help_overlay(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!(
            "handle_open_help_overlay called, help_overlay_visible={}",
            self.help_overlay_visible
        );
        if self.help_overlay_visible {
            debug!("Opening help modal");
            if let Some(editor) = self.active_editor() {
                editor.update(cx, |editor, cx| {
                    editor.stoat.update(cx, |stoat, cx| {
                        stoat.open_help_modal(cx);
                    });
                });
            }
            self.help_overlay_visible = false;
        } else {
            debug!("Showing help overlay");
            self.help_overlay_visible = true;
        }
        cx.notify();
    }
}
