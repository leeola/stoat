use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_file_finder_dismiss(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            self.app_state.file_finder.input = None;
            self.app_state.file_finder.files.clear();
            self.app_state.file_finder.filtered.clear();
            self.app_state.file_finder.selected = 0;
            self.app_state.file_finder.preview = None;
            self.app_state.file_finder.preview_task = None;

            if let Some(previous_context) = self.app_state.file_finder.previous_key_context.take() {
                editor.update(cx, |editor, cx| {
                    editor.stoat.update(cx, |stoat, cx| {
                        stoat.handle_set_key_context(previous_context, cx);
                    });
                });
            }

            cx.notify();
        }
    }
}
