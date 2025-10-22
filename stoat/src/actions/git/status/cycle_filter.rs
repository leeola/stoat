//! Git status cycle filter action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Cycle to next git status filter.
    ///
    /// Rotates through filter modes (All to Staged to Unstaged to UnstagedWithUntracked to
    /// Untracked to All) using [`crate::git_status::GitStatusFilter::next`], then re-applies
    /// the filter to the files list via [`Stoat::filter_git_status_files`]. This updates the
    /// filtered list, resets selection to 0, and loads a preview for the first filtered file.
    ///
    /// # Behavior
    ///
    /// - Only operates in git_status mode
    /// - Uses cyclic rotation (wraps from Untracked back to All)
    /// - Automatically loads preview for newly selected file
    ///
    /// # Related
    ///
    /// - [`crate::git_status::GitStatusFilter::next`] - filter cycling logic
    /// - [`Stoat::filter_git_status_files`] - applies filter and loads preview
    /// - [`Stoat::git_status_set_filter_all`] - set filter directly to All
    pub fn git_status_cycle_filter(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        self.git_status_filter = self.git_status_filter.next();
        debug!(filter = ?self.git_status_filter, "Git status: cycled filter");

        self.filter_git_status_files(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn cycles_filter(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_git_status(cx);
            if s.mode() == "git_status" {
                let initial_filter = s.git_status_filter;
                s.git_status_cycle_filter(cx);
                // Filter should change
                assert_ne!(s.git_status_filter, initial_filter);
            }
        });
    }
}
