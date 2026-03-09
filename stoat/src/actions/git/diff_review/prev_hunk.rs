//! Diff review prev hunk action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Jump to previous hunk in diff review mode.
    ///
    /// Navigates to the previous hunk in the current file (whether reviewed or not).
    /// Automatically loads the previous file if at the first hunk of the current file
    /// via [`Stoat::load_prev_file`], jumping to that file's last hunk.
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
            cx.notify();
        } else {
            // Go to previous file's last hunk (async, handles its own cache refresh)
            self.load_prev_file(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_prev_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            if s.mode() == "diff_review" {
                s.diff_review_prev_hunk(cx);
            }
        });
    }
}
