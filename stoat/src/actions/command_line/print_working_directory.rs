use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};
use std::time::Duration;

impl PaneGroupView {
    pub(crate) fn handle_print_working_directory(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = self.app_state.worktree.lock().root().to_path_buf();
        let display_path = root.canonicalize().unwrap_or(root).display().to_string();
        self.app_state.flash_message = Some(display_path);

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;

            this.update(cx, |this, cx| {
                this.app_state.flash_message = None;
                cx.notify();
            })
            .ok();
        })
        .detach();

        cx.notify();
    }
}
