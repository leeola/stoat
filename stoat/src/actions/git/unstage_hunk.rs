//! Git unstage hunk implementation and tests.
//!
//! This module implements the [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) action, which
//! unstages individual diff hunks via libgit2's index apply. The
//! action is part of the git staging workflow alongside
//! [`git_unstage_file`](crate::Stoat::git_unstage_file) for unstaging entire files and
//! [`git_unstage_all`](crate::Stoat::git_unstage_all) for unstaging all changes.

use crate::stoat::Stoat;
use git2::DiffOptions;
use gpui::Context;

impl Stoat {
    /// Unstage the current hunk.
    ///
    /// Uses the display diff (working-vs-HEAD) to locate the hunk at cursor, then
    /// computes an index-vs-HEAD diff via [`git2::Patch::from_buffers`] to generate
    /// the correct patch for reverse-application. This avoids content mismatches
    /// that occur when the working copy and index differ.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No file path is set for the current buffer
    /// - No diff information is available for the file
    /// - No hunk exists at the cursor position
    /// - Failed to compute the index-vs-HEAD diff or apply the patch
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_hunk`](crate::Stoat::git_stage_hunk) - Stage this hunk
    /// - [`git_unstage_file`](crate::Stoat::git_unstage_file) - Unstage the entire file
    /// - [`git_unstage_all`](crate::Stoat::git_unstage_all) - Unstage all changes
    pub fn git_unstage_hunk(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        if self.review_state.source.is_commit() {
            return Ok(());
        }

        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        // Use the display diff to find the HEAD-side row range of the hunk at cursor
        let (display_old_start, display_old_end) = {
            let diff = buffer_item
                .read(cx)
                .diff()
                .ok_or_else(|| "No diff information available".to_string())?;
            let hunk_index = diff
                .hunk_for_row(cursor_row, buffer_snapshot)
                .ok_or_else(|| format!("No hunk at cursor row {cursor_row}"))?;
            let hunk = &diff.hunks[hunk_index];
            (hunk.old_start, hunk.old_start + hunk.old_lines)
        };

        let repo = self
            .services
            .git
            .discover(&file_path)
            .map_err(|e| format!("Repository not found: {e}"))?;
        let head_content = repo.head_content(&file_path).unwrap_or_default();
        let index_content = repo.index_content(&file_path).unwrap_or_default();

        let mut diff_options = DiffOptions::new();
        diff_options.context_lines(0);
        diff_options.ignore_whitespace(false);

        let patch = git2::Patch::from_buffers(
            head_content.as_bytes(),
            None,
            index_content.as_bytes(),
            None,
            Some(&mut diff_options),
        )
        .map_err(|e| format!("Index-vs-HEAD diff failed: {e}"))?;

        // Find the index-vs-HEAD hunk whose HEAD-side range overlaps the display hunk.
        // Zero-length ranges (pure additions) are expanded to length 1 for overlap.
        let found_hunk = (0..patch.num_hunks()).find(|&idx| {
            let Ok((hdr, _)) = patch.hunk(idx) else {
                return false;
            };
            let old_start = hdr.old_start();
            let old_end = old_start + hdr.old_lines();
            old_start < display_old_end.max(display_old_start + 1)
                && old_end.max(old_start + 1) > display_old_start
        });

        if let Some(hunk_idx) = found_hunk {
            let file_name = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| "Invalid file name".to_string())?;

            let (hdr, num_lines) = patch
                .hunk(hunk_idx)
                .map_err(|e| format!("Failed to read hunk: {e}"))?;

            let mut patch_str = format!(
                "diff --git a/{file_name} b/{file_name}\n\
                 --- a/{file_name}\n\
                 +++ b/{file_name}\n\
                 @@ -{},{} +{},{} @@\n",
                hdr.old_start(),
                hdr.old_lines(),
                hdr.new_start(),
                hdr.new_lines(),
            );

            for line_idx in 0..num_lines {
                let line = patch
                    .line_in_hunk(hunk_idx, line_idx)
                    .map_err(|e| format!("Failed to read line: {e}"))?;
                let content = String::from_utf8_lossy(line.content());
                let prefix = match line.origin() {
                    '+' => '+',
                    '-' => '-',
                    _ => ' ',
                };
                patch_str.push(prefix);
                patch_str.push_str(&content);
                if !content.ends_with('\n') {
                    patch_str.push('\n');
                }
            }

            super::hunk_patch::apply_patch(
                &patch_str,
                &*repo,
                true,
                crate::git::provider::ApplyLocation::Index,
            )?;
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

        tracing::info!("Unstaged hunk at row {} in {:?}", cursor_row, file_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn unstages_hunk_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("test.txt", "line 1\nline 2\nline 3\n")
            .with_staged_change("test.txt", "line 1\nline 2\nline 3\nnew line\n")
            .load_and_diff("test.txt");
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.update(|s, cx| s.git_unstage_hunk(cx).unwrap());

        let diffs = stoat.fake_git().applied_diffs();
        assert!(!diffs.is_empty(), "Should have applied an unstage diff");
    }

    #[gpui::test]
    #[should_panic(expected = "No file path set for current buffer")]
    fn fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.update(|s, cx| s.git_unstage_hunk(cx).unwrap());
    }

    #[gpui::test]
    #[should_panic(expected = "No diff information available")]
    fn fails_without_diff(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.set_file_path(PathBuf::from("/fake/repo/test.txt"));
        stoat.update(|s, cx| s.git_unstage_hunk(cx).unwrap());
    }

    #[gpui::test]
    #[should_panic(expected = "No hunk at cursor row")]
    fn fails_when_not_on_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("test.txt", "line 1\n")
            .with_working_change("test.txt", "line 1\n")
            .load_and_diff("test.txt");

        stoat.update(|s, cx| s.git_unstage_hunk(cx).unwrap());
    }
}
