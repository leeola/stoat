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
            let result = {
                let git_dir = root_path.join(".git");
                if services.fs.exists(&git_dir.join("rebase-merge")).await
                    || services.fs.exists(&git_dir.join("rebase-apply")).await
                {
                    Err(GitError::GitOperationFailed(
                        "A rebase is already in progress".into(),
                    ))
                } else {
                    let repo = services.git.open(&root_path).await;
                    match repo {
                        Ok(repo) => repo.rebase_interactive(&base_ref, &todo).await,
                        Err(e) => Err(e),
                    }
                }
            };

            let git_dir = root_path.join(".git");

            let repo_for_detect = services.git.open(&root_path).await.ok();
            let in_progress = match repo_for_detect {
                Some(ref repo) => detect_rebase_state(&git_dir, &*services.fs, &**repo).await,
                None => None,
            };
            let (phase, conflict_files) = if let Some(ref ip) = in_progress {
                let phase = phase_from_in_progress(ip, &git_dir, &*services.fs).await;
                let conflicts = if matches!(
                    phase,
                    crate::git::rebase::RebasePhase::PausedConflict { .. }
                ) {
                    match repo_for_detect {
                        Some(ref repo) => repo.conflict_files().await,
                        None => vec![],
                    }
                } else {
                    vec![]
                };
                (Some(phase), conflicts)
            } else {
                (None, vec![])
            };

            let (branch_info, status_files) =
                if let Ok(repo) = services.git.discover(&root_path).await {
                    let bi = repo.gather_branch_info().await;
                    let sf = repo.gather_status().await.unwrap_or_default();
                    (bi, sf)
                } else {
                    (None, Vec::new())
                };

            let _ = this.update(cx, |pane_group, cx| {
                if let (Some(phase), Some(ip)) = (phase, in_progress) {
                    if !conflict_files.is_empty() {
                        pane_group.app_state.rebase.conflict_files = conflict_files;
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

                pane_group
                    .app_state
                    .set_git_status(branch_info, status_files);
                cx.notify();
            });
        }));
    }
}
