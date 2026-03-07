//! Git unstage file implementation and tests.
//!
//! This module implements the [`git_unstage_file`](crate::Stoat::git_unstage_file) action, which
//! unstages individual file changes using `git reset HEAD`. The action is part of the git
//! staging workflow alongside [`git_unstage_all`](crate::Stoat::git_unstage_all) for unstaging
//! all changes and [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) for unstaging hunks.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Unstage the current file's changes using `git reset HEAD`.
    ///
    /// Executes `git reset HEAD <file>` to remove the current file from the staging area
    /// while preserving working directory changes. The file path must be set via
    /// [`set_file_path`](crate::Stoat::set_file_path).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No file path is set for the current buffer
    /// - The file path has no parent directory
    /// - The file name is invalid
    /// - The file is not in a git repository
    /// - The `git reset HEAD` command fails
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_file`](crate::Stoat::git_stage_file) - Stage this file
    /// - [`git_unstage_all`](crate::Stoat::git_unstage_all) - Unstage all changes
    /// - [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) - Unstage individual hunks
    pub fn git_unstage_file(&mut self, _cx: &mut Context<Self>) -> Result<(), String> {
        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = self
            .services
            .git
            .discover(&root_path)
            .map_err(|e| format!("git unstage failed: {e}"))?;

        repo.unstage_file(&file_path)
            .map_err(|e| format!("git unstage failed: {e}"))?;

        tracing::info!("Unstaged file {:?}", file_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn unstages_file_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file_path = PathBuf::from("/fake/repo/test.txt");
        stoat.set_file_path(file_path.clone());
        stoat.update(|s, _cx| {
            s.services.fake_git().set_exists(true);
            s.services
                .fake_git()
                .set_workdir(PathBuf::from("/fake/repo"));
        });

        // Stage the file first
        stoat.update(|s, cx| s.git_stage_file(cx).unwrap());
        stoat.update(|s, _cx| {
            assert!(s.services.fake_git().staged_files().contains(&file_path));
        });

        // Unstage
        stoat.update(|s, cx| s.git_unstage_file(cx).unwrap());
        stoat.update(|s, _cx| {
            assert!(!s.services.fake_git().staged_files().contains(&file_path));
        });
    }

    #[gpui::test]
    #[should_panic(expected = "No file path set for current buffer")]
    fn fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.git_unstage_file(cx).unwrap());
    }
}
