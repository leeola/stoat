use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Focusable, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_quit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pane_to_close = self.active_pane;

        debug!(pane_id = pane_to_close, "Attempting to quit/close pane");

        match self.pane_group.remove(pane_to_close) {
            Ok(()) => {
                self.pane_contents.remove(&pane_to_close);

                let remaining_panes = self.pane_group.panes();
                if let Some(&new_active_pane) = remaining_panes.first() {
                    debug!(
                        closed_pane = pane_to_close,
                        new_active_pane,
                        remaining_count = remaining_panes.len(),
                        "Pane closed, switching focus"
                    );

                    self.active_pane = new_active_pane;
                    if let Some(editor) = self
                        .pane_contents
                        .get(&new_active_pane)
                        .and_then(|content| content.as_editor())
                    {
                        window.focus(&editor.read(cx).focus_handle(cx));
                    }

                    self.update_minimap_to_active_pane(cx);

                    self.exit_pane_mode(cx);

                    cx.notify();
                }
            },
            Err(e) => {
                debug!(
                    pane_id = pane_to_close,
                    error = %e,
                    "Last pane - quitting application"
                );
                cx.quit();
            },
        }
    }
}
