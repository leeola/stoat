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
        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        let repo_dir = file_path
            .parent()
            .ok_or_else(|| "File path has no parent directory".to_string())?;

        let output = std::process::Command::new("git")
            .arg("add")
            .arg("-A")
            .current_dir(repo_dir)
            .output()
            .map_err(|e| format!("Failed to execute git add -A: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git add -A failed: {stderr}"));
        }

        tracing::info!("Staged all changes in repository at {:?}", repo_dir);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn stages_all_changes_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let repo_path = stoat.repo_path().unwrap().to_path_buf();

        // Create multiple files
        let file1 = repo_path.join("file1.txt");
        let file2 = repo_path.join("file2.txt");
        let file3 = repo_path.join("file3.txt");

        std::fs::write(&file1, "content 1").expect("Failed to write file1");
        std::fs::write(&file2, "content 2").expect("Failed to write file2");
        std::fs::write(&file3, "content 3").expect("Failed to write file3");

        // Set file path (required for git_stage_all to find repo)
        stoat.set_file_path(file1.clone());

        // Stage all changes
        stoat.update(|s, cx| s.git_stage_all(cx).unwrap());

        // Verify all files are staged
        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to execute git status");

        let status = String::from_utf8_lossy(&output.stdout);

        // All files should be staged (status starts with 'A ')
        assert!(
            status.contains("A  file1.txt"),
            "file1.txt should be staged, got: {status}"
        );
        assert!(
            status.contains("A  file2.txt"),
            "file2.txt should be staged, got: {status}"
        );
        assert!(
            status.contains("A  file3.txt"),
            "file3.txt should be staged, got: {status}"
        );
    }

    #[gpui::test]
    #[should_panic(expected = "No file path set for current buffer")]
    fn fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        stoat.update(|s, cx| s.git_stage_all(cx).unwrap());
    }

    #[gpui::test]
    #[should_panic(expected = "git add -A failed")]
    fn fails_outside_git_repo(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let file_path = temp_dir.path().join("test.txt");

        std::fs::write(&file_path, "content").expect("Failed to write file");
        stoat.set_file_path(file_path);

        stoat.update(|s, cx| s.git_stage_all(cx).unwrap());
    }
}
