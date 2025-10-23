//! Git unstage hunk implementation and tests.
//!
//! This module implements the [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) action, which
//! unstages individual diff hunks using `git apply --cached --reverse --unidiff-zero`. The
//! action is part of the git staging workflow alongside
//! [`git_unstage_file`](crate::Stoat::git_unstage_file) for unstaging entire files and
//! [`git_unstage_all`](crate::Stoat::git_unstage_all) for unstaging all changes.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Unstage the current hunk.
    ///
    /// Finds the git diff hunk at the cursor position and unstages only that hunk using
    /// `git apply --cached --reverse --unidiff-zero`. The file must have diff information
    /// available via [`BufferDiff`](crate::git::diff::BufferDiff). The patch is generated
    /// by [`generate_hunk_patch`](super::hunk_patch::generate_hunk_patch) and applied in
    /// reverse to the staging area.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No file path is set for the current buffer
    /// - The file path has no parent directory
    /// - No diff information is available for the file
    /// - No hunk exists at the cursor position
    /// - Failed to generate or apply the patch
    ///
    /// # Implementation
    ///
    /// Uses [`BufferDiff::hunk_for_row`](crate::git::diff::BufferDiff::hunk_for_row) to find
    /// the hunk at the cursor, then generates a minimal unified diff patch and applies it
    /// in reverse to remove it from the staging area.
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_hunk`](crate::Stoat::git_stage_hunk) - Stage this hunk
    /// - [`git_unstage_file`](crate::Stoat::git_unstage_file) - Unstage the entire file
    /// - [`git_unstage_all`](crate::Stoat::git_unstage_all) - Unstage all changes
    pub fn git_unstage_hunk(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        let repo_dir = file_path
            .parent()
            .ok_or_else(|| "File path has no parent directory".to_string())?;

        // Get diff and find hunk at cursor
        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let diff = buffer_item
            .read(cx)
            .diff()
            .ok_or_else(|| "No diff information available".to_string())?;

        let hunk_index = diff
            .hunk_for_row(cursor_row, &buffer_snapshot)
            .ok_or_else(|| format!("No hunk at cursor row {cursor_row}"))?;

        let hunk = &diff.hunks[hunk_index];

        // Generate patch for this hunk
        let patch =
            super::hunk_patch::generate_hunk_patch(diff, hunk, &buffer_snapshot, &file_path)?;

        // Apply patch in reverse to unstage
        let mut child = std::process::Command::new("git")
            .args(["apply", "--cached", "--reverse", "--unidiff-zero", "-"])
            .current_dir(repo_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn git apply: {e}"))?;

        {
            use std::io::Write;
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| "Failed to open stdin".to_string())?;
            stdin
                .write_all(patch.as_bytes())
                .map_err(|e| format!("Failed to write patch to stdin: {e}"))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| format!("Failed to wait for git apply: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git apply --reverse failed: {stderr}"));
        }

        tracing::info!("Unstaged hunk at row {} in {:?}", cursor_row, file_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn unstages_hunk_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        // Create initial file and commit it
        let file_path = stoat.repo_path().unwrap().join("test.txt");
        std::fs::write(&file_path, "line 1\nline 2\nline 3\n").expect("Failed to write file");

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

        // Modify the file
        std::fs::write(&file_path, "line 1\nline 2\nline 3\nnew line\n")
            .expect("Failed to write modified file");

        // Stage the entire file
        std::process::Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to git add");

        // Load file and compute diff
        stoat.set_file_path(file_path.clone());
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let content = std::fs::read_to_string(&file_path).unwrap();
                item.buffer().update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, content.as_str())]);
                });

                // Compute diff
                let repo = crate::git::repository::Repository::discover(&file_path).unwrap();
                let head_content = repo.head_content(&file_path).unwrap();
                let buffer_snapshot = item.buffer().read(cx).snapshot();
                let diff = crate::git::diff::BufferDiff::new(
                    item.buffer().read(cx).remote_id(),
                    head_content,
                    &buffer_snapshot,
                )
                .unwrap();
                item.set_diff(Some(diff));
            });

            // Move cursor to the changed hunk (line 3)
            s.set_cursor_position(text::Point::new(3, 0));
        });

        // Unstage the hunk
        stoat.dispatch(GitUnstageHunk);

        // Verify hunk is no longer staged
        let output = std::process::Command::new("git")
            .args(["diff", "--cached"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to execute git diff --cached");

        let diff_output = String::from_utf8_lossy(&output.stdout);
        assert!(
            diff_output.trim().is_empty(),
            "Staged diff should be empty after unstaging hunk, got: {diff_output}"
        );
    }

    #[gpui::test]
    #[should_panic(expected = "GitUnstageHunk action failed: No file path set for current buffer")]
    fn fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        stoat.dispatch(GitUnstageHunk);
    }

    #[gpui::test]
    #[should_panic(expected = "GitUnstageHunk action failed: No diff information available")]
    fn fails_without_diff(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let file_path = stoat.repo_path().unwrap().join("test.txt");
        stoat.set_file_path(file_path);

        stoat.dispatch(GitUnstageHunk);
    }

    #[gpui::test]
    #[should_panic(expected = "GitUnstageHunk action failed: No hunk at cursor row")]
    fn fails_when_not_on_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        // Create initial file and commit it
        let file_path = stoat.repo_path().unwrap().join("test.txt");
        std::fs::write(&file_path, "line 1\n").expect("Failed to write file");

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

        // Load file (unchanged, so no diff)
        stoat.set_file_path(file_path.clone());
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let content = std::fs::read_to_string(&file_path).unwrap();
                item.buffer().update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, content.as_str())]);
                });

                // Compute diff (will be empty)
                let repo = crate::git::repository::Repository::discover(&file_path).unwrap();
                let head_content = repo.head_content(&file_path).unwrap();
                let buffer_snapshot = item.buffer().read(cx).snapshot();
                let diff = crate::git::diff::BufferDiff::new(
                    item.buffer().read(cx).remote_id(),
                    head_content,
                    &buffer_snapshot,
                )
                .unwrap();
                item.set_diff(Some(diff));
            });
        });

        // Try to unstage hunk when cursor is not on any hunk
        stoat.dispatch(GitUnstageHunk);
    }
}
