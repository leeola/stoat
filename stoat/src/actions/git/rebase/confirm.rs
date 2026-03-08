use crate::{
    git::{
        rebase::{detect_rebase_state, format_todo, phase_from_in_progress, validate_todo},
        repository::GitError,
    },
    pane_group::view::PaneGroupView,
};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_rebase_confirm(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.app_state.rebase.commits.is_empty() {
            return;
        }

        if let Err(msg) = validate_todo(&self.app_state.rebase.commits) {
            self.app_state.flash_message = Some(msg);
            cx.notify();
            return;
        }

        let todo = format_todo(&self.app_state.rebase.commits);
        let base_ref = self.app_state.rebase.base_ref.clone();
        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let services = self.app_state.services.clone();

        self.app_state.rebase.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = smol::unblock({
                let services = services.clone();
                let root_path = root_path.clone();
                move || {
                    let git_dir = root_path.join(".git");
                    if services.fs.exists(&git_dir.join("rebase-merge"))
                        || services.fs.exists(&git_dir.join("rebase-apply"))
                    {
                        return Err(GitError::GitOperationFailed(
                            "A rebase is already in progress".into(),
                        ));
                    }
                    let repo = services.git.open(&root_path)?;
                    repo.rebase_interactive(&base_ref, &todo)
                }
            })
            .await;

            let _ = this.update(cx, |pane_group, cx| {
                let git_dir = root_path.join(".git");
                let fs = &*services.fs;

                let repo_for_detect = services.git.open(&root_path).ok();
                let in_progress = repo_for_detect
                    .as_deref()
                    .and_then(|r| detect_rebase_state(&git_dir, fs, r));

                if let Some(ip) = in_progress {
                    let phase = phase_from_in_progress(&ip, &git_dir, fs);
                    if matches!(
                        phase,
                        crate::git::rebase::RebasePhase::PausedConflict { .. }
                    ) {
                        if let Some(repo) = repo_for_detect.as_deref() {
                            pane_group.app_state.rebase.conflict_files = repo.conflict_files();
                        }
                    } else {
                        pane_group.app_state.rebase.conflict_files.clear();
                    }
                    pane_group.app_state.rebase.in_progress = Some(ip);
                    pane_group.app_state.rebase.phase = phase;

                    if let Some(editor) = pane_group.active_editor().cloned() {
                        editor.update(cx, |editor, cx| {
                            editor.stoat.update(cx, |stoat, _cx| {
                                stoat.set_mode("rebase_progress");
                            });
                        });
                    }
                } else {
                    let msg = match &result {
                        Ok(()) => "Rebase completed successfully".to_string(),
                        Err(e) => format!("Rebase failed: {e}"),
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
                }

                pane_group.app_state.refresh_git_status();
                cx.notify();
            });
        }));
    }
}
