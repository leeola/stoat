use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Toggle review of previous commit (HEAD vs HEAD~1).
    ///
    /// When entering HeadVsParent mode, saves the current comparison mode, replaces
    /// the file list with files changed in the last commit, and loads HEAD content.
    /// When already in HeadVsParent mode, restores the saved comparison mode and
    /// reloads the working tree file list from git status.
    pub fn diff_review_previous_commit(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git::repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        if self.diff_review_comparison_mode
            == crate::git::diff_review::DiffComparisonMode::HeadVsParent
        {
            self.exit_head_vs_parent(&repo, cx);
        } else {
            self.enter_head_vs_parent(&repo, cx);
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    fn enter_head_vs_parent(
        &mut self,
        repo: &crate::git::repository::Repository,
        cx: &mut Context<Self>,
    ) {
        self.diff_review_saved_comparison_mode = Some(self.diff_review_comparison_mode);
        self.diff_review_comparison_mode =
            crate::git::diff_review::DiffComparisonMode::HeadVsParent;

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

        self.diff_review_files = file_paths;
        self.diff_review_current_file_idx = 0;
        self.diff_review_current_hunk_idx = 0;
        self.diff_review_approved_hunks.clear();

        // Find first file with hunks
        for (idx, file_path) in self.diff_review_files.clone().iter().enumerate() {
            let abs_path = repo.workdir().join(file_path);

            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {e}", abs_path);
                continue;
            }

            // Replace buffer with HEAD content
            if let Ok(head_content) = repo.head_content(&abs_path) {
                let buffer_item = self.active_buffer(cx);
                buffer_item.update(cx, |item, cx| {
                    item.buffer().update(cx, |buffer, _| {
                        let len = buffer.len();
                        buffer.edit([(0..len, head_content.as_str())]);
                    });
                    let _ = item.reparse(cx);
                });
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

                    self.diff_review_current_file_idx = idx;
                    self.diff_review_current_hunk_idx = 0;
                    self.jump_to_current_hunk(false, cx);
                    return;
                }
            }
        }

        debug!("No files with hunks in HeadVsParent mode");
    }

    fn exit_head_vs_parent(
        &mut self,
        repo: &crate::git::repository::Repository,
        cx: &mut Context<Self>,
    ) {
        // Restore saved comparison mode
        if let Some(saved_mode) = self.diff_review_saved_comparison_mode.take() {
            self.diff_review_comparison_mode = saved_mode;
        } else {
            self.diff_review_comparison_mode =
                crate::git::diff_review::DiffComparisonMode::default();
        }

        // Reload file list from git status
        let entries = match crate::git::status::gather_git_status(repo.inner()) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!("Failed to gather git status: {e}");
                return;
            },
        };

        let mut seen = std::collections::HashSet::new();
        self.diff_review_files = entries
            .into_iter()
            .filter(|e| seen.insert(e.path.clone()))
            .map(|e| e.path)
            .collect();

        self.diff_review_current_file_idx = 0;
        self.diff_review_current_hunk_idx = 0;
        self.diff_review_approved_hunks.clear();

        if self.diff_review_files.is_empty() {
            return;
        }

        // Load first file with hunks
        for (idx, file_path) in self.diff_review_files.clone().iter().enumerate() {
            let abs_path = repo.workdir().join(file_path);

            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {e}", abs_path);
                continue;
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

                    self.diff_review_current_file_idx = idx;
                    self.diff_review_current_hunk_idx = 0;
                    self.jump_to_current_hunk(false, cx);
                    return;
                }
            }
        }
    }
}
