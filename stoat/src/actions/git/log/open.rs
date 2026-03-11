use crate::{git::log_graph::compute_graph, pane_group::view::PaneGroupView, stoat::KeyContext};
use gpui::{Context, Window};

const LOG_BATCH_SIZE: usize = 200;

impl PaneGroupView {
    pub(crate) fn handle_open_git_log(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let editor_opt = self.active_editor().cloned();
        let Some(editor) = editor_opt else { return };

        let (current_mode, current_key_context) = {
            let stoat = editor.read(cx).stoat.read(cx);
            (stoat.mode().to_string(), stoat.key_context())
        };

        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let services = self.app_state.services.clone();

        self.app_state
            .open_git_log(current_mode, current_key_context);

        editor.update(cx, |editor, cx| {
            editor.stoat.update(cx, |stoat, cx| {
                stoat.set_key_context(KeyContext::GitLog);
                stoat.set_mode_by_name("git_log", cx);
            });
        });

        cx.notify();

        cx.spawn({
            let editor = editor.clone();
            async move |this, cx| {
                let repo = match services.git.open(&root_path).await {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = this.update(cx, |pgv, cx| {
                            pgv.app_state.flash_message =
                                Some(format!("Failed to open git repo: {e}"));
                            let (_prev_mode, prev_ctx) = pgv.app_state.dismiss_git_log();
                            if let Some(prev) = prev_ctx {
                                editor.update(cx, |editor, cx| {
                                    editor.stoat.update(cx, |stoat, cx| {
                                        stoat.handle_set_key_context(prev, cx);
                                    });
                                });
                            }
                            cx.notify();
                        });
                        return;
                    },
                };

                let result = repo.log_all_branches(0, LOG_BATCH_SIZE).await;

                let _ = this.update(cx, |pgv, cx| {
                    match result {
                        Ok(commits) => {
                            let graph = compute_graph(&commits);
                            let all_loaded = commits.len() < LOG_BATCH_SIZE;
                            pgv.app_state.git_log.commits = commits;
                            pgv.app_state.git_log.graph = graph;
                            pgv.app_state.git_log.loading = false;
                            pgv.app_state.git_log.all_loaded = all_loaded;
                        },
                        Err(e) => {
                            pgv.app_state.flash_message =
                                Some(format!("Failed to load commits: {e}"));
                            let (_prev_mode, prev_ctx) = pgv.app_state.dismiss_git_log();
                            if let Some(prev) = prev_ctx {
                                editor.update(cx, |editor, cx| {
                                    editor.stoat.update(cx, |stoat, cx| {
                                        stoat.handle_set_key_context(prev, cx);
                                    });
                                });
                            }
                        },
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(crate) fn load_more_git_log_commits(&mut self, cx: &mut Context<'_, Self>) {
        if self.app_state.git_log.loading || self.app_state.git_log.all_loaded {
            return;
        }

        let current_len = self.app_state.git_log.commits.len();
        if current_len == 0 {
            return;
        }

        self.app_state.git_log.loading = true;
        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let services = self.app_state.services.clone();

        cx.spawn({
            async move |this, cx| {
                let repo = match services.git.open(&root_path).await {
                    Ok(r) => r,
                    Err(_) => return,
                };

                let result = repo.log_all_branches(current_len, LOG_BATCH_SIZE).await;

                let _ = this.update(cx, |pgv, cx| {
                    match result {
                        Ok(new_commits) => {
                            let all_loaded = new_commits.len() < LOG_BATCH_SIZE;
                            pgv.app_state.git_log.commits.extend(new_commits);
                            pgv.app_state.git_log.graph =
                                compute_graph(&pgv.app_state.git_log.commits);
                            pgv.app_state.git_log.loading = false;
                            pgv.app_state.git_log.all_loaded = all_loaded;
                        },
                        Err(_) => {
                            pgv.app_state.git_log.loading = false;
                            pgv.app_state.git_log.all_loaded = true;
                        },
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }
}
