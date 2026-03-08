use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_rebase_abort(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let services = self.app_state.services.clone();

        self.app_state.rebase.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                let repo = services.git.open(&root_path)?;
                repo.rebase_abort()
            })
            .await;

            let _ = this.update(cx, |pane_group, cx| {
                let msg = match &result {
                    Ok(()) => "Rebase aborted".to_string(),
                    Err(e) => format!("Rebase abort failed: {e}"),
                };
                pane_group.app_state.flash_message = Some(msg);
                let (_prev_mode, prev_ctx) = pane_group.app_state.dismiss_rebase();
                if let Some(prev) = prev_ctx {
                    if let Some(editor) = pane_group.active_editor().cloned() {
                        editor.update(cx, |editor, cx| {
                            editor.stoat.update(cx, |stoat, cx| {
                                stoat.handle_set_key_context(prev, cx);
                            });
                        });
                    }
                }
                pane_group.app_state.refresh_git_status();
                cx.notify();
            });
        }));
    }
}
