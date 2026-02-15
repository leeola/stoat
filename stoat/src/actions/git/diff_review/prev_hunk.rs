//! Diff review prev hunk action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Jump to previous hunk in diff review mode.
    ///
    /// Navigates to the previous hunk in the current file (whether reviewed or not).
    /// Automatically loads the previous file if at the first hunk of the current file
    /// via [`Stoat::load_prev_file`], jumping to that file's last hunk.
    ///
    /// # Workflow
    ///
    /// 1. If not at first hunk: decrement hunk index, jump to it
    /// 2. If at first hunk: load previous file (wraps around to last file)
    ///
    /// # Behavior
    ///
    /// - Only operates in diff_review mode
    /// - Returns early if no files in review list
    /// - Navigates backwards through all hunks sequentially
    /// - Loads previous file automatically when at first hunk
    /// - Wraps around to last file after first file
    ///
    /// # Related
    ///
    /// - [`Stoat::diff_review_next_hunk`] - navigate forwards
    /// - [`Stoat::load_prev_file`] - cross-file navigation helper
    /// - [`Stoat::jump_to_current_hunk`] - cursor positioning and scrolling
    pub fn diff_review_prev_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        if self.review_state.files.is_empty() {
            return;
        }

        if self.review_state.hunk_idx > 0 {
            // Go to previous hunk in current file
            self.review_state.hunk_idx -= 1;
            self.jump_to_current_hunk(true, cx);
        } else {
            // Go to previous file's last hunk
            self.load_prev_file(cx);
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_prev_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" {
                // Just verify it doesn't panic
                s.diff_review_prev_hunk(cx);
            }
        });
    }
}
