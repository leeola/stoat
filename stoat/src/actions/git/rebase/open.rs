use crate::{
    git::rebase::{detect_rebase_state, phase_from_in_progress, RebaseCommit, RebasePhase},
    pane_group::view::PaneGroupView,
    stoat::KeyContext,
};
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_open_rebase(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let editor_opt = self.active_editor().cloned();
        let Some(editor) = editor_opt else { return };

        let (current_mode, current_key_context) = {
            let stoat = editor.read(cx).stoat.read(cx);
            (stoat.mode().to_string(), stoat.key_context())
        };

        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let git_dir = root_path.join(".git");
        let services = self.app_state.services.clone();

        let repo = match services.git.open(&root_path) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Detect in-progress rebase (rebase-merge or rebase-apply)
        if let Some(in_progress) = detect_rebase_state(&git_dir, &*services.fs, &*repo) {
            self.app_state
                .open_rebase(current_mode, current_key_context);
            let phase = phase_from_in_progress(&in_progress, &git_dir, &*services.fs);
            if matches!(phase, RebasePhase::PausedConflict { .. }) {
                self.app_state.rebase.conflict_files = repo.conflict_files();
            }
            self.app_state.rebase.in_progress = Some(in_progress);
            self.app_state.rebase.phase = phase;

            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, _cx| {
                    stoat.set_key_context(KeyContext::Rebase);
                    stoat.set_mode("rebase_progress");
                });
            });

            cx.notify();
            return;
        }

        // Determine base ref for planning mode
        let base_ref = match repo.upstream_ref() {
            Ok(Some(r)) => r,
            _ => {
                if repo.merge_base("origin/main", "HEAD").is_ok() {
                    "origin/main".to_string()
                } else if repo.merge_base("origin/master", "HEAD").is_ok() {
                    "origin/master".to_string()
                } else {
                    self.app_state.flash_message = Some(
                        "No upstream branch found (tried upstream, origin/main, origin/master)"
                            .to_string(),
                    );
                    cx.notify();
                    return;
                }
            },
        };

        // Enter planning mode immediately with empty commits, then load async
        self.app_state
            .open_rebase(current_mode, current_key_context);
        self.app_state.rebase.base_ref = base_ref.clone();
        self.app_state.rebase.phase = RebasePhase::Planning;

        editor.update(cx, |editor, cx| {
            editor.stoat.update(cx, |stoat, _cx| {
                stoat.set_key_context(KeyContext::Rebase);
                stoat.set_mode("rebase_plan");
            });
        });

        cx.notify();

        // Async: load commits via merge_base + log_commits
        self.app_state.rebase.preview_task = Some(cx.spawn({
            let base_ref = base_ref.clone();
            async move |this, cx| {
                let result = smol::unblock({
                    let services = services.clone();
                    let root_path = root_path.clone();
                    let base_ref = base_ref.clone();
                    move || {
                        let repo = services.git.open(&root_path)?;
                        let merge_base = repo.merge_base(&base_ref, "HEAD")?;
                        repo.log_commits(&merge_base, "HEAD", 100)
                    }
                })
                .await;

                let _ = this.update(cx, |pane_group, cx| {
                    match result {
                        Ok(entries) if !entries.is_empty() => {
                            let commits: Vec<RebaseCommit> = entries
                                .into_iter()
                                .map(RebaseCommit::from_log_entry)
                                .collect();
                            pane_group.app_state.rebase.commits = commits;
                            pane_group.load_rebase_preview(cx);
                        },
                        Ok(_) => {
                            pane_group.app_state.flash_message =
                                Some("No commits to rebase".to_string());
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
                        },
                        Err(e) => {
                            pane_group.app_state.flash_message =
                                Some(format!("Failed to load commits: {e}"));
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
                        },
                    }
                    cx.notify();
                });
            }
        }));
    }
}
