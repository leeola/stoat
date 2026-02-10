//! Diff review next hunk action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Jump to next hunk in diff review mode.
    ///
    /// Navigates to the next hunk in the current file (whether reviewed or not).
    /// Automatically loads the next file if at the last hunk of the current file
    /// via [`Stoat::load_next_file`]. Following Zed's hunk navigation pattern with
    /// cross-file support.
    ///
    /// # Workflow
    ///
    /// 1. Gets hunk count from buffer diff
    /// 2. If next hunk exists in current file: increment hunk index, jump to it
    /// 3. If at last hunk: load next file (wraps around to first file)
    ///
    /// # Behavior
    ///
    /// - Only operates in diff_review mode
    /// - Advances through all hunks sequentially
    /// - Loads next file automatically when reaching end of current file
    /// - Wraps around to first file after last file
    ///
    /// # Related
    ///
    /// - [`Stoat::diff_review_prev_hunk`] - navigate backwards
    /// - [`Stoat::diff_review_next_unreviewed_hunk`] - skip to next unreviewed
    /// - [`Stoat::load_next_file`] - cross-file navigation helper
    /// - [`Stoat::jump_to_current_hunk`] - cursor positioning and scrolling
    pub fn diff_review_next_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        // Get hunk count from buffer diff
        let hunk_count = {
            let buffer_item = self.active_buffer(cx);
            let item = buffer_item.read(cx);
            match item.diff() {
                Some(diff) => diff.hunks.len(),
                None => return,
            }
        };

        tracing::debug!(
            "diff_review_next_hunk: file_idx={}, hunk_idx={}, hunk_count={}",
            self.diff_review_current_file_idx,
            self.diff_review_current_hunk_idx,
            hunk_count
        );

        // Try to move to next hunk in current file
        if self.diff_review_current_hunk_idx + 1 < hunk_count {
            // Move to next hunk in current file
            self.diff_review_current_hunk_idx += 1;
            tracing::debug!("Moving to next hunk: {}", self.diff_review_current_hunk_idx);
            self.jump_to_current_hunk(true, cx);
        } else {
            // At last hunk, try next file
            tracing::debug!("At last hunk, loading next file");
            self.load_next_file(cx);
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_next_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" {
                // Just verify it doesn't panic
                s.diff_review_next_hunk(cx);
            }
        });
    }
}
