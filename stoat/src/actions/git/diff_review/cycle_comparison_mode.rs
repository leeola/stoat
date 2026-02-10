//! Diff review cycle comparison mode action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Cycle through diff comparison modes in diff review.
    ///
    /// Rotates through [`crate::git::diff::DiffComparisonMode`] variants (WorkingVsHead to
    /// WorkingVsIndex to IndexVsHead to WorkingVsHead) via
    /// [`Stoat::cycle_diff_comparison_mode`]. Recomputes the diff for the current file using
    /// [`Stoat::compute_diff_for_review_mode`] to reflect the new comparison. If the current
    /// hunk index is now out of range, resets to hunk 0. Jumps to the current (or first)
    /// hunk after recomputing.
    ///
    /// # Workflow
    ///
    /// 1. Cycles to next mode via [`Stoat::cycle_diff_comparison_mode`]
    /// 2. Gets current file path
    /// 3. Discovers repository and builds absolute path
    /// 4. Recomputes diff for new comparison mode
    /// 5. Updates buffer item's diff
    /// 6. Resets hunk index if out of range
    /// 7. Jumps to current hunk via [`Stoat::jump_to_current_hunk`]
    ///
    /// # Behavior
    ///
    /// - Only operates in diff_review mode
    /// - Returns early if no current file
    /// - Returns early if repository discovery fails
    /// - Resets hunk index to 0 if current index exceeds new hunk count
    /// - Jumps to hunk after mode change to show new diff
    ///
    /// # Related
    ///
    /// - [`Stoat::cycle_diff_comparison_mode`] - cycles the mode setting
    /// - [`Stoat::compute_diff_for_review_mode`] - centralized diff computation
    /// - [`Stoat::jump_to_current_hunk`] - cursor positioning and scrolling
    /// - [`crate::git::diff::DiffComparisonMode`] - comparison mode enum
    pub fn diff_review_cycle_comparison_mode(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        debug!("Cycling diff comparison mode");

        // Cycle to next mode
        self.cycle_diff_comparison_mode();
        let new_mode = self.diff_comparison_mode();
        debug!("New comparison mode: {:?}", new_mode);

        // Get current file path
        let current_file_path = match self
            .diff_review_files
            .get(self.diff_review_current_file_idx)
        {
            Some(path) => path.clone(),
            None => return,
        };

        // Get absolute path
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git::repository::Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => return,
        };
        let abs_path = repo.workdir().join(&current_file_path);

        // For IndexVsHead mode, update buffer with index content so anchors resolve correctly
        if new_mode == crate::git::diff_review::DiffComparisonMode::IndexVsHead {
            if let Ok(index_content) = repo.index_content(&abs_path) {
                let buffer_item = self.active_buffer(cx);
                buffer_item.update(cx, |item, cx| {
                    item.buffer().update(cx, |buffer, _| {
                        let len = buffer.len();
                        buffer.edit([(0..len, index_content.as_str())]);
                    });
                    // Reparse to update syntax highlighting tokens
                    let _ = item.reparse(cx);
                });
            }
        } else {
            // For other modes, reload file to get working tree content
            let _ = self.load_file(&abs_path, cx);
        }

        // Recompute diff for new comparison mode
        if let Some(new_diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
            // Update the buffer item diff
            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, _cx| {
                item.set_diff(Some(new_diff.clone()));
            });

            // Reset hunk index if it's now out of range
            let hunk_count = new_diff.hunks.len();
            if self.diff_review_current_hunk_idx >= hunk_count {
                self.diff_review_current_hunk_idx = 0;
            }

            if hunk_count > 0 {
                self.jump_to_current_hunk(true, cx);
            } else {
                // No hunks in new mode - reset cursor to file start
                let target_pos = text::Point::new(0, 0);
                self.cursor.move_to(target_pos);

                // Sync selections to cursor position
                let buffer_snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: target_pos,
                        end: target_pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &buffer_snapshot,
                );
            }
        } else {
            // No diff for new mode - clear old diff and reset cursor
            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, _| {
                item.set_diff(None);
            });

            let target_pos = text::Point::new(0, 0);
            self.cursor.move_to(target_pos);

            // Sync selections to cursor position
            let buffer_snapshot = self.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = self.selections.next_id();
            self.selections.select(
                vec![text::Selection {
                    id,
                    start: target_pos,
                    end: target_pos,
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &buffer_snapshot,
            );
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn cycles_comparison_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            if s.mode() == "diff_review" && !s.diff_review_files.is_empty() {
                let initial_mode = s.diff_comparison_mode();
                s.diff_review_cycle_comparison_mode(cx);
                // Mode should change
                assert_ne!(s.diff_comparison_mode(), initial_mode);
            }
        });
    }
}
