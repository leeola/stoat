//! Diff review toggle follow mode action.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Toggle live follow mode in diff review.
    ///
    /// When follow is enabled, file watcher events auto-navigate to the most recently
    /// modified file's new hunks. When disabled, the diff still refreshes but the cursor
    /// stays put.
    pub fn diff_review_toggle_follow(&mut self, cx: &mut Context<Self>) {
        self.review_state.follow = !self.review_state.follow;
        if self.review_state.follow {
            self.refresh_review_hunk_snapshot();
        }
        debug!(follow = self.review_state.follow, "Toggled follow mode");
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn toggles_follow(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() != "diff_review" {
                return;
            }

            assert!(!s.review_state.follow);
            s.diff_review_toggle_follow(cx);
            assert!(s.review_state.follow);
            s.diff_review_toggle_follow(cx);
            assert!(!s.review_state.follow);
        });
    }
}
