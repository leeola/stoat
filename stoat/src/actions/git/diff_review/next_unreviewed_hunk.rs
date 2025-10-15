//! Diff review next unreviewed hunk action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Jump to next unreviewed hunk across all files.
    ///
    /// Searches files on-demand for the next unreviewed hunk (not in
    /// [`Stoat::diff_review_approved_hunks`]). Loads each file, computes diff via
    /// [`Stoat::compute_diff_for_review_mode`], and checks for unreviewed hunks. Wraps around
    /// to the beginning if needed. Exits review mode via [`Stoat::diff_review_dismiss`] if all
    /// hunks reviewed.
    ///
    /// # Workflow
    ///
    /// 1. Search current file from next hunk onward
    /// 2. Search remaining files (load each on-demand)
    /// 3. Search current file from beginning up to start hunk (wrap-around)
    /// 4. If no unreviewed hunks found: dismiss review mode
    ///
    /// # Behavior
    ///
    /// - Only operates in diff_review mode
    /// - Returns early if no files in review list
    /// - Loads files on-demand to check for unreviewed hunks
    /// - Wraps around to beginning of file list
    /// - Exits review mode when all hunks reviewed
    ///
    /// # Related
    ///
    /// - [`Stoat::diff_review_approve_hunk`] - mark hunk as reviewed
    /// - [`Stoat::diff_review_toggle_approval`] - toggle without advancing
    /// - [`Stoat::diff_review_next_hunk`] - advance to any next hunk
    /// - [`Stoat::compute_diff_for_review_mode`] - on-demand diff computation
    pub fn diff_review_next_unreviewed_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        if self.diff_review_files.is_empty() {
            return;
        }

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };

        let start_file = self.diff_review_current_file_idx;
        let start_hunk = self.diff_review_current_hunk_idx + 1; // Start from next hunk
        let file_count = self.diff_review_files.len();

        let empty_set = std::collections::HashSet::new();

        // Helper to check if a file has an unreviewed hunk at/after start_hunk_idx
        let find_unreviewed_in_file =
            |file_path: &std::path::PathBuf, start_hunk_idx: usize| -> Option<usize> {
                let approved_hunks = self
                    .diff_review_approved_hunks
                    .get(file_path)
                    .unwrap_or(&empty_set);

                // Get hunk count from current buffer diff if this is the current file
                let hunk_count = {
                    let buffer_item = self.active_buffer(cx);
                    let item = buffer_item.read(cx);
                    item.diff().map(|d| d.hunks.len()).unwrap_or(0)
                };

                (start_hunk_idx..hunk_count).find(|idx| !approved_hunks.contains(idx))
            };

        // Search in current file first (from start_hunk onward)
        if let Some(current_file_path) = self.diff_review_files.get(start_file) {
            if let Some(hunk_idx) = find_unreviewed_in_file(current_file_path, start_hunk) {
                self.diff_review_current_hunk_idx = hunk_idx;
                self.jump_to_current_hunk(cx);
                cx.notify();
                return;
            }
        }

        // Search remaining files (load each on-demand)
        for offset in 1..file_count {
            let file_idx = (start_file + offset) % file_count;
            if file_idx == start_file {
                break; // Back to start - handle this case separately
            }

            // Clone file path to avoid borrow conflicts
            let file_path = self.diff_review_files[file_idx].clone();
            let abs_path = repo.workdir().join(&file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // Compute diff
            if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                let buffer_item = self.active_buffer(cx);
                buffer_item.update(cx, |item, _| {
                    item.set_diff(Some(diff.clone()));
                });

                // Check for unreviewed hunks in this file
                let approved_hunks = self
                    .diff_review_approved_hunks
                    .get(&file_path)
                    .unwrap_or(&empty_set);

                if let Some(hunk_idx) =
                    (0..diff.hunks.len()).find(|idx| !approved_hunks.contains(idx))
                {
                    self.diff_review_current_file_idx = file_idx;
                    self.diff_review_current_hunk_idx = hunk_idx;
                    self.jump_to_current_hunk(cx);
                    cx.notify();
                    return;
                }
            }
        }

        // Search current file from beginning up to start_hunk
        if let Some(current_file_path) = self.diff_review_files.get(start_file) {
            let approved_hunks = self
                .diff_review_approved_hunks
                .get(current_file_path)
                .unwrap_or(&empty_set);

            if let Some(hunk_idx) = (0..start_hunk).find(|idx| !approved_hunks.contains(idx)) {
                self.diff_review_current_hunk_idx = hunk_idx;
                self.jump_to_current_hunk(cx);
                cx.notify();
                return;
            }
        }

        // No unreviewed hunks found - all review complete
        debug!("All hunks reviewed");
        self.diff_review_dismiss(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn finds_next_unreviewed(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" {
                // Just verify it doesn't panic
                s.diff_review_next_unreviewed_hunk(cx);
            }
        });
    }
}
