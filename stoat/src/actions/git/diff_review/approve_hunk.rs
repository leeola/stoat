//! Diff review approve hunk action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Approve current hunk and jump to next hunk.
    ///
    /// Marks the current hunk as reviewed by adding it to [`Stoat::diff_review_approved_hunks`],
    /// then automatically navigates to the next hunk via [`Stoat::diff_review_next_hunk`].
    /// Combines marking with navigation for an efficient review workflow.
    ///
    /// # Workflow
    ///
    /// 1. Gets current file path from [`Stoat::diff_review_files`]
    /// 2. Inserts current hunk index into approved set for this file
    /// 3. Calls [`Stoat::diff_review_next_hunk`] to advance
    ///
    /// # Behavior
    ///
    /// - Only operates in diff_review mode
    /// - Returns early if current file index out of bounds
    /// - Approval persists across review sessions
    /// - Automatically advances after marking
    ///
    /// # Related
    ///
    /// - [`Stoat::diff_review_toggle_approval`] - toggle without advancing
    /// - [`Stoat::diff_review_next_unreviewed_hunk`] - skip to next unreviewed
    /// - [`Stoat::diff_review_reset_progress`] - clear all approvals
    pub fn diff_review_approve_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        // Get current file path
        let current_file_path = match self.review_state.files.get(self.review_state.file_idx) {
            Some(path) => path.clone(),
            None => return,
        };

        // Mark current hunk as approved
        self.review_state
            .approved_hunks
            .entry(current_file_path.clone())
            .or_default()
            .insert(self.review_state.hunk_idx);

        debug!(
            file = ?current_file_path,
            hunk = self.review_state.hunk_idx,
            "Approved hunk"
        );

        // Move to next hunk
        self.diff_review_next_hunk(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn approves_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" && !s.review_state.files.is_empty() {
                s.diff_review_approve_hunk(cx);
                // Verify hunk was marked as approved
                let file_path = &s.review_state.files[s.review_state.file_idx];
                if let Some(approved) = s.review_state.approved_hunks.get(file_path) {
                    assert!(!approved.is_empty());
                }
            }
        });
    }
}
