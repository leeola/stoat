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
    fn opens_diff_review(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            // Mode changes if git repo found with modified files
            if s.mode() == "diff_review" {
                assert_eq!(s.key_context, crate::stoat::KeyContext::DiffReview);
            }
        });
    }
}
