//! Open diff review modal action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Open diff review mode.
    ///
    /// Scans the repository for all modified files and enters diff_review mode for hunk-by-hunk
    /// review. Supports resuming previous review sessions if state exists. Computes diffs
    /// on-demand for each file using the current [`crate::git::diff::DiffComparisonMode`].
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
    /// - Respects current [`crate::git::diff::DiffComparisonMode`]
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
        let repo = match self.services.git.discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => {
                debug!("No git repository found");
                return;
            },
        };

        // Check if we have existing review state to restore
        if !self.review_state.files.is_empty() {
            // Restore previous review session
            debug!(
                "Restoring review session at file {}, hunk {}",
                self.review_state.file_idx, self.review_state.hunk_idx
            );

            // Load the saved file
            if let Some(saved_file_path) = self.review_state.files.get(self.review_state.file_idx) {
                let abs_path = repo.workdir().join(saved_file_path);

                if let Err(e) = self.load_file(&abs_path, cx) {
                    tracing::error!("Failed to load saved file {:?}: {}", abs_path, e);
                    return;
                }

                // For IndexVsHead/HeadVsParent, replace buffer content so anchors resolve correctly
                match self.review_comparison_mode() {
                    crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                        match repo.index_content(&abs_path) {
                            Ok(content) => {
                                let buffer_item = self.active_buffer(cx);
                                self.replace_buffer_content(&content, &buffer_item, cx);
                            },
                            Err(e) => {
                                tracing::warn!("Failed to read index content for {abs_path:?}: {e}")
                            },
                        }
                    },
                    crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                        match repo.head_content(&abs_path) {
                            Ok(content) => {
                                let buffer_item = self.active_buffer(cx);
                                self.replace_buffer_content(&content, &buffer_item, cx);
                            },
                            Err(e) => {
                                tracing::warn!("Failed to read head content for {abs_path:?}: {e}")
                            },
                        }
                    },
                    _ => {},
                }

                // Compute diff respecting the comparison mode
                if let Some((diff, staged_rows, staged_hunk_indices)) =
                    self.compute_diff_for_review_mode(&abs_path, cx)
                {
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff));
                        item.set_staged_rows(staged_rows);
                        item.set_staged_hunk_indices(staged_hunk_indices);
                    });
                }
            }

            // Enter diff_review mode
            self.key_context = crate::stoat::KeyContext::DiffReview;
            self.mode = "diff_review".to_string();

            // Jump to saved hunk
            self.jump_to_current_hunk(false, cx);

            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
            return;
        }

        // No existing state - start fresh review session
        // Scan git status to get list of modified files
        let entries = match repo.gather_status() {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!("Failed to gather git status: {}", e);
                return;
            },
        };

        if entries.is_empty() {
            debug!("No modified files to review, entering empty diff review mode");
            self.review_state.files = Vec::new();
            self.review_state.file_idx = 0;
            self.review_state.hunk_idx = 0;
            self.review_state.approved_hunks.clear();
            self.key_context = crate::stoat::KeyContext::DiffReview;
            self.mode = "diff_review".to_string();
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
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
        self.review_state.files = file_paths.clone();

        // Find first file with hunks by loading and checking on-demand
        let mut first_file_idx = None;
        for (idx, file_path) in file_paths.iter().enumerate() {
            let abs_path = repo.workdir().join(file_path);

            // Load file
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load file {:?}: {}", abs_path, e);
                continue;
            }

            // For IndexVsHead/HeadVsParent, replace buffer content so anchors resolve correctly
            match self.review_comparison_mode() {
                crate::git::diff_review::DiffComparisonMode::IndexVsHead => {
                    match repo.index_content(&abs_path) {
                        Ok(content) => {
                            let buffer_item = self.active_buffer(cx);
                            self.replace_buffer_content(&content, &buffer_item, cx);
                        },
                        Err(e) => {
                            tracing::warn!("Failed to read index content for {abs_path:?}: {e}")
                        },
                    }
                },
                crate::git::diff_review::DiffComparisonMode::HeadVsParent => {
                    match repo.head_content(&abs_path) {
                        Ok(content) => {
                            let buffer_item = self.active_buffer(cx);
                            self.replace_buffer_content(&content, &buffer_item, cx);
                        },
                        Err(e) => {
                            tracing::warn!("Failed to read head content for {abs_path:?}: {e}")
                        },
                    }
                },
                _ => {},
            }

            // Compute diff
            if let Some((diff, staged_rows, staged_hunk_indices)) =
                self.compute_diff_for_review_mode(&abs_path, cx)
            {
                if !diff.hunks.is_empty() {
                    // Found first file with hunks
                    let buffer_item = self.active_buffer(cx);
                    buffer_item.update(cx, |item, _| {
                        item.set_diff(Some(diff.clone()));
                        item.set_staged_rows(staged_rows);
                        item.set_staged_hunk_indices(staged_hunk_indices);
                    });

                    first_file_idx = Some(idx);
                    tracing::info!(
                        "Diff review: found first file with {} hunks in {} mode",
                        diff.hunks.len(),
                        self.review_comparison_mode().display_name()
                    );
                    break;
                }
            }
        }

        let first_idx = match first_file_idx {
            Some(idx) => idx,
            None => {
                debug!("No files with hunks in current comparison mode, entering empty diff review mode");
                self.review_state.file_idx = 0;
                self.review_state.hunk_idx = 0;
                self.review_state.approved_hunks.clear();
                self.key_context = crate::stoat::KeyContext::DiffReview;
                self.mode = "diff_review".to_string();
                cx.emit(crate::stoat::StoatEvent::Changed);
                cx.notify();
                return;
            },
        };

        // Initialize state to start at first file with hunks
        self.review_state.file_idx = first_idx;
        self.review_state.hunk_idx = 0;
        self.review_state.approved_hunks.clear();

        // Enter diff_review mode
        self.key_context = crate::stoat::KeyContext::DiffReview;
        self.mode = "diff_review".to_string();

        // Jump to first hunk
        self.jump_to_current_hunk(false, cx);

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

use crate::pane_group::view::PaneGroupView;

impl PaneGroupView {
    pub(crate) fn handle_open_diff_review(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_diff_review(cx);
                });
            });
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::status::GitStatusEntry;
    use gpui::TestAppContext;
    use std::path::PathBuf;
    use text::ToPoint;

    /// Helper: Assert that cursor is at the current hunk's start position.
    ///
    /// Verifies the cursor landed on the hunk row, not at a stale position
    /// like (0,0).
    fn assert_cursor_at_hunk(stoat: &Stoat, cx: &gpui::App) {
        let buffer_item = stoat.active_buffer(cx);
        let diff = buffer_item.read(cx).diff().expect("Diff should be loaded");

        if stoat.review_state.hunk_idx >= diff.hunks.len() {
            panic!(
                "Hunk index {} out of range (only {} hunks)",
                stoat.review_state.hunk_idx,
                diff.hunks.len()
            );
        }

        let hunk = &diff.hunks[stoat.review_state.hunk_idx];
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
        let hunk_start = hunk.buffer_range.start.to_point(&buffer_snapshot);

        let cursor = stoat.cursor_position();
        assert_eq!(
            cursor.row, hunk_start.row,
            "Cursor should be at hunk {} start (row {}), but is at row {}",
            stoat.review_state.hunk_idx, hunk_start.row, cursor.row
        );
    }

    #[gpui::test]
    fn opens_diff_review_with_correct_state(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\n")
            .with_committed_file("file2.txt", "foo\nbar\nbaz\n")
            .with_working_change("file1.txt", "line 1\nMODIFIED\nline 3\n")
            .with_working_change("file2.txt", "foo\nbar\nADDED\n");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_status(vec![
                GitStatusEntry::new(PathBuf::from("file1.txt"), "M".into(), false),
                GitStatusEntry::new(PathBuf::from("file2.txt"), "M".into(), false),
            ]);
        });

        stoat.update(|s, cx| {
            s.open_diff_review(cx);

            assert_eq!(s.mode(), "diff_review");
            assert_eq!(s.key_context, crate::stoat::KeyContext::DiffReview);

            assert_eq!(s.review_state.files.len(), 2);
            let file_names: Vec<String> = s
                .review_state
                .files
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
                .collect();
            assert!(file_names.contains(&"file1.txt".to_string()));
            assert!(file_names.contains(&"file2.txt".to_string()));

            assert_eq!(s.review_state.file_idx, 0);
            assert_eq!(s.review_state.hunk_idx, 0);

            let buffer_item = s.active_buffer(cx);
            let diff = buffer_item
                .read(cx)
                .diff()
                .expect("Diff should be loaded for first file");

            assert_eq!(diff.hunks.len(), 1, "First file should have 1 hunk");

            let hunk = &diff.hunks[0];
            let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
            let hunk_start_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
            assert_eq!(
                s.cursor_position().row,
                hunk_start_row,
                "Cursor should be at start of first hunk (row {hunk_start_row})"
            );

            assert!(
                s.review_state.approved_hunks.is_empty(),
                "No hunks should be approved initially"
            );
        });
    }

    #[gpui::test]
    fn switches_to_staged_mode_and_navigates(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\n")
            .with_staged_change("file1.txt", "line 1\nMODIFIED\nline 3\n");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("file1.txt"),
                "M".into(),
                true,
            )]);
        });

        stoat.update(|s, cx| {
            s.open_diff_review(cx);

            assert_eq!(s.mode(), "diff_review");
            assert_eq!(s.review_state.files.len(), 1);

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

            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsIndex
            );

            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );

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

            s.diff_review_next_hunk(cx);

            assert_eq!(s.mode(), "diff_review");

            let _cursor_after_next = s.cursor_position();

            let buffer_after_next = s.active_buffer(cx);
            let diff_final = buffer_after_next.read(cx).diff();
            assert!(
                diff_final.is_some(),
                "Diff should still be loaded after pressing next"
            );
        });
    }

    #[gpui::test]
    fn indexed_mode_wraparound_cursor_position(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\n")
            .with_staged_change("file1.txt", "line 1\nMODIFIED\nline 3\n");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("file1.txt"),
                "M".into(),
                true,
            )]);
        });

        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");
            assert_eq!(s.review_state.files.len(), 1);

            s.diff_review_cycle_comparison_mode(cx);
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );

            assert_cursor_at_hunk(s, cx);

            s.diff_review_next_hunk(cx);

            assert_cursor_at_hunk(s, cx);

            assert_eq!(s.review_state.file_idx, 0);
            assert_eq!(s.review_state.hunk_idx, 0);
        });
    }

    #[gpui::test]
    fn indexed_mode_with_working_tree_changes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\n")
            .with_staged_change("file1.txt", "line 1\nSTAGED\nline 3\n")
            .with_working_change("file1.txt", "line 1\nWORKING\nline 3\n");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("file1.txt"),
                "M".into(),
                false,
            )]);
        });

        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");

            let buffer_item = s.active_buffer(cx);
            let buffer_text = buffer_item.read(cx).buffer().read(cx).text();
            assert!(
                buffer_text.contains("WORKING"),
                "Buffer should contain working tree content in WorkingVsHead mode"
            );

            s.diff_review_cycle_comparison_mode(cx);
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );

            let buffer_item_after = s.active_buffer(cx);
            let buffer_text_after = buffer_item_after.read(cx).buffer().read(cx).text();
            assert!(
                buffer_text_after.contains("STAGED"),
                "Buffer should contain index content (STAGED) in IndexVsHead mode, but got: {buffer_text_after}"
            );
            assert!(
                !buffer_text_after.contains("WORKING"),
                "Buffer should NOT contain working tree content (WORKING) in IndexVsHead mode"
            );

            assert_cursor_at_hunk(s, cx);

            let diff = buffer_item_after
                .read(cx)
                .diff()
                .expect("Diff should be loaded");
            assert_eq!(diff.hunks.len(), 1, "Should have 1 hunk for staged change");
        });
    }

    #[gpui::test]
    fn indexed_mode_next_with_different_working_tree(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\n")
            .with_staged_change("file1.txt", "line 1\nSTAGED\nline 3\n")
            .with_working_change("file1.txt", "line 1\nWORKING\nline 3\n");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("file1.txt"),
                "M".into(),
                false,
            )]);
        });

        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");

            s.diff_review_cycle_comparison_mode(cx);
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );

            assert_cursor_at_hunk(s, cx);
            let buffer_item_before = s.active_buffer(cx);
            let buffer_text_before = buffer_item_before.read(cx).buffer().read(cx).text();
            assert!(
                buffer_text_before.contains("STAGED"),
                "Buffer should contain STAGED before navigation"
            );

            s.diff_review_next_hunk(cx);

            let buffer_item_after = s.active_buffer(cx);
            let buffer_text_after = buffer_item_after.read(cx).buffer().read(cx).text();
            assert!(
                buffer_text_after.contains("STAGED"),
                "Buffer should contain index content (STAGED) after next/wraparound, but got: {buffer_text_after}"
            );
            assert!(
                !buffer_text_after.contains("WORKING"),
                "Buffer should NOT contain working tree content (WORKING) after wraparound"
            );

            assert_cursor_at_hunk(s, cx);

            let diff_after = buffer_item_after
                .read(cx)
                .diff()
                .expect("Diff should be loaded");
            assert_eq!(
                diff_after.hunks.len(),
                1,
                "Should still have 1 hunk after wraparound"
            );
        });
    }

    #[gpui::test]
    fn unstaged_mode_with_no_unstaged_changes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\n")
            .with_staged_change("file1.txt", "line 1\nMODIFIED\nline 3\n");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("file1.txt"),
                "M".into(),
                true,
            )]);
        });

        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");

            let buffer_item_before = s.active_buffer(cx);
            let diff_before = buffer_item_before.read(cx).diff();
            assert!(diff_before.is_some(), "Should have diff in WorkingVsHead mode");
            assert_eq!(
                diff_before.unwrap().hunks.len(),
                1,
                "Should have 1 hunk in WorkingVsHead"
            );

            assert_cursor_at_hunk(s, cx);
            let cursor_before_switch = s.cursor_position();
            assert!(
                cursor_before_switch.row >= 1,
                "Cursor should be at hunk (row >= 1) before switching modes"
            );

            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsIndex
            );

            let buffer_item_after = s.active_buffer(cx);
            let diff_after = buffer_item_after.read(cx).diff();

            assert!(
                diff_after.is_none() || diff_after.unwrap().hunks.is_empty(),
                "Diff should be cleared in WorkingVsIndex when no unstaged changes, but has {} hunks",
                diff_after.map(|d| d.hunks.len()).unwrap_or(0)
            );

            let cursor_after_switch = s.cursor_position();
            assert_eq!(
                cursor_after_switch.row, 0,
                "BUG C: Cursor should be reset to row 0 when switching to mode with no hunks, but is at row {}",
                cursor_after_switch.row
            );
            assert_eq!(
                cursor_after_switch.column, 0,
                "BUG C: Cursor should be reset to column 0 when switching to mode with no hunks, but is at column {}",
                cursor_after_switch.column
            );
        });
    }

    #[gpui::test]
    fn unstaged_mode_next_with_no_hunks(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\n")
            .with_staged_change("file1.txt", "line 1\nMODIFIED\nline 3\n");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("file1.txt"),
                "M".into(),
                true,
            )]);
        });

        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");

            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsIndex
            );

            s.diff_review_next_hunk(cx);

            let buffer_item_after_next = s.active_buffer(cx);
            let diff_after_next = buffer_item_after_next.read(cx).diff();

            assert!(
                diff_after_next.is_none() || diff_after_next.unwrap().hunks.is_empty(),
                "Diff should be cleared after next in WorkingVsIndex when no unstaged changes, but has {} hunks",
                diff_after_next.map(|d| d.hunks.len()).unwrap_or(0)
            );

            let cursor_after_next = s.cursor_position();
            assert_eq!(
                cursor_after_next.row, 0,
                "BUG B: Cursor should be reset to row 0 after pressing next in mode with no hunks, but is at row {}",
                cursor_after_next.row
            );
            assert_eq!(
                cursor_after_next.column, 0,
                "BUG B: Cursor should be reset to column 0 after pressing next in mode with no hunks, but is at column {}",
                cursor_after_next.column
            );
        });
    }

    #[gpui::test]
    fn indexed_mode_has_broken_syntax_highlighting(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file(
                "example.rs",
                "fn main() {\n    println!(\"hello\");\n    let x = 42;\n}\n",
            )
            .with_staged_change(
                "example.rs",
                "fn main() {\n    println!(\"STAGED\");\n    let x = 42;\n}\n",
            )
            .with_working_change(
                "example.rs",
                "fn main() {\n    println!(\"WORKING\");\n    let x = 42;\n}\n",
            );

        stoat.update(|s, _cx| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("example.rs"),
                "M".into(),
                false,
            )]);
        });

        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");

            let buffer_item_before = s.active_buffer(cx);
            let source_before = buffer_item_before.read(cx).buffer().read(cx).text();
            let captures_before = buffer_item_before
                .read(cx)
                .highlight_captures(0..source_before.len(), &source_before);

            assert!(
                !captures_before.is_empty(),
                "Should have highlight captures in WorkingVsHead mode"
            );

            s.diff_review_cycle_comparison_mode(cx);
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );

            let buffer_item_after = s.active_buffer(cx);

            let buffer_text = buffer_item_after.read(cx).buffer().read(cx).text();
            assert!(
                buffer_text.contains("STAGED"),
                "Buffer should contain index content (STAGED)"
            );

            let captures_after = buffer_item_after
                .read(cx)
                .highlight_captures(0..buffer_text.len(), &buffer_text);
            assert!(
                !captures_after.is_empty(),
                "Should have highlight captures after mode switch"
            );
        });
    }
}
