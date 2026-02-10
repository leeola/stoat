//! Unstage selected lines from line selection mode.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Unstage only the selected lines from the current hunk.
    ///
    /// Same as staging but applies the patch in reverse.
    pub fn diff_review_line_select_unstage(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = self.apply_line_selection(true, cx) {
            tracing::error!("DiffReviewLineSelectUnstage failed: {e}");
        }
    }
}
