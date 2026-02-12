//! Diff review reset progress action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Reset all review progress and start from beginning.
    ///
    /// Clears all approved hunks from [`Stoat::diff_review_approved_hunks`] and resets to
    /// the first file with hunks at hunk index 0. If in review mode, loads the first file
    /// on-demand, computes its diff via [`Stoat::compute_diff_for_review_mode`], and jumps
    /// to the first hunk. Use this to start a fresh review pass.
    ///
    /// # Workflow
    ///
    /// 1. Clears [`Stoat::diff_review_approved_hunks`]
    /// 2. If in diff_review mode and files exist: a. Iterates through files to find first with
    ///    hunks b. Loads file and computes diff c. Resets file and hunk indices to 0 d. Jumps to
    ///    first hunk via [`Stoat::jump_to_current_hunk`]
    ///
    /// # Behavior
    ///
    /// - Clears all approval state
    /// - Works in any mode (not just diff_review)
    /// - If not in diff_review mode: only clears approval state
    /// - If in diff_review mode: also reloads first file and resets indices
    /// - Loads files on-demand to find first with hunks
    ///
    /// # Related
    ///
    /// - [`Stoat::open_diff_review`] - start review session
    /// - [`Stoat::diff_review_approve_hunk`] - mark hunk as reviewed
    /// - [`Stoat::diff_review_toggle_approval`] - toggle review status
    pub fn diff_review_reset_progress(&mut self, cx: &mut Context<Self>) {
        debug!("Resetting diff review progress");

        // Clear all approved hunks
        self.diff_review_approved_hunks.clear();

        // If in review mode, load first file and jump to first hunk
        if self.mode == "diff_review" && !self.diff_review_files.is_empty() {
            let root_path = self.worktree.lock().root().to_path_buf();
            if let Ok(repo) = crate::git::repository::Repository::discover(&root_path) {
                // Clone file list to avoid borrow conflicts
                let files = self.diff_review_files.clone();
                // Find first file with hunks by loading files on-demand
                for (idx, file_path) in files.iter().enumerate() {
                    let abs_path = repo.workdir().join(file_path);

                    // Load file
                    if let Err(e) = self.load_file(&abs_path, cx) {
                        tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                        continue;
                    }

                    // Compute diff
                    if let Some((diff, staged_rows, staged_hunk_indices)) =
                        self.compute_diff_for_review_mode(&abs_path, cx)
                    {
                        if !diff.hunks.is_empty() {
                            // Found first file with hunks
                            let buffer_item = self.active_buffer(cx);
                            buffer_item.update(cx, |item, _| {
                                item.set_diff(Some(diff.clone()));
                                item.set_staged_rows(staged_rows);
                                item.set_staged_hunk_indices(staged_hunk_indices);
                            });

                            // Reset to start
                            self.diff_review_current_file_idx = idx;
                            self.diff_review_current_hunk_idx = 0;

                            self.jump_to_current_hunk(true, cx);
                            cx.notify();
                            return;
                        }
                    }
                }
            }
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn resets_progress(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" && !s.diff_review_files.is_empty() {
                // Approve a hunk
                s.diff_review_approve_hunk(cx);
                // Reset progress
                s.diff_review_reset_progress(cx);
                // Approvals cleared
                assert!(s.diff_review_approved_hunks.is_empty());
            }
        });
    }
}
