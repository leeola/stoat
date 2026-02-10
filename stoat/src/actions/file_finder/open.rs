use crate::{pane_group::view::PaneGroupView, stoat::KeyContext};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_open_file_finder(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let (current_mode, current_key_context) = {
                let stoat = editor.read(cx).stoat.read(cx);
                (stoat.mode().to_string(), stoat.key_context())
            };

            self.app_state
                .open_file_finder(current_mode, current_key_context, cx);

            let input_buffer = self.app_state.file_finder.input.clone();
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, _cx| {
                    stoat.file_finder_input_ref = input_buffer;
                    stoat.set_key_context(KeyContext::FileFinder);
                    stoat.set_mode("file_finder");
                });
            });

            self.load_file_finder_preview(cx);

            cx.notify();
        }
    }
}
