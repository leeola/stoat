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

        let repo_dir = file_path
            .parent()
            .ok_or_else(|| "File path has no parent directory".to_string())?;

        let output = std::process::Command::new("git")
            .arg("reset")
            .arg("HEAD")
            .arg(
                file_path
                    .file_name()
                    .ok_or_else(|| "Invalid file name".to_string())?,
            )
            .current_dir(repo_dir)
            .output()
            .map_err(|e| format!("Failed to execute git reset HEAD: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git reset HEAD failed: {stderr}"));
        }

        tracing::info!("Unstaged file {:?}", file_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn unstages_file_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let file_path = stoat.repo_path().unwrap().join("test.txt");

        // Create initial commit (needed for git reset HEAD to work)
        std::fs::write(&file_path, "initial").expect("Failed to write file");

        std::process::Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to git add");

        std::process::Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to git commit");

        // Modify and stage the file
        std::fs::write(&file_path, "modified content").expect("Failed to write file");

        std::process::Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to git add");

        // Verify file is staged
        let output = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to execute git diff --cached");

        let staged_files = String::from_utf8_lossy(&output.stdout);
        assert!(
            staged_files.contains("test.txt"),
            "File should be staged before unstaging"
        );

        // Set file path and unstage
        stoat.set_file_path(file_path.clone());
        stoat.dispatch(GitUnstageFile);

        // Verify file is no longer staged
        let output = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to execute git diff --cached");

        let staged_files = String::from_utf8_lossy(&output.stdout);
        assert!(
            !staged_files.contains("test.txt"),
            "File should not be staged after unstaging, got: {staged_files}"
        );
    }

    #[gpui::test]
    #[should_panic(expected = "GitUnstageFile action failed: No file path set for current buffer")]
    fn fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        stoat.dispatch(GitUnstageFile);
    }
}
