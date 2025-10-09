//! Git status tracking for modified files.
//!
//! This module provides data structures and functions for gathering git repository status
//! information to display in the git status modal overlay. It uses libgit2 to query file
//! statuses and presents them in a format suitable for quick review.
//!
//! # Architecture
//!
//! The status system has two main components:
//!
//! 1. [`GitStatusEntry`] - Represents a single file with its git status
//! 2. [`gather_git_status`] - Function that queries git and builds the entry list
//!
//! ## How Status Works
//!
//! Status is gathered by discovering the git repository, then calling `statuses()`:
//!
//! ```ignore
//! let repo = Repository::discover(current_path)?;
//! let entries = gather_git_status(&repo)?;
//! ```
//!
//! # Status Types
//!
//! Status entries track both index (staged) and working tree changes:
//! - **Modified** (M) - File has changes
//! - **Added** (A) - New file
//! - **Deleted** (D) - File removed
//! - **Renamed** (R) - File renamed
//! - **Conflicted** (!) - Merge conflict
//! - **Untracked** (??) - Not tracked by git
//!
//! # Usage
//!
//! ```ignore
//! use stoat::git_status::{GitStatusEntry, gather_git_status};
//! use stoat::git_repository::Repository;
//!
//! let repo = Repository::discover(Path::new("."))?;
//! let entries = gather_git_status(repo.inner())?;
//!
//! for entry in &entries {
//!     println!("{} {}", entry.status_display(), entry.path.display());
//! }
//! ```
//!
//! # Related
//!
//! - [`git_repository::Repository`](crate::git_repository::Repository) - Git repository wrapper
//! - [`Stoat`](crate::Stoat) - Uses this for git status modal state

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during git status gathering.
#[derive(Debug, Error)]
pub enum GitStatusError {
    /// Git operation failed
    #[error("Git status error: {0}")]
    GitError(String),
}

/// A git status entry for a single file.
///
/// Represents the status of one file in the working tree and/or index. Used by the
/// git status modal to display modified files for quick review.
///
/// # Status Representation
///
/// Status is simplified to a single character for display:
/// - `M` - Modified (in index or working tree)
/// - `A` - Added (new file in index)
/// - `D` - Deleted (removed from index or working tree)
/// - `R` - Renamed (file renamed in index)
/// - `!` - Conflicted (merge conflict)
/// - `??` - Untracked (not tracked by git)
///
/// # Staging
///
/// The `staged` flag indicates whether changes are in the index (staged for commit).
/// This allows different visual styling in the UI.
#[derive(Clone, Debug)]
pub struct GitStatusEntry {
    /// Path to the file, relative to repository root
    pub path: PathBuf,
    /// Status string for display ("M", "A", "D", "R", "!", "??")
    pub status: String,
    /// Whether changes are staged in index
    pub staged: bool,
}

impl GitStatusEntry {
    /// Create a new git status entry.
    pub fn new(path: PathBuf, status: String, staged: bool) -> Self {
        Self {
            path,
            status,
            staged,
        }
    }

    /// Get display string for status with staging indicator.
    ///
    /// Returns a two-character status like "M " (modified, staged) or
    /// " M" (modified, unstaged).
    pub fn status_display(&self) -> String {
        if self.staged {
            format!("{} ", self.status)
        } else {
            format!(" {}", self.status)
        }
    }
}

/// Gather git status entries from a repository.
///
/// Queries the git repository for file statuses and returns a list of entries
/// for files that have changes. Ignores clean files and sorts results by path.
///
/// # Arguments
///
/// * `repo` - Git repository to query
///
/// # Returns
///
/// Vector of status entries for changed files, sorted by path
///
/// # Status Priorities
///
/// When a file has both index and working tree changes, index status takes priority
/// for the display character. The `staged` flag indicates index changes.
///
/// # Errors
///
/// Returns error if git status query fails.
pub fn gather_git_status(repo: &git2::Repository) -> Result<Vec<GitStatusEntry>, GitStatusError> {
    let mut entries = Vec::new();

    let statuses = repo
        .statuses(None)
        .map_err(|e| GitStatusError::GitError(e.message().to_string()))?;

    for entry in statuses.iter() {
        let status = entry.status();
        let path = entry
            .path()
            .ok_or_else(|| GitStatusError::GitError("Invalid UTF-8 path".to_string()))?;

        // Determine primary status and staging
        let (status_char, staged) = if status.is_index_new() {
            ("A", true)
        } else if status.is_index_modified() {
            ("M", true)
        } else if status.is_index_deleted() {
            ("D", true)
        } else if status.is_index_renamed() {
            ("R", true)
        } else if status.is_wt_new() {
            ("??", false)
        } else if status.is_wt_modified() {
            ("M", false)
        } else if status.is_wt_deleted() {
            ("D", false)
        } else if status.is_wt_renamed() {
            ("R", false)
        } else if status.is_conflicted() {
            ("!", false)
        } else {
            continue; // Skip clean files
        };

        entries.push(GitStatusEntry::new(
            PathBuf::from(path),
            status_char.to_string(),
            staged,
        ));
    }

    // Sort by path for consistent display
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(entries)
}
