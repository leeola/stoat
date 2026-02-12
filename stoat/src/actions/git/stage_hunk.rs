//! Git stage hunk implementation and tests.
//!
//! This module implements the [`git_stage_hunk`](crate::Stoat::git_stage_hunk) action, which
//! stages individual diff hunks via libgit2's index apply. The action is part
//! of the git staging workflow alongside [`git_stage_file`](crate::Stoat::git_stage_file) for
//! staging entire files and [`git_stage_all`](crate::Stoat::git_stage_all) for staging all
//! changes.

use crate::{
    git::{diff::BufferDiff, repository::Repository},
    stoat::Stoat,
};
use gpui::Context;
use text::ToPoint;

impl Stoat {
    /// Toggle the staging state of the current hunk.
    ///
    /// Uses [`staged_hunk_indices`](crate::buffer::BufferItem::staged_hunk_indices)
    /// to detect staged state, which works for all hunk types including pure
    /// deletions. Delegates to [`git_stage_hunk`](Self::git_stage_hunk) or
    /// [`git_unstage_hunk`](Self::git_unstage_hunk).
    pub fn git_toggle_stage_hunk(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let cursor_row = self.cursor.position().row;
        let diff = buffer_item
            .read(cx)
            .diff()
            .ok_or_else(|| "No diff information available".to_string())?;

        let hunk_index = diff
            .hunk_for_row(cursor_row, &buffer_snapshot)
            .ok_or_else(|| format!("No hunk at cursor row {cursor_row}"))?;

        let is_staged = buffer_item
            .read(cx)
            .staged_hunk_indices()
            .is_some_and(|indices| indices.contains(&hunk_index));

        if is_staged {
            self.git_unstage_hunk(cx)
        } else {
            self.git_stage_hunk(cx)
        }
    }

    /// Stage the current hunk for commit.
    ///
    /// Uses the display diff to locate the hunk at the cursor, then computes a
    /// working-vs-index diff to generate a patch against the correct base. If the
    /// hunk is already staged (no working-vs-index difference in that region),
    /// the operation is a no-op and the display is refreshed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No file path is set for the current buffer
    /// - No diff information is available for the file
    /// - No hunk exists at the cursor position
    /// - Failed to generate or apply the patch
    ///
    /// # Related Actions
    ///
    /// - [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) - Unstage this hunk
    /// - [`git_stage_file`](crate::Stoat::git_stage_file) - Stage the entire file
    /// - [`git_stage_all`](crate::Stoat::git_stage_all) - Stage all changes
    pub fn git_stage_hunk(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        let repo_dir = self.worktree_root_abs();

        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        // Use the display diff to find the hunk region at the cursor
        let (display_start, display_end) = {
            let diff = buffer_item
                .read(cx)
                .diff()
                .ok_or_else(|| "No diff information available".to_string())?;
            let hunk_index = diff
                .hunk_for_row(cursor_row, &buffer_snapshot)
                .ok_or_else(|| format!("No hunk at cursor row {cursor_row}"))?;
            let hunk = &diff.hunks[hunk_index];
            let start = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
            let end = hunk.buffer_range.end.to_point(&buffer_snapshot).row;
            (start, end)
        };

        // Compute working-vs-index diff to generate a patch with the correct base
        let repo =
            Repository::discover(&file_path).map_err(|e| format!("Repository not found: {e}"))?;
        let index_content = repo.index_content(&file_path).unwrap_or_default();
        let buffer_id = buffer_snapshot.remote_id();
        let stage_diff = BufferDiff::new(buffer_id, index_content, &buffer_snapshot)
            .map_err(|e| format!("Working-vs-index diff failed: {e}"))?;

        // Find the working-vs-index hunk that overlaps the display hunk's buffer range
        let stage_hunk = stage_diff.hunks.iter().find(|h| {
            let start = h.buffer_range.start.to_point(&buffer_snapshot).row;
            let end = h.buffer_range.end.to_point(&buffer_snapshot).row;
            start <= display_end && end >= display_start
        });

        if let Some(hunk) = stage_hunk {
            let patch = super::hunk_patch::generate_hunk_patch(
                &stage_diff,
                hunk,
                &buffer_snapshot,
                &file_path,
            )?;
            super::hunk_patch::apply_patch(&patch, &repo_dir, false)?;
        }

        if let Some((new_diff, staged_rows, staged_hunk_indices)) =
            self.compute_diff_for_review_mode(&file_path, cx)
        {
            buffer_item.update(cx, |item, _| {
                item.set_diff(Some(new_diff));
                item.set_staged_rows(staged_rows);
                item.set_staged_hunk_indices(staged_hunk_indices);
            });
        }

        tracing::info!("Staged hunk at row {} in {:?}", cursor_row, file_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::*;
    use gpui::TestAppContext;

    fn setup_repo_with_change(stoat: &mut crate::test::TestStoat<'_>) -> std::path::PathBuf {
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

        std::fs::write(&file_path, "line 1\nline 2\nline 3\nnew line\n")
            .expect("Failed to write modified file");

        stoat.set_file_path(file_path.clone());
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let content = std::fs::read_to_string(&file_path).unwrap();
                item.buffer().update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, content.as_str())]);
                });

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

            s.set_cursor_position(text::Point::new(3, 0));
        });

        file_path
    }

    #[gpui::test]
    fn stages_hunk_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        setup_repo_with_change(&mut stoat);

        stoat.dispatch(GitStageHunk);

        let output = std::process::Command::new("git")
            .args(["diff", "--cached"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to execute git diff --cached");

        let diff_output = String::from_utf8_lossy(&output.stdout);
        assert!(
            diff_output.contains("new line"),
            "Staged diff should contain the new line, got: {diff_output}"
        );
    }

    #[gpui::test]
    fn double_stage_is_noop(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        setup_repo_with_change(&mut stoat);

        stoat.dispatch(GitStageHunk);

        // Snapshot the index after first stage
        let first_output = std::process::Command::new("git")
            .args(["diff", "--cached"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to execute git diff --cached");
        let first_staged = String::from_utf8_lossy(&first_output.stdout).to_string();

        // Second stage should succeed (no-op) without changing the index
        let result = stoat.update(|s, cx| s.git_stage_hunk(cx));
        assert!(
            result.is_ok(),
            "Second stage should be a no-op, got: {result:?}"
        );

        let second_output = std::process::Command::new("git")
            .args(["diff", "--cached"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("Failed to execute git diff --cached");
        let second_staged = String::from_utf8_lossy(&second_output.stdout).to_string();

        assert_eq!(
            first_staged, second_staged,
            "Index should not change on double-stage"
        );
    }

    #[gpui::test]
    fn toggle_unstages_deletion_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("test.txt");
        std::fs::write(&file_path, "line 1\nline 2\nline 3\n").expect("write");

        std::process::Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("git commit");

        std::fs::write(&file_path, "line 1\nline 3\n").expect("write deletion");

        stoat.set_file_path(file_path.clone());
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let content = std::fs::read_to_string(&file_path).unwrap();
                item.buffer().update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, content.as_str())]);
                });

                let repo = crate::git::repository::Repository::discover(&file_path).unwrap();
                let head_content = repo.head_content(&file_path).unwrap();
                let snapshot = item.buffer().read(cx).snapshot();
                let diff = crate::git::diff::BufferDiff::new(
                    item.buffer().read(cx).remote_id(),
                    head_content,
                    &snapshot,
                )
                .unwrap();
                item.set_diff(Some(diff));
            });
            s.set_cursor_position(text::Point::new(0, 0));
        });

        stoat.dispatch(GitStageHunk);

        let cached = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["diff", "--cached"])
                .current_dir(stoat.repo_path().unwrap())
                .output()
                .expect("git diff --cached")
                .stdout,
        )
        .to_string();
        assert!(
            cached.contains("-line 2"),
            "Deletion should be staged: {cached}"
        );

        stoat.dispatch(GitToggleStageHunk);

        let cached_after = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["diff", "--cached"])
                .current_dir(stoat.repo_path().unwrap())
                .output()
                .expect("git diff --cached")
                .stdout,
        )
        .to_string();
        assert!(
            cached_after.is_empty(),
            "Toggle should unstage the deletion: {cached_after}"
        );
    }

    #[gpui::test]
    #[should_panic(expected = "GitStageHunk action failed: No file path set for current buffer")]
    fn fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        stoat.dispatch(GitStageHunk);
    }

    #[gpui::test]
    #[should_panic(expected = "GitStageHunk action failed: No diff information available")]
    fn fails_without_diff(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let file_path = stoat.repo_path().unwrap().join("test.txt");
        stoat.set_file_path(file_path);

        stoat.dispatch(GitStageHunk);
    }

    #[gpui::test]
    #[should_panic(expected = "GitStageHunk action failed: No hunk at cursor row")]
    fn fails_when_not_on_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

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

        stoat.set_file_path(file_path.clone());
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let content = std::fs::read_to_string(&file_path).unwrap();
                item.buffer().update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, content.as_str())]);
                });

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

        stoat.dispatch(GitStageHunk);
    }
}
