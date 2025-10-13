//! Git diff review mode for hunk-by-hunk review.
//!
//! This module implements a modal-based git diff review system that allows navigating
//! through all modified files and their hunks, marking them as reviewed/approved one by one.
//!
//! # Architecture
//!
//! Following Zed's ProjectDiff pattern but simplified for stoat's modal architecture:
//! - Scan repo for all modified files
//! - Load diffs on-demand as files are visited
//! - Track review progress per hunk
//! - Navigate cross-file automatically
//!
//! # Usage
//!
//! ```ignore
//! // Enter review mode
//! stoat.open_diff_review(cx);
//!
//! // Navigate hunks
//! stoat.diff_review_next_hunk(cx); // Moves to next unreviewed hunk
//!
//! // Approve current hunk
//! stoat.diff_review_approve_hunk(cx); // Marks reviewed and moves to next
//!
//! // Exit review mode
//! stoat.diff_review_dismiss(cx);
//! ```
//!
//! # Related
//!
//! - [`git_diff`](crate::git_diff) - Core diff computation
//! - [`git_status`](crate::git_status) - File status tracking
//! - Zed's `ProjectDiff` - Inspiration for multi-file diff navigation

use crate::git_diff::BufferDiff;
use std::path::PathBuf;

/// Information about a file in diff review mode.
///
/// Contains the file path, its git status, and the computed diff hunks.
/// Similar to Zed's ProjectDiff but simplified for on-demand loading.
#[derive(Clone, Debug)]
pub struct DiffReviewFile {
    /// Path to the modified file
    pub path: PathBuf,

    /// Git status string ("M", "A", "D", etc.)
    pub status: String,

    /// Computed diff for this file.
    ///
    /// Contains all hunks for the file. `None` if diff hasn't been computed yet
    /// (loaded on-demand when file is visited).
    pub diff: Option<BufferDiff>,

    /// Total number of hunks in this file.
    ///
    /// Cached from `diff.hunks.len()` for quick access without loading diff.
    pub hunk_count: usize,
}

impl DiffReviewFile {
    /// Create a new diff review file entry.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file
    /// * `status` - Git file status
    ///
    /// # Returns
    ///
    /// A new [`DiffReviewFile`] with unloaded diff
    pub fn new(path: PathBuf, status: String) -> Self {
        Self {
            path,
            status,
            diff: None,
            hunk_count: 0,
        }
    }

    /// Check if this file has an unreviewed hunk at the given index.
    ///
    /// # Arguments
    ///
    /// * `hunk_idx` - Hunk index to check
    /// * `approved_hunks` - Set of approved hunk indices for this file
    ///
    /// # Returns
    ///
    /// `true` if the hunk exists and is not approved
    pub fn has_unreviewed_hunk(
        &self,
        hunk_idx: usize,
        approved_hunks: &std::collections::HashSet<usize>,
    ) -> bool {
        hunk_idx < self.hunk_count && !approved_hunks.contains(&hunk_idx)
    }

    /// Get the next unreviewed hunk index starting from `current_idx`.
    ///
    /// # Arguments
    ///
    /// * `current_idx` - Current hunk index
    /// * `approved_hunks` - Set of approved hunk indices for this file
    ///
    /// # Returns
    ///
    /// Index of next unreviewed hunk, or `None` if all remaining hunks are reviewed
    pub fn next_unreviewed_hunk(
        &self,
        current_idx: usize,
        approved_hunks: &std::collections::HashSet<usize>,
    ) -> Option<usize> {
        (current_idx + 1..self.hunk_count).find(|idx| !approved_hunks.contains(idx))
    }
}
