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
    pub fn diff_review_next_hunk(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        if self.review_state.files.is_empty() {
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
            self.review_state.file_idx,
            self.review_state.hunk_idx,
            hunk_count
        );

        // Try to move to next hunk in current file
        if self.review_state.hunk_idx + 1 < hunk_count {
            // Move to next hunk in current file
            self.review_state.hunk_idx += 1;
            tracing::debug!("Moving to next hunk: {}", self.review_state.hunk_idx);
            self.jump_to_current_hunk(true, cx);
            cx.notify();
        } else {
            // At last hunk, load next file (async, handles its own cache refresh)
            tracing::debug!("At last hunk, loading next file");
            self.load_next_file(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_next_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            if s.mode() == "diff_review" {
                s.diff_review_next_hunk(cx);
            }
        });
    }
}
