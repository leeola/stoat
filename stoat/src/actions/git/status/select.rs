//! Git status select action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Open selected file from git status.
    ///
    /// Loads the currently selected file from the git status list into the active buffer.
    /// Builds the absolute path from the repository workdir and uses [`Stoat::load_file`]
    /// to ensure git diff computation. Automatically dismisses the git status modal after
    /// selection via [`Stoat::git_status_dismiss`].
    ///
    /// # Workflow
    ///
    /// 1. Gets selected entry from filtered list
    /// 2. Discovers repository from worktree root
    /// 3. Builds absolute path from repository workdir
    /// 4. Loads file using [`Stoat::load_file`] (triggers diff computation)
    /// 5. Dismisses git status modal
    ///
    /// # Behavior
    ///
    /// - Only operates in git_status mode
    /// - Logs error if file load fails (doesn't block dismissal)
    /// - Always dismisses modal after selection attempt
    ///
    /// # Related
    ///
    /// - [`Stoat::load_file`] - file loading with diff computation
    /// - [`Stoat::git_status_dismiss`] - cleanup and mode restoration
    pub fn git_status_select(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        if self.git_status_selected < self.git_status_filtered.len() {
            let entry = &self.git_status_filtered[self.git_status_selected];
            let relative_path = &entry.path;
            debug!(file = ?relative_path, "Git status: select");

            // Build absolute path from repository root
            let root_path = self.worktree.lock().root().to_path_buf();
            if let Ok(repo) = crate::git_repository::Repository::discover(&root_path) {
                let abs_path = repo.workdir().join(relative_path);

                // Load the file
                if let Err(e) = self.load_file(&abs_path, cx) {
                    tracing::error!("Failed to load file {:?}: {}", abs_path, e);
                }
            }
        }

        self.git_status_dismiss(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn selects_file(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_git_status(cx);
            s.git_status_select(cx);
            // Dismiss clears state
            assert_eq!(s.git_status_files.len(), 0);
        });
    }
}
