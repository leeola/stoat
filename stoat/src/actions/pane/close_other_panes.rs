use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Focusable, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_close_other_panes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let keep = self.active_pane;
        let to_remove: Vec<_> = self
            .pane_group
            .panes()
            .into_iter()
            .filter(|&id| id != keep)
            .collect();

        if to_remove.is_empty() {
            return;
        }

        debug!(
            keep_pane = keep,
            closing = to_remove.len(),
            "Closing other panes"
        );

        for pane_id in to_remove {
            if self.pane_group.remove(pane_id).is_ok() {
                self.pane_contents.remove(&pane_id);
            }
        }

        // Re-focus the kept pane
        if let Some(editor) = self
            .pane_contents
            .get(&keep)
            .and_then(|content| content.as_editor())
        {
            window.focus(&editor.read(cx).focus_handle(cx));
        }

        self.update_minimap_to_active_pane(cx);
        self.exit_pane_mode(cx);
        cx.notify();
    }
}
