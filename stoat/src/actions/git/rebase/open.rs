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

        if let Some(in_progress) = detect_rebase_state(&git_dir, &*services.fs, &*repo) {
            self.app_state
                .open_rebase(current_mode, current_key_context);
            let phase = phase_from_in_progress(&in_progress, &git_dir, &*services.fs);
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

        let base_ref = match repo.upstream_ref() {
            Ok(Some(r)) => r,
            _ => {
                if repo.merge_base("origin/main", "HEAD").is_ok() {
                    "origin/main".to_string()
                } else if repo.merge_base("origin/master", "HEAD").is_ok() {
                    "origin/master".to_string()
                } else {
                    return;
                }
            },
        };

        let merge_base = match repo.merge_base(&base_ref, "HEAD") {
            Ok(mb) => mb,
            Err(_) => return,
        };

        let log_entries = match repo.log_commits(&merge_base, "HEAD", 100) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        if log_entries.is_empty() {
            return;
        }

        let commits: Vec<RebaseCommit> = log_entries
            .into_iter()
            .map(RebaseCommit::from_log_entry)
            .collect();

        self.app_state
            .open_rebase(current_mode, current_key_context);
        self.app_state.rebase.commits = commits;
        self.app_state.rebase.base_ref = base_ref;
        self.app_state.rebase.phase = RebasePhase::Planning;

        editor.update(cx, |editor, cx| {
            editor.stoat.update(cx, |stoat, _cx| {
                stoat.set_key_context(KeyContext::Rebase);
                stoat.set_mode("rebase_plan");
            });
        });

        self.load_rebase_preview(cx);
        cx.notify();
    }
}
