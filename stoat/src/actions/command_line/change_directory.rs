use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};
use stoat_text::Language;

impl PaneGroupView {
    pub(crate) fn handle_change_directory(
        &mut self,
        path: &std::path::Path,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match self.app_state.change_directory(path.to_path_buf(), cx) {
            Ok(()) => {
                self.app_state
                    .ensure_lsp_for_language(Language::Rust, cx.weak_entity(), cx);
                self.handle_command_line_dismiss(_window, cx);
            },
            Err(e) => {
                tracing::error!("Failed to change directory: {}", e);
            },
        }

        cx.notify();
    }
}
