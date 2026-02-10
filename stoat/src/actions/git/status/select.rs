use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_git_status_select(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            if self.app_state.git_status.selected < self.app_state.git_status.filtered.len() {
                let entry = &self.app_state.git_status.filtered[self.app_state.git_status.selected];
                let root = self.app_state.worktree.lock().root().to_path_buf();
                let abs_path = root.join(&entry.path);

                editor.update(cx, |editor, cx| {
                    editor.stoat.update(cx, |stoat, cx| {
                        if let Err(e) = stoat.load_file(&abs_path, cx) {
                            tracing::error!("Failed to load file {:?}: {}", abs_path, e);
                        }
                    });
                });
            }
            self.handle_git_status_dismiss(_window, cx);
        }
    }
}
