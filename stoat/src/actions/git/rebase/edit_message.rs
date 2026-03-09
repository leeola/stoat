use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_rebase_edit_message(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let message_path = root_path.join(".git/rebase-merge/message");
        let services = self.app_state.services.clone();

        cx.spawn(async move |this, cx| {
            if !services.fs.exists(&message_path).await {
                return;
            }

            let _ = this.update(cx, |pane_group, cx| {
                if let Some(editor) = pane_group.active_editor().cloned() {
                    editor.update(cx, |editor, cx| {
                        editor.stoat.update(cx, |stoat, cx| {
                            let _ = stoat.load_file(&message_path, cx);
                        });
                    });

                    let (_prev_mode, _prev_ctx) = pane_group.app_state.dismiss_rebase();

                    editor.update(cx, |editor, cx| {
                        editor.stoat.update(cx, |stoat, cx| {
                            stoat.handle_set_key_context(crate::stoat::KeyContext::TextEditor, cx);
                        });
                    });

                    pane_group.app_state.flash_message =
                        Some("Edit message, save, then space-g-r to continue".to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
