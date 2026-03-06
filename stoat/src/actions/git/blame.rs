use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    pub fn open_git_blame(&mut self, cx: &mut Context<Self>) {
        if self.blame_state.active {
            self.blame_dismiss(cx);
            return;
        }

        let file_path = match &self.current_file_path {
            Some(p) => p.clone(),
            None => {
                tracing::debug!("No file open for blame");
                return;
            },
        };

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git::repository::Repository::discover(&root_path) {
            Ok(r) => r,
            Err(_) => {
                tracing::debug!("No git repository found");
                return;
            },
        };

        let data = match crate::git::blame::blame_file(&repo, &file_path) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Blame failed: {e}");
                return;
            },
        };

        self.blame_state.active = true;
        self.blame_state.data = Some(data);
        self.blame_state.popup_visible = false;
        self.blame_state.popup_line = None;

        self.key_context = crate::stoat::KeyContext::BlameReview;
        self.mode = "blame_review".to_string();

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    pub fn blame_dismiss(&mut self, cx: &mut Context<Self>) {
        self.blame_state.active = false;
        self.blame_state.data = None;
        self.blame_state.popup_visible = false;
        self.blame_state.popup_line = None;

        self.key_context = crate::stoat::KeyContext::TextEditor;
        self.mode = "normal".to_string();

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    pub fn blame_toggle_author(&mut self, cx: &mut Context<Self>) {
        self.blame_state.show_author = !self.blame_state.show_author;
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    pub fn blame_toggle_date(&mut self, cx: &mut Context<Self>) {
        self.blame_state.show_date = !self.blame_state.show_date;
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    pub fn blame_show_detail(&mut self, cx: &mut Context<Self>) {
        self.blame_state.popup_visible = true;
        self.blame_state.popup_line = Some(self.cursor_position().row);
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

use crate::pane_group::view::PaneGroupView;

impl PaneGroupView {
    pub(crate) fn handle_open_git_blame(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_git_blame(cx);
                });
            });
            cx.notify();
        }
    }
}
