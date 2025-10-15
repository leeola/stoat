//! Open diff review modal action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Open diff review mode.
    ///
    /// Scans the repository for all modified files and enters diff_review mode for hunk-by-hunk
    /// review. Supports resuming previous review sessions if state exists. Computes diffs
    /// on-demand for each file using the current [`crate::git_diff::DiffComparisonMode`].
    /// Following Zed's ProjectDiff pattern but simplified for stoat's modal architecture.
    ///
    /// # Workflow
    ///
    /// ## Restoring Previous Session
    /// 1. Checks if [`Stoat::diff_review_files`] is non-empty
    /// 2. Loads the saved file at [`Stoat::diff_review_current_file_idx`]
    /// 3. Computes diff via [`Stoat::compute_diff_for_review_mode`]
    /// 4. Jumps to saved hunk index via [`Stoat::jump_to_current_hunk`]
    ///
    /// ## Starting Fresh Session
    /// 1. Discovers repository from worktree root
    /// 2. Gathers git status entries
    /// 3. Deduplicates and stores file paths
    /// 4. Finds first file with hunks (loads on-demand)
    /// 5. Initializes review state (file index, hunk index, approved hunks)
    /// 6. Enters [`crate::stoat::KeyContext::DiffReview`] with diff_review mode
    /// 7. Jumps to first hunk
    ///
    /// # Behavior
    ///
    /// - Returns early if no git repository found
    /// - Returns early if no modified files
    /// - Returns early if no files have hunks in current comparison mode
    /// - Respects current [`crate::git_diff::DiffComparisonMode`]
    /// - Preserves review progress across sessions
    ///
    /// # Related
    ///
    /// - [`Stoat::diff_review_next_hunk`] - navigate to next hunk
    /// - [`Stoat::diff_review_prev_hunk`] - navigate to previous hunk
    /// - [`Stoat::diff_review_approve_hunk`] - mark hunk as reviewed
    /// - [`Stoat::diff_review_dismiss`] - exit review mode
    /// - [`Stoat::diff_review_reset_progress`] - clear all progress
    /// - [`Stoat::compute_diff_for_review_mode`] - centralized diff computation
    pub fn open_diff_review(&mut self, cx: &mut Context<Self>) {
        tracing::info!("Opening diff review");
        debug!("Opening diff review");

        // Save current mode to restore later
        self.diff_review_previous_mode = Some(self.mode.clone());

        // Use worktree root to discover repository
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match crate::git_repository::Repository::discover(&root_path).ok() {
            Some(repo) => repo,
            None => {
                debug!("No git repository found");
                return;
            },
        };

        // Check if we have existing review state to restore
        if !self.diff_review_files.is_empty() {
            // Restore previous review session
            debug!(
                "Restoring review session at file {}, hunk {}",
                self.diff_review_current_file_idx, self.diff_review_current_hunk_idx
            );

            // Load the saved file
            if let Some(saved_file_path) = self
                .diff_review_files
                .get(self.diff_review_current_file_idx)
            {
                let abs_path = repo.workdir().join(saved_file_path);

                if let Err(e) = self.load_file(&abs_path, cx) {
                    tracing::error!("Failed to load saved file {:?}: {}", abs_path, e);
                    return;
                }

                // For IndexVsHead mode, update buffer with index content so anchors resolve
                // correctly
                if self.diff_comparison_mode()
                    == crate::diff_review::DiffComparisonMode::IndexVsHead
                {
                    if let Ok(index_content) = repo.index_content(&abs_path) {
                        let buffer_item = self.active_buffer(cx);
                        buffer_item.update(cx, |item, cx| {
                            item.buffer().update(cx, |buffer, _| {
                                let len = buffer.len();
                                buffer.edit([(0..len, index_content.as_str())]);
                            });
                        });
                    }
                }

                // Compute diff respecting the comparison mode
                if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                    // Update the buffer item's diff for display
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff));
                    });
                }
            }

            // Enter diff_review mode
            self.key_context = crate::stoat::KeyContext::DiffReview;
            self.mode = "diff_review".to_string();

            // Jump to saved hunk
            self.jump_to_current_hunk(cx);

            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
            return;
        }

        // No existing state - start fresh review session
        // Scan git status to get list of modified files
        let entries = match crate::git_status::gather_git_status(repo.inner()) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!("Failed to gather git status: {}", e);
                return;
            },
        };

        if entries.is_empty() {
            debug!("No modified files to review");
            return;
        }

        // Deduplicate files and store paths
        let mut seen = std::collections::HashSet::new();
        let file_paths: Vec<std::path::PathBuf> = entries
            .into_iter()
            .filter(|e| seen.insert(e.path.clone()))
            .map(|e| e.path)
            .collect();

        if file_paths.is_empty() {
            debug!("No unique files to review");
            return;
        }

        // Store file list
        self.diff_review_files = file_paths.clone();

        // Find first file with hunks by loading and checking on-demand
        let mut first_file_idx = None;
        for (idx, file_path) in file_paths.iter().enumerate() {
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // For IndexVsHead mode, update buffer with index content so anchors resolve correctly
            if self.diff_comparison_mode() == crate::diff_review::DiffComparisonMode::IndexVsHead {
                if let Ok(index_content) = repo.index_content(&abs_path) {
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, cx| {
                        item.buffer().update(cx, |buffer, _| {
                            let len = buffer.len();
                            buffer.edit([(0..len, index_content.as_str())]);
                        });
                    });
                }
            }

            // Compute diff
            if let Some(diff) = self.compute_diff_for_review_mode(&abs_path, cx) {
                if !diff.hunks.is_empty() {
                    // Found first file with hunks
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff.clone()));
                    });

                    first_file_idx = Some(idx);
                    tracing::info!(
                        "Diff review: found first file with {} hunks in {} mode",
                        diff.hunks.len(),
                        self.diff_review_comparison_mode.display_name()
                    );
                    break;
                }
            }
        }

        let first_idx = match first_file_idx {
            Some(idx) => idx,
            None => {
                debug!("No files with hunks in current comparison mode");
                self.diff_review_files.clear();
                return;
            },
        };

        // Initialize state to start at first file with hunks
        self.diff_review_current_file_idx = first_idx;
        self.diff_review_current_hunk_idx = 0;
        self.diff_review_approved_hunks.clear();

        // Enter diff_review mode
        self.key_context = crate::stoat::KeyContext::DiffReview;
        self.mode = "diff_review".to_string();

        // Jump to first hunk
        self.jump_to_current_hunk(cx);

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opens_diff_review_with_correct_state(cx: &mut TestAppContext) {
        use std::process::Command;
        use text::ToPoint;

        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap();

        // Create initial committed state
        let file1 = repo_path.join("file1.txt");
        let file2 = repo_path.join("file2.txt");

        std::fs::write(&file1, "line 1\nline 2\nline 3\n").unwrap();
        std::fs::write(&file2, "foo\nbar\nbaz\n").unwrap();

        // Git add and commit
        Command::new("git")
            .args(&["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(&["commit", "-m", "Initial"])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Modify both files to create hunks
        std::fs::write(&file1, "line 1\nMODIFIED\nline 3\n").unwrap(); // 1 hunk at line 1
        std::fs::write(&file2, "foo\nbar\nADDED\n").unwrap(); // 1 hunk at line 2

        // Open diff review
        stoat.update(|s, cx| {
            s.open_diff_review(cx);

            // Verify mode and context
            assert_eq!(s.mode(), "diff_review");
            assert_eq!(s.key_context, crate::stoat::KeyContext::DiffReview);

            // Verify file list contains both files
            assert_eq!(s.diff_review_files.len(), 2);
            let file_names: Vec<String> = s
                .diff_review_files
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
                .collect();
            assert!(file_names.contains(&"file1.txt".to_string()));
            assert!(file_names.contains(&"file2.txt".to_string()));

            // Verify current position (first file, first hunk)
            assert_eq!(s.diff_review_current_file_idx, 0);
            assert_eq!(s.diff_review_current_hunk_idx, 0);

            // Verify active buffer loaded correctly
            let buffer_item = s.active_buffer(cx);
            let diff = buffer_item
                .read(cx)
                .diff()
                .expect("Diff should be loaded for first file");

            // Verify hunk count for first file
            assert_eq!(diff.hunks.len(), 1, "First file should have 1 hunk");

            // Verify cursor position at hunk start
            let hunk = &diff.hunks[0];
            let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
            let hunk_start_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
            assert_eq!(
                s.cursor_position().row,
                hunk_start_row,
                "Cursor should be at start of first hunk (row {})",
                hunk_start_row
            );

            // Verify no approvals yet
            assert!(
                s.diff_review_approved_hunks.is_empty(),
                "No hunks should be approved initially"
            );
        });
    }

    #[gpui::test]
    fn switches_to_staged_mode_and_navigates(cx: &mut TestAppContext) {
        use std::process::Command;
        use text::ToPoint;

        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap();

        // Create initial committed state
        let file1 = repo_path.join("file1.txt");
        std::fs::write(&file1, "line 1\nline 2\nline 3\n").unwrap();

        Command::new("git")
            .args(&["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(&["commit", "-m", "Initial"])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Modify file and stage changes (no unstaged changes - working tree = index)
        std::fs::write(&file1, "line 1\nMODIFIED\nline 3\n").unwrap();
        Command::new("git")
            .args(&["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();

        stoat.update(|s, cx| {
            // Open diff review in default WorkingVsHead mode
            s.open_diff_review(cx);

            assert_eq!(s.mode(), "diff_review");
            assert_eq!(s.diff_review_files.len(), 1);

            // Verify initial state in WorkingVsHead mode
            let buffer_item = s.active_buffer(cx);
            let diff_before = buffer_item.read(cx).diff().expect("Diff should be loaded");
            let hunk_count_before = diff_before.hunks.len();
            assert_eq!(hunk_count_before, 1, "Should have 1 hunk in WorkingVsHead");

            let cursor_before = s.cursor_position();
            let hunk_start_before = {
                let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
                diff_before.hunks[0]
                    .buffer_range
                    .start
                    .to_point(&buffer_snapshot)
            };
            assert_eq!(
                cursor_before.row, hunk_start_before.row,
                "Cursor should be at hunk start in WorkingVsHead mode"
            );

            // Switch to IndexVsHead mode (staged changes only)
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.diff_comparison_mode(),
                crate::diff_review::DiffComparisonMode::WorkingVsIndex
            );

            // Cycle again to IndexVsHead
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.diff_comparison_mode(),
                crate::diff_review::DiffComparisonMode::IndexVsHead
            );

            // Verify diff is still present after switching to IndexVsHead
            let buffer_item_after = s.active_buffer(cx);
            let diff_after = buffer_item_after
                .read(cx)
                .diff()
                .expect("Diff should be loaded after mode switch");

            assert_eq!(
                diff_after.hunks.len(),
                1,
                "Should still have 1 hunk in IndexVsHead mode (staged changes)"
            );

            // Verify cursor is at hunk start
            let cursor_after_switch = s.cursor_position();
            let hunk_start_after = {
                let buffer_snapshot = buffer_item_after.read(cx).buffer().read(cx).snapshot();
                diff_after.hunks[0]
                    .buffer_range
                    .start
                    .to_point(&buffer_snapshot)
            };
            assert_eq!(
                cursor_after_switch.row, hunk_start_after.row,
                "Cursor should be at hunk start after switching to IndexVsHead"
            );

            // Now press next hunk - this should work without issues
            s.diff_review_next_hunk(cx);

            // Verify we're still in diff review mode
            assert_eq!(s.mode(), "diff_review");

            // Verify cursor is still valid (either at same hunk or next file's first hunk)
            let _cursor_after_next = s.cursor_position();

            // Verify diff is still loaded
            let buffer_after_next = s.active_buffer(cx);
            let diff_final = buffer_after_next.read(cx).diff();
            assert!(
                diff_final.is_some(),
                "Diff should still be loaded after pressing next"
            );
        });
    }
}
