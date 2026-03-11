use crate::{
    app_state::GitLogCommitDetail, git::status::DiffPreviewData, pane_group::view::PaneGroupView,
};
use gpui::Context;

impl PaneGroupView {
    pub(crate) fn handle_git_log_select(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.app_state.git_log.detail_visible {
            self.app_state.git_log.detail_visible = false;
            self.app_state.git_log.detail = None;
            self.app_state.git_log.detail_task = None;
            cx.notify();
        } else {
            self.app_state.git_log.detail_visible = true;
            self.load_git_log_detail_for_selected(cx);
        }
    }

    pub(crate) fn load_git_log_detail_for_selected(&mut self, cx: &mut Context<'_, Self>) {
        let selected = self.app_state.git_log.selected;
        let commit = match self.app_state.git_log.commits.get(selected) {
            Some(c) => c.clone(),
            None => return,
        };

        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let services = self.app_state.services.clone();
        let oid = commit.oid.clone();

        self.app_state.git_log.detail_task = None;

        let task = cx.spawn({
            async move |this, cx| {
                let repo = match services.git.open(&root_path).await {
                    Ok(r) => r,
                    Err(_) => return,
                };

                let files = repo.commit_files_by_oid(&oid).await.unwrap_or_default();

                let preview = if let Some(first_file) = files.first() {
                    repo.commit_file_diff(&oid, &first_file.path)
                        .await
                        .ok()
                        .map(DiffPreviewData::new)
                } else {
                    None
                };

                let _ = this.update(cx, |pgv, cx| {
                    pgv.app_state.git_log.detail = Some(GitLogCommitDetail {
                        files,
                        selected_file: 0,
                        preview,
                        preview_task: None,
                    });
                    cx.notify();
                });
            }
        });

        self.app_state.git_log.detail_task = Some(task);
        cx.notify();
    }

    pub(crate) fn handle_git_log_detail_next_file(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut Context<'_, Self>,
    ) {
        let detail = match self.app_state.git_log.detail.as_mut() {
            Some(d) => d,
            None => return,
        };

        if detail.selected_file < detail.files.len().saturating_sub(1) {
            detail.selected_file += 1;
            self.load_git_log_file_preview(cx);
        }
    }

    pub(crate) fn handle_git_log_detail_prev_file(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut Context<'_, Self>,
    ) {
        let detail = match self.app_state.git_log.detail.as_mut() {
            Some(d) => d,
            None => return,
        };

        if detail.selected_file > 0 {
            detail.selected_file -= 1;
            self.load_git_log_file_preview(cx);
        }
    }

    fn load_git_log_file_preview(&mut self, cx: &mut Context<'_, Self>) {
        let selected = self.app_state.git_log.selected;
        let commit = match self.app_state.git_log.commits.get(selected) {
            Some(c) => c.clone(),
            None => return,
        };

        let detail = match self.app_state.git_log.detail.as_ref() {
            Some(d) => d,
            None => return,
        };

        let file = match detail.files.get(detail.selected_file) {
            Some(f) => f.path.clone(),
            None => return,
        };

        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let services = self.app_state.services.clone();
        let oid = commit.oid.clone();

        let task = cx.spawn({
            async move |this, cx| {
                let repo = match services.git.open(&root_path).await {
                    Ok(r) => r,
                    Err(_) => return,
                };

                let preview = repo
                    .commit_file_diff(&oid, &file)
                    .await
                    .ok()
                    .map(DiffPreviewData::new);

                let _ = this.update(cx, |pgv, cx| {
                    if let Some(ref mut detail) = pgv.app_state.git_log.detail {
                        detail.preview = preview;
                    }
                    cx.notify();
                });
            }
        });

        if let Some(ref mut detail) = self.app_state.git_log.detail {
            detail.preview_task = Some(task);
        }

        cx.notify();
    }
}
