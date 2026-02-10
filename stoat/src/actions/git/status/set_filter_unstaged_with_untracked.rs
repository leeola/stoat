use crate::{git::status::GitStatusFilter, pane_group::view::PaneGroupView};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_git_status_set_filter_unstaged_with_untracked(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            self.app_state.git_status.filter = GitStatusFilter::UnstagedWithUntracked;

            self.app_state.git_status.filtered = self
                .app_state
                .git_status
                .files
                .iter()
                .filter(|entry| self.app_state.git_status.filter.matches(entry))
                .cloned()
                .collect();

            self.app_state.git_status.selected = 0;

            self.load_git_status_preview(cx);

            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, _cx| {
                    stoat.set_mode("git_status");
                });
            });

            cx.notify();
        }
    }
}
