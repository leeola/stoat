//! Toggle diff hunk action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Toggle expansion of diff hunk at cursor position (no-op).
    ///
    /// NOTE: This action is now a no-op. In the new phantom row design, all diff hunks
    /// are always visible with their deleted content shown inline via phantom rows.
    /// There is no concept of collapsed/expanded hunks anymore - all hunks display
    /// both their old (deleted) and new (added) content simultaneously.
    ///
    /// # Historical Context
    ///
    /// This action previously toggled between collapsed and expanded views of diff hunks.
    /// The phantom row redesign eliminated this need by showing all content inline.
    ///
    /// # Behavior
    ///
    /// - Does nothing (logs debug message)
    /// - Kept for backward compatibility with keybindings
    ///
    /// # Related
    ///
    /// - [`Stoat::goto_next_hunk`] - navigate to next hunk
    /// - [`Stoat::goto_prev_hunk`] - navigate to previous hunk
    pub fn toggle_diff_hunk(&mut self, _cx: &mut Context<Self>) {
        tracing::debug!("toggle_diff_hunk called (no-op - all hunks always visible)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn toggle_is_noop(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Just verify it doesn't panic
            s.toggle_diff_hunk(cx);
        });
    }
}
