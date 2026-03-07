//! Git stage all implementation and tests.
//!
//! This module implements the [`git_stage_all`](crate::Stoat::git_stage_all) action, which
//! stages all changes in the repository using `git add -A`. The action is part of the git
//! staging workflow alongside [`git_stage_file`](crate::Stoat::git_stage_file) for staging
//! individual files and [`git_stage_hunk`](crate::Stoat::git_stage_hunk) for staging hunks.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Stage all changes in the repository for commit using `git add -A`.
    ///
    /// Executes `git add -A` to stage all modified, deleted, and untracked files.
    /// The current file path must be set to determine the repository location via
    /// [`set_file_path`](crate::Stoat::set_file_path).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No file path is set for the current buffer
    /// - The file path has no parent directory
    /// - The file is not in a git repository
    /// - The `git add -A` command fails
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_file`](crate::Stoat::git_stage_file) - Stage only the current file
    /// - [`git_unstage_all`](crate::Stoat::git_unstage_all) - Unstage all changes
    /// - [`git_stage_hunk`](crate::Stoat::git_stage_hunk) - Stage individual hunks
    pub fn git_stage_all(&mut self, _cx: &mut Context<Self>) -> Result<(), String> {
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = self
            .services
            .git
            .discover(&root_path)
            .map_err(|e| format!("git stage all failed: {e}"))?;

        repo.stage_all()
            .map_err(|e| format!("git stage all failed: {e}"))?;

        tracing::info!("Staged all changes in repository at {:?}", root_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::status::GitStatusEntry;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn stages_all_changes_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, _cx| {
            s.services.fake_git().set_exists(true);
            s.services
                .fake_git()
                .set_workdir(PathBuf::from("/fake/repo"));
            s.services.fake_git().set_status(vec![
                GitStatusEntry::new(PathBuf::from("file1.txt"), "??".into(), false),
                GitStatusEntry::new(PathBuf::from("file2.txt"), "??".into(), false),
                GitStatusEntry::new(PathBuf::from("file3.txt"), "??".into(), false),
            ]);
        });

        stoat.update(|s, cx| s.git_stage_all(cx).unwrap());

        stoat.update(|s, _cx| {
            let staged = s.services.fake_git().staged_files();
            assert!(staged.contains(&PathBuf::from("file1.txt")));
            assert!(staged.contains(&PathBuf::from("file2.txt")));
            assert!(staged.contains(&PathBuf::from("file3.txt")));
        });
    }

    #[gpui::test]
    #[should_panic(expected = "git stage all failed")]
    fn fails_outside_git_repo(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.git_stage_all(cx).unwrap());
    }
}
