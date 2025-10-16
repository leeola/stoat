//! Git stage file implementation and tests.
//!
//! This module implements the [`git_stage_file`](crate::Stoat::git_stage_file) action, which
//! stages individual file changes for commit using `git add`. The action is part of the git
//! staging workflow alongside [`git_stage_all`](crate::Stoat::git_stage_all) for staging all
//! changes and [`git_stage_hunk`](crate::Stoat::git_stage_hunk) for staging individual hunks.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Stage the current file for commit using `git add`.
    ///
    /// Executes `git add <file>` to stage the current file's changes for the next commit.
    /// The file path must be set on the current buffer via
    /// [`set_file_path`](crate::Stoat::set_file_path).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No file path is set for the current buffer
    /// - The file path has no parent directory
    /// - The file name is invalid
    /// - The git add command fails (e.g., not in a git repository)
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_all`](crate::Stoat::git_stage_all) - Stage all changes in the repository
    /// - [`git_unstage_file`](crate::Stoat::git_unstage_file) - Unstage this file
    /// - [`git_stage_hunk`](crate::Stoat::git_stage_hunk) - Stage individual hunks
    pub fn git_stage_file(&mut self, _cx: &mut Context<Self>) -> Result<(), String> {
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
            .arg(
                file_path
                    .file_name()
                    .ok_or_else(|| "Invalid file name".to_string())?,
            )
            .current_dir(repo_dir)
            .output()
            .map_err(|e| format!("Failed to execute git add: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git add failed: {stderr}"));
        }

        tracing::info!("Staged file {:?}", file_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn stages_file_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let file_path = stoat.repo_path().unwrap().join("test.txt");
        stoat.set_file_path(file_path.clone());

        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Hello from Stoat!".to_string()));
        stoat.dispatch(WriteFile);

        stoat.dispatch(GitStageFile);

        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to execute git status");

        let status = String::from_utf8_lossy(&output.stdout);
        assert!(
            status.starts_with("A "),
            "File should be staged (status should start with 'A '), got: {status}"
        );
    }

    #[gpui::test]
    #[should_panic(expected = "GitStageFile action failed: No file path set for current buffer")]
    fn fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Hello".to_string()));

        stoat.dispatch(GitStageFile);
    }

    #[gpui::test]
    #[should_panic(expected = "GitStageFile action failed: git add failed")]
    fn fails_outside_git_repo(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let file_path = temp_dir.path().join("test.txt");
        stoat.set_file_path(file_path.clone());

        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Hello".to_string()));
        stoat.dispatch(WriteFile);

        stoat.dispatch(GitStageFile);
    }
}
