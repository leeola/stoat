//! Git stage hunk implementation and tests.
//!
//! This module implements the [`git_stage_hunk`](crate::Stoat::git_stage_hunk) action, which
//! stages individual diff hunks via libgit2's index apply. The action is part
//! of the git staging workflow alongside [`git_stage_file`](crate::Stoat::git_stage_file) for
//! staging entire files and [`git_stage_all`](crate::Stoat::git_stage_all) for staging all
//! changes.

use crate::{git::diff::BufferDiff, stoat::Stoat};
use gpui::Context;
use text::ToPoint;

impl Stoat {
    /// Toggle the staging state of the current hunk.
    ///
    /// Uses [`staged_hunk_indices`](crate::buffer::BufferItem::staged_hunk_indices)
    /// to detect staged state, which works for all hunk types including pure
    /// deletions. Delegates to [`git_stage_hunk`](Self::git_stage_hunk) or
    /// [`git_unstage_hunk`](Self::git_unstage_hunk).
    pub fn git_toggle_stage_hunk(&mut self, cx: &mut Context<Self>) {
        if self.review_state.source.is_commit() {
            return;
        }

        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let cursor_row = self.cursor.position().row;
        let diff = match buffer_item.read(cx).diff() {
            Some(d) => d,
            None => {
                tracing::error!("git_toggle_stage_hunk: No diff information available");
                return;
            },
        };

        let hunk_index = match diff.hunk_for_row(cursor_row, buffer_snapshot) {
            Some(idx) => idx,
            None => {
                tracing::error!("git_toggle_stage_hunk: No hunk at cursor row {cursor_row}");
                return;
            },
        };

        let is_staged = buffer_item
            .read(cx)
            .staged_hunk_indices()
            .is_some_and(|indices| indices.contains(&hunk_index));

        if is_staged {
            self.git_unstage_hunk(cx);
        } else {
            self.git_stage_hunk(cx);
        }
    }

    /// Stage the current hunk for commit.
    ///
    /// Uses the display diff to locate the hunk at the cursor, then spawns an
    /// async task that computes a working-vs-index diff to generate a patch
    /// against the correct base and applies it. If the hunk is already staged
    /// (no working-vs-index difference in that region), the operation is a no-op
    /// and the display is refreshed.
    ///
    /// # Related Actions
    ///
    /// - [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) - Unstage this hunk
    /// - [`git_stage_file`](crate::Stoat::git_stage_file) - Stage the entire file
    /// - [`git_stage_all`](crate::Stoat::git_stage_all) - Stage all changes
    pub fn git_stage_hunk(&mut self, cx: &mut Context<Self>) {
        if self.review_state.source.is_commit() {
            return;
        }

        let file_path = match self.current_file_path.as_ref() {
            Some(p) => p.clone(),
            None => {
                tracing::error!("git_stage_hunk: No file path set for current buffer");
                return;
            },
        };

        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let (display_start, display_end) = {
            let diff = match buffer_item.read(cx).diff() {
                Some(d) => d,
                None => {
                    tracing::error!("git_stage_hunk: No diff information available");
                    return;
                },
            };
            let hunk_index = match diff.hunk_for_row(cursor_row, buffer_snapshot) {
                Some(idx) => idx,
                None => {
                    tracing::error!("git_stage_hunk: No hunk at cursor row {cursor_row}");
                    return;
                },
            };
            let hunk = &diff.hunks[hunk_index];
            let start = hunk.buffer_range.start.to_point(buffer_snapshot).row;
            let end = hunk.buffer_range.end.to_point(buffer_snapshot).row;
            (start, end)
        };

        let buffer_id = buffer_snapshot.remote_id();
        let buffer_snapshot_clone = buffer_snapshot.clone();
        let services = self.services.clone();

        cx.spawn(async move |this, cx| {
            let result = async {
                let repo = services
                    .git
                    .discover(&file_path)
                    .await
                    .map_err(|e| format!("Repository not found: {e}"))?;
                let index_content = repo.index_content(&file_path).await.unwrap_or_default();
                let stage_diff = BufferDiff::new(buffer_id, index_content, &buffer_snapshot_clone)
                    .map_err(|e| format!("Working-vs-index diff failed: {e}"))?;

                let stage_hunk = stage_diff.hunks.iter().find(|h| {
                    let start = h.buffer_range.start.to_point(&buffer_snapshot_clone).row;
                    let end = h.buffer_range.end.to_point(&buffer_snapshot_clone).row;
                    start <= display_end && end >= display_start
                });

                if let Some(hunk) = stage_hunk {
                    let patch = super::hunk_patch::generate_hunk_patch(
                        &stage_diff,
                        hunk,
                        &buffer_snapshot_clone,
                        &file_path,
                    )?;
                    super::hunk_patch::apply_patch(
                        &patch,
                        &*repo,
                        false,
                        crate::git::provider::ApplyLocation::Index,
                    )
                    .await?;
                }

                tracing::info!("Staged hunk at row {} in {:?}", cursor_row, file_path);
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("git_stage_hunk: {e}");
                    return;
                }
                stoat.refresh_git_diff(cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn stages_hunk_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("test.txt", "line 1\nline 2\nline 3\n")
            .with_working_change("test.txt", "line 1\nline 2\nline 3\nnew line\n")
            .load_and_diff("test.txt");
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.update(|s, cx| s.git_stage_hunk(cx));
        stoat.run_until_parked();

        let diffs = stoat.fake_git().applied_diffs();
        assert!(!diffs.is_empty(), "Should have applied a diff");
        assert!(
            diffs[0].0.contains("new line"),
            "Patch should contain the new line"
        );
    }

    #[gpui::test]
    fn double_stage_is_noop(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("test.txt", "line 1\nline 2\nline 3\n")
            .with_working_change("test.txt", "line 1\nline 2\nline 3\nnew line\n")
            .load_and_diff("test.txt");
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.update(|s, cx| s.git_stage_hunk(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| s.git_stage_hunk(cx));
        stoat.run_until_parked();
    }

    #[gpui::test]
    fn toggle_unstages_deletion_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("test.txt", "line 1\nline 2\nline 3\n")
            .with_working_change("test.txt", "line 1\nline 3\n")
            .load_and_diff("test.txt");
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(0, 0)));

        stoat.update(|s, cx| s.git_stage_hunk(cx));
        stoat.run_until_parked();

        let diffs = stoat.fake_git().applied_diffs();
        assert!(
            diffs[0].0.contains("-line 2"),
            "Deletion should be staged: {}",
            diffs[0].0
        );

        stoat.update(|s, cx| s.git_toggle_stage_hunk(cx));
        stoat.run_until_parked();

        let diffs = stoat.fake_git().applied_diffs();
        assert!(diffs.len() >= 2, "Should have applied unstage diff");
    }

    #[gpui::test]
    fn noop_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.update(|s, cx| s.git_stage_hunk(cx));
        stoat.run_until_parked();
    }

    #[gpui::test]
    fn noop_without_diff(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.set_file_path(PathBuf::from("/fake/repo/test.txt"));
        stoat.update(|s, cx| s.git_stage_hunk(cx));
        stoat.run_until_parked();
    }

    #[gpui::test]
    fn noop_when_not_on_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("test.txt", "line 1\n")
            .with_working_change("test.txt", "line 1\n")
            .load_and_diff("test.txt");

        stoat.update(|s, cx| s.git_stage_hunk(cx));
        stoat.run_until_parked();
    }
}
