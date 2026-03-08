use crate::{
    git::rebase::{detect_rebase_state, phase_from_in_progress},
    pane_group::view::PaneGroupView,
};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_rebase_skip(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let services = self.app_state.services.clone();

        self.app_state.rebase.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = smol::unblock({
                let services = services.clone();
                let root_path = root_path.clone();
                move || {
                    let repo = services.git.open(&root_path)?;
                    repo.rebase_skip()
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
                } else {
                    let msg = match &result {
                        Ok(()) => "Rebase completed successfully".to_string(),
                        Err(e) => format!("Rebase skip failed: {e}"),
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
