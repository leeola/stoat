//! Git unstage all implementation and tests.
//!
//! This module implements the [`git_unstage_all`](crate::Stoat::git_unstage_all) action, which
//! unstages all changes in the repository using `git reset HEAD`. The action is part of the git
//! staging workflow alongside [`git_unstage_file`](crate::Stoat::git_unstage_file) for
//! unstaging individual files and [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) for
//! unstaging hunks.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Unstage all changes in the repository using `git reset HEAD`.
    ///
    /// Executes `git reset HEAD` to remove all files from the staging area while
    /// preserving working directory changes. The current file path must be set to
    /// determine the repository location via [`set_file_path`](crate::Stoat::set_file_path).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No file path is set for the current buffer
    /// - The file path has no parent directory
    /// - The file is not in a git repository
    /// - The `git reset HEAD` command fails
    ///
    /// # Related Actions
    ///
    /// - [`git_unstage_file`](crate::Stoat::git_unstage_file) - Unstage only the current file
    /// - [`git_stage_all`](crate::Stoat::git_stage_all) - Stage all changes
    /// - [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) - Unstage individual hunks
    pub fn git_unstage_all(&mut self, _cx: &mut Context<Self>) -> Result<(), String> {
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = self
            .services
            .git
            .discover(&root_path)
            .map_err(|e| format!("git unstage all failed: {e}"))?;

        repo.unstage_all()
            .map_err(|e| format!("git unstage all failed: {e}"))?;

        tracing::info!("Unstaged all changes in repository at {:?}", root_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn unstages_all_changes_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file1 = PathBuf::from("file1.txt");
        let file2 = PathBuf::from("file2.txt");
        let file3 = PathBuf::from("file3.txt");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_exists(true);
            s.services
                .fake_git()
                .set_workdir(PathBuf::from("/fake/repo"));
        });

        // Stage files individually via the fake
        stoat.update(|s, cx| {
            s.current_file_path = Some(file1.clone());
            s.git_stage_file(cx).unwrap();
            s.current_file_path = Some(file2.clone());
            s.git_stage_file(cx).unwrap();
            s.current_file_path = Some(file3.clone());
            s.git_stage_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            assert_eq!(s.services.fake_git().staged_files().len(), 3);
        });

        stoat.update(|s, cx| s.git_unstage_all(cx).unwrap());

        stoat.update(|s, _cx| {
            assert!(s.services.fake_git().staged_files().is_empty());
        });
    }

    #[gpui::test]
    #[should_panic(expected = "git unstage all failed")]
    fn fails_outside_git_repo(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.git_unstage_all(cx).unwrap());
    }
}
