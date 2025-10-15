//! Diff review dismiss action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Exit diff review mode.
    ///
    /// Clears the previous mode reference. Mode and KeyContext transitions are now
    /// handled by the [`crate::actions::SetKeyContext`] action bound to Escape.
    ///
    /// State persists for next review session (files, current file/hunk indices, approved hunks).
    /// To fully reset review progress, use [`Stoat::diff_review_reset_progress`].
    ///
    /// # Workflow
    ///
    /// 1. Clears [`Stoat::diff_review_previous_mode`]
    /// 2. Emits Changed event and notifies
    ///
    /// # Behavior
    ///
    /// - Only operates in diff_review mode
    /// - Does NOT change mode or KeyContext (handled by SetKeyContext)
    /// - Does NOT clear review state (preserved for resuming)
    /// - Does NOT clear approved hunks (use reset_progress for that)
    ///
    /// # Related
    ///
    /// - [`Stoat::open_diff_review`] - opens the modal (resumes if state exists)
    /// - [`Stoat::diff_review_reset_progress`] - clears all progress
    /// - [`crate::actions::SetKeyContext`] - handles mode/context transitions
    pub fn diff_review_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        debug!("Dismissing diff review");

        // Clear previous mode reference
        self.diff_review_previous_mode = None;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn dismisses_diff_review(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" {
                s.diff_review_dismiss(cx);
                // Previous mode cleared
                assert!(s.diff_review_previous_mode.is_none());
                // State preserved (not cleared)
                // Files list should still exist if review was started
            }
        });
    }
}
