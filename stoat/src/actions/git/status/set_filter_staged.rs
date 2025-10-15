//! Git status set filter staged action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Set git status filter to show only staged files.
    ///
    /// Changes the filter mode to [`crate::git_status::GitStatusFilter::Staged`], which shows
    /// only files that have been staged (added to the index). Re-applies the filter via
    /// [`Stoat::filter_git_status_files`], then transitions from git_filter mode back to
    /// git_status mode.
    ///
    /// # Behavior
    ///
    /// - Only operates in git_filter mode
    /// - Sets filter to Staged (shows only staged files)
    /// - Transitions back to git_status mode after setting filter
    /// - Automatically loads preview for first filtered file
    ///
    /// # Related
    ///
    /// - [`Stoat::filter_git_status_files`] - applies filter and loads preview
    /// - [`Stoat::git_status_set_filter_all`] - set to All filter
    /// - [`Stoat::git_status_set_filter_unstaged`] - set to Unstaged filter
    pub fn git_status_set_filter_staged(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_filter" {
            return;
        }

        self.git_status_filter = crate::git_status::GitStatusFilter::Staged;
        debug!("Git status: set filter to Staged");

        self.filter_git_status_files(cx);
        self.set_mode("git_status");
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn sets_filter_to_staged(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_git_status(cx);
            if s.mode() == "git_status" {
                s.set_mode("git_filter");
                s.git_status_set_filter_staged(cx);
                assert_eq!(
                    s.git_status_filter,
                    crate::git_status::GitStatusFilter::Staged
                );
                assert_eq!(s.mode(), "git_status");
            }
        });
    }
}
