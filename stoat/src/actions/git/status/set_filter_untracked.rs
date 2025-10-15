//! Git status set filter untracked action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Set git status filter to show only untracked files.
    ///
    /// Changes the filter mode to [`crate::git_status::GitStatusFilter::Untracked`], which shows
    /// only files that are not tracked by git (new files not yet added). Re-applies the filter via
    /// [`Stoat::filter_git_status_files`], then transitions from git_filter mode back to
    /// git_status mode.
    ///
    /// # Behavior
    ///
    /// - Only operates in git_filter mode
    /// - Sets filter to Untracked (shows only new/untracked files)
    /// - Transitions back to git_status mode after setting filter
    /// - Automatically loads preview for first filtered file
    ///
    /// # Related
    ///
    /// - [`Stoat::filter_git_status_files`] - applies filter and loads preview
    /// - [`Stoat::git_status_set_filter_unstaged_with_untracked`] - includes unstaged files
    /// - [`Stoat::git_status_set_filter_all`] - shows all file types
    pub fn git_status_set_filter_untracked(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_filter" {
            return;
        }

        self.git_status_filter = crate::git_status::GitStatusFilter::Untracked;
        debug!("Git status: set filter to Untracked");

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
    fn sets_filter_to_untracked(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_git_status(cx);
            if s.mode() == "git_status" {
                s.set_mode("git_filter");
                s.git_status_set_filter_untracked(cx);
                assert_eq!(
                    s.git_status_filter,
                    crate::git_status::GitStatusFilter::Untracked
                );
                assert_eq!(s.mode(), "git_status");
            }
        });
    }
}
