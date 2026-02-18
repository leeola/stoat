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
        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        let repo_dir = file_path
            .parent()
            .ok_or_else(|| "File path has no parent directory".to_string())?;

        let output = std::process::Command::new("git")
            .arg("reset")
            .arg("HEAD")
            .current_dir(repo_dir)
            .output()
            .map_err(|e| format!("Failed to execute git reset HEAD: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git reset HEAD failed: {stderr}"));
        }

        tracing::info!("Unstaged all changes in repository at {:?}", repo_dir);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn unstages_all_changes_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let repo_path = stoat.repo_path().unwrap().to_path_buf();

        // Create initial commit (needed for git reset HEAD to work)
        let init_file = repo_path.join("init.txt");
        std::fs::write(&init_file, "init").expect("Failed to write init file");

        std::process::Command::new("git")
            .args(["add", "init.txt"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to git add");

        std::process::Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to git commit");

        // Create and stage multiple files
        let file1 = repo_path.join("file1.txt");
        let file2 = repo_path.join("file2.txt");
        let file3 = repo_path.join("file3.txt");

        std::fs::write(&file1, "content 1").expect("Failed to write file1");
        std::fs::write(&file2, "content 2").expect("Failed to write file2");
        std::fs::write(&file3, "content 3").expect("Failed to write file3");

        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to git add");

        // Verify files are staged
        let output = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to execute git diff --cached");

        let staged_files = String::from_utf8_lossy(&output.stdout);
        assert!(
            staged_files.contains("file1.txt"),
            "file1.txt should be staged"
        );

        // Set file path and unstage all
        stoat.set_file_path(file1.clone());
        stoat.update(|s, cx| s.git_unstage_all(cx).unwrap());

        // Verify no files are staged
        let output = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to execute git diff --cached");

        let staged_files = String::from_utf8_lossy(&output.stdout);
        assert!(
            staged_files.trim().is_empty(),
            "No files should be staged after unstage all, got: {staged_files}"
        );
    }

    #[gpui::test]
    #[should_panic(expected = "No file path set for current buffer")]
    fn fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        stoat.update(|s, cx| s.git_unstage_all(cx).unwrap());
    }
}
