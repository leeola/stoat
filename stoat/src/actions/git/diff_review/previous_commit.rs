use crate::{
    git::diff_review::{ReviewScope, ScopeState},
    stoat::Stoat,
};
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Toggle between WorkingTree and Commit scope.
    ///
    /// Entering Commit scope saves the current state and builds a new file list
    /// from `commit_changed_files()`. Exiting restores the saved state exactly.
    pub fn diff_review_previous_commit(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git::repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        if self.review_scope == ReviewScope::Commit {
            self.exit_commit_scope(&repo, cx);
        } else {
            self.enter_commit_scope(&repo, cx);
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    fn enter_commit_scope(
        &mut self,
        repo: &crate::git::repository::Repository,
        cx: &mut Context<Self>,
    ) {
        self.review_saved = Some((self.review_scope, self.review_state.clone()));
        self.review_scope = ReviewScope::Commit;
        self.review_state = ScopeState::default();

        let file_paths = match repo.commit_changed_files() {
            Ok(paths) => paths,
            Err(e) => {
                debug!("No commit changes found: {e}");
                return;
            },
        };

        if file_paths.is_empty() {
            debug!("No files changed in previous commit");
            return;
        }

        self.review_state.files = file_paths;

        // Find first file with hunks
        for (idx, file_path) in self.review_state.files.clone().iter().enumerate() {
            let abs_path = repo.workdir().join(file_path);

            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {e}", abs_path);
                continue;
            }

            // Replace buffer with HEAD content
            match repo.head_content(&abs_path) {
                Ok(content) => {
                    let buffer_item = self.active_buffer(cx);
                    self.replace_buffer_content(&content, &buffer_item, cx);
                },
                Err(e) => tracing::warn!("Failed to read head content for {abs_path:?}: {e}"),
            }

            if let Some((diff, staged_rows, staged_hunk_indices)) =
                self.compute_diff_for_review_mode(&abs_path, cx)
            {
                if !diff.hunks.is_empty() {
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff));
                        item.set_staged_rows(staged_rows);
                        item.set_staged_hunk_indices(staged_hunk_indices);
                    });

                    self.review_state.file_idx = idx;
                    self.review_state.hunk_idx = 0;
                    self.jump_to_current_hunk(false, cx);
                    return;
                }
            }
        }

        debug!("No files with hunks in Commit scope");
    }

    fn exit_commit_scope(
        &mut self,
        repo: &crate::git::repository::Repository,
        cx: &mut Context<Self>,
    ) {
        if let Some((saved_scope, saved_state)) = self.review_saved.take() {
            self.review_scope = saved_scope;
            self.review_state = saved_state;
        } else {
            self.review_scope = ReviewScope::WorkingTree;
            self.review_state = ScopeState::default();
        }

        // Reload the file at the restored position from working tree
        if let Some(file_path) = self
            .review_state
            .files
            .get(self.review_state.file_idx)
            .cloned()
        {
            let abs_path = repo.workdir().join(&file_path);
            let _ = self.load_file(&abs_path, cx);

            if let Some((diff, staged_rows, staged_hunk_indices)) =
                self.compute_diff_for_review_mode(&abs_path, cx)
            {
                let buffer_item = self.active_buffer(cx);
                buffer_item.update(cx, |item, _| {
                    item.set_diff(Some(diff));
                    item.set_staged_rows(staged_rows);
                    item.set_staged_hunk_indices(staged_hunk_indices);
                });
            }

            self.jump_to_current_hunk(false, cx);
        }
    }
}
