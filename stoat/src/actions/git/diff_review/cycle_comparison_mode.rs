//! Diff review cycle comparison mode action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Cycle through diff comparison modes in diff review.
    ///
    /// Rotates through [`crate::git_diff::DiffComparisonMode`] variants (WorkingVsHead to
    /// WorkingVsIndex to IndexVsHead to WorkingVsHead) via
    /// [`Stoat::cycle_diff_comparison_mode`]. Recomputes the diff for the current file using
    /// [`Stoat::compute_diff_for_review_mode`] to reflect the new comparison. If the current
    /// hunk index is now out of range, resets to hunk 0. Jumps to the current (or first)
    /// hunk after recomputing.
    ///
    /// # Workflow
    ///
    /// 1. Cycles to next mode via [`Stoat::cycle_diff_comparison_mode`]
    /// 2. Gets current file path
    /// 3. Discovers repository and builds absolute path
    /// 4. Recomputes diff for new comparison mode
    /// 5. Updates buffer item's diff
    /// 6. Resets hunk index if out of range
    /// 7. Jumps to current hunk via [`Stoat::jump_to_current_hunk`]
    ///
    /// # Behavior
    ///
    /// - Only operates in diff_review mode
    /// - Returns early if no current file
    /// - Returns early if repository discovery fails
    /// - Resets hunk index to 0 if current index exceeds new hunk count
    /// - Jumps to hunk after mode change to show new diff
    ///
    /// # Related
    ///
    /// - [`Stoat::cycle_diff_comparison_mode`] - cycles the mode setting
    /// - [`Stoat::compute_diff_for_review_mode`] - centralized diff computation
    /// - [`Stoat::jump_to_current_hunk`] - cursor positioning and scrolling
    /// - [`crate::git_diff::DiffComparisonMode`] - comparison mode enum
    pub fn diff_review_cycle_comparison_mode(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        debug!("Cycling diff comparison mode");

        // Cycle to next mode
        self.cycle_diff_comparison_mode();
        let new_mode = self.diff_comparison_mode();
        debug!("New comparison mode: {:?}", new_mode);

        // Get current file path
        let current_file_path = match self
            .diff_review_files
            .get(self.diff_review_current_file_idx)
        {
            Some(path) => path.clone(),
            None => return,
        };

        // Get absolute path
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };
        let abs_path = repo.workdir().join(&current_file_path);

        // Recompute diff for new comparison mode
        if let Some(new_diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
            // Update the buffer item diff
            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, _cx| {
                item.set_diff(Some(new_diff.clone()));
            });

            // Reset hunk index if it's now out of range
            let hunk_count = new_diff.hunks.len();
            if self.diff_review_current_hunk_idx >= hunk_count {
                self.diff_review_current_hunk_idx = if hunk_count > 0 { 0 } else { 0 };
            }

            // Jump to current hunk (or first hunk if current is out of range)
            self.jump_to_current_hunk(cx);
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn cycles_comparison_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" && !s.diff_review_files.is_empty() {
                let initial_mode = s.diff_comparison_mode();
                s.diff_review_cycle_comparison_mode(cx);
                // Mode should change
                assert_ne!(s.diff_comparison_mode(), initial_mode);
            }
        });
    }
}
