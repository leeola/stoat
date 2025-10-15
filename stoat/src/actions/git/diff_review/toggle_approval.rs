//! Diff review toggle approval action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Toggle approval status of current hunk.
    ///
    /// Toggles the current hunk between reviewed and not reviewed in
    /// [`Stoat::diff_review_approved_hunks`]. Unlike [`Stoat::diff_review_approve_hunk`], this
    /// stays on the current hunk (doesn't advance). Useful for marking things you've already
    /// seen without moving to the next hunk.
    ///
    /// # Workflow
    ///
    /// 1. Gets current file path from [`Stoat::diff_review_files`]
    /// 2. Checks if hunk is in approved set
    /// 3. If approved: removes from set
    /// 4. If not approved: adds to set
    ///
    /// # Behavior
    ///
    /// - Only operates in diff_review mode
    /// - Returns early if current file index out of bounds
    /// - Does not advance to next hunk (stays in place)
    /// - Approval state persists across sessions
    ///
    /// # Related
    ///
    /// - [`Stoat::diff_review_approve_hunk`] - approve and advance
    /// - [`Stoat::diff_review_next_unreviewed_hunk`] - skip to next unreviewed
    /// - [`Stoat::diff_review_reset_progress`] - clear all approvals
    pub fn diff_review_toggle_approval(&mut self, _cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        // Get current file path
        let current_file_path = match self
            .diff_review_files
            .get(self.diff_review_current_file_idx)
        {
            Some(path) => path.clone(),
            None => return,
        };

        let approved_hunks = self
            .diff_review_approved_hunks
            .entry(current_file_path.clone())
            .or_default();

        if approved_hunks.contains(&self.diff_review_current_hunk_idx) {
            // Currently approved - unapprove it
            approved_hunks.remove(&self.diff_review_current_hunk_idx);
            debug!(
                file = ?current_file_path,
                hunk = self.diff_review_current_hunk_idx,
                "Unapproved hunk"
            );
        } else {
            // Not approved - approve it
            approved_hunks.insert(self.diff_review_current_hunk_idx);
            debug!(
                file = ?current_file_path,
                hunk = self.diff_review_current_hunk_idx,
                "Approved hunk"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn toggles_approval(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" && !s.diff_review_files.is_empty() {
                let file_path = &s.diff_review_files[s.diff_review_current_file_idx].clone();
                let hunk_idx = s.diff_review_current_hunk_idx;

                // Toggle on
                s.diff_review_toggle_approval(cx);
                assert!(s
                    .diff_review_approved_hunks
                    .get(file_path)
                    .map(|set| set.contains(&hunk_idx))
                    .unwrap_or(false));

                // Toggle off
                s.diff_review_toggle_approval(cx);
                assert!(!s
                    .diff_review_approved_hunks
                    .get(file_path)
                    .map(|set| set.contains(&hunk_idx))
                    .unwrap_or(false));
            }
        });
    }
}
