use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_buffer_finder_select(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            if self.app_state.buffer_finder.selected < self.app_state.buffer_finder.filtered.len() {
                let buffer_entry =
                    &self.app_state.buffer_finder.filtered[self.app_state.buffer_finder.selected];
                let buffer_id = buffer_entry.buffer_id;

                editor.update(cx, |editor, cx| {
                    editor.stoat.update(cx, |stoat, cx| {
                        if let Err(e) = stoat.switch_to_buffer(buffer_id, cx) {
                            tracing::error!("Failed to switch to buffer {:?}: {}", buffer_id, e);
                        }
                    });
                });
            }
            self.handle_buffer_finder_dismiss(_window, cx);
        }
    }
}
