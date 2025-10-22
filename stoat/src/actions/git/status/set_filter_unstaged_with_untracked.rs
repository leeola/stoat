//! Git status set filter unstaged with untracked action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Set git status filter to show unstaged and untracked files.
    ///
    /// Changes the filter mode to [`crate::git_status::GitStatusFilter::UnstagedWithUntracked`],
    /// which shows both files with unstaged modifications and untracked files. Re-applies the
    /// filter via [`Stoat::filter_git_status_files`], then transitions from git_filter mode
    /// back to git_status mode.
    ///
    /// # Behavior
    ///
    /// - Only operates in git_filter mode
    /// - Sets filter to UnstagedWithUntracked (shows unstaged + untracked)
    /// - Transitions back to git_status mode after setting filter
    /// - Automatically loads preview for first filtered file
    ///
    /// # Related
    ///
    /// - [`Stoat::filter_git_status_files`] - applies filter and loads preview
    /// - [`Stoat::git_status_set_filter_unstaged`] - excludes untracked files
    /// - [`Stoat::git_status_set_filter_untracked`] - shows only untracked
    pub fn git_status_set_filter_unstaged_with_untracked(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_filter" {
            return;
        }

        self.git_status_filter = crate::git_status::GitStatusFilter::UnstagedWithUntracked;
        debug!("Git status: set filter to UnstagedWithUntracked");

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
    fn sets_filter_to_unstaged_with_untracked(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_git_status(cx);
            if s.mode() == "git_status" {
                s.set_mode("git_filter");
                s.git_status_set_filter_unstaged_with_untracked(cx);
                assert_eq!(
                    s.git_status_filter,
                    crate::git_status::GitStatusFilter::UnstagedWithUntracked
                );
                assert_eq!(s.mode(), "git_status");
            }
        });
    }
}
