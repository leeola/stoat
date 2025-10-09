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

use std::path::{Path, PathBuf};
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

        let path_buf = PathBuf::from(path);

        // Check for staged changes
        if status.is_index_new() {
            entries.push(GitStatusEntry::new(path_buf.clone(), "A".to_string(), true));
        } else if status.is_index_modified() {
            entries.push(GitStatusEntry::new(path_buf.clone(), "M".to_string(), true));
        } else if status.is_index_deleted() {
            entries.push(GitStatusEntry::new(path_buf.clone(), "D".to_string(), true));
        } else if status.is_index_renamed() {
            entries.push(GitStatusEntry::new(path_buf.clone(), "R".to_string(), true));
        }

        // Check for unstaged changes (can happen in addition to staged changes)
        if status.is_wt_new() {
            entries.push(GitStatusEntry::new(
                path_buf.clone(),
                "??".to_string(),
                false,
            ));
        } else if status.is_wt_modified() {
            entries.push(GitStatusEntry::new(
                path_buf.clone(),
                "M".to_string(),
                false,
            ));
        } else if status.is_wt_deleted() {
            entries.push(GitStatusEntry::new(
                path_buf.clone(),
                "D".to_string(),
                false,
            ));
        } else if status.is_wt_renamed() {
            entries.push(GitStatusEntry::new(
                path_buf.clone(),
                "R".to_string(),
                false,
            ));
        } else if status.is_conflicted() {
            entries.push(GitStatusEntry::new(path_buf, "!".to_string(), false));
        }
    }

    // Sort by path for consistent display
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(entries)
}

/// Git diff preview data for the status modal.
///
/// Contains the diff patch text for a file, showing what has changed.
/// Similar to [`crate::file_finder::PreviewData`] but for git diffs instead of file content.
#[derive(Clone)]
pub struct DiffPreviewData {
    /// The diff patch text in unified diff format
    pub text: String,
}

impl DiffPreviewData {
    /// Create a new diff preview with the given patch text.
    pub fn new(text: String) -> Self {
        Self { text }
    }

    /// Get the diff text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Git branch information for the status modal.
///
/// Contains the current branch name and tracking information (ahead/behind upstream).
/// Used by the git status modal to display branch context alongside file changes.
#[derive(Clone, Debug)]
pub struct GitBranchInfo {
    /// Name of the current branch
    pub branch_name: String,
    /// Number of commits ahead of upstream
    pub ahead: u32,
    /// Number of commits behind upstream
    pub behind: u32,
}

impl GitBranchInfo {
    /// Create new branch info.
    pub fn new(branch_name: String, ahead: u32, behind: u32) -> Self {
        Self {
            branch_name,
            ahead,
            behind,
        }
    }
}

/// Gather git branch information from a repository.
///
/// Queries the current branch name and upstream tracking status (ahead/behind).
/// Returns [`None`] if the repository is in detached HEAD state or if branch
/// information cannot be determined.
///
/// # Arguments
///
/// * `repo` - Git repository to query
///
/// # Returns
///
/// [`Some(GitBranchInfo)`] if on a branch with tracking info, [`None`] otherwise.
pub fn gather_git_branch_info(repo: &git2::Repository) -> Option<GitBranchInfo> {
    let head = repo.head().ok()?;

    if !head.is_branch() {
        return None;
    }

    let branch_name = head.shorthand()?.to_string();

    let (ahead, behind) = if let Some(local_oid) = head.target() {
        let branch = repo
            .find_branch(&branch_name, git2::BranchType::Local)
            .ok()?;

        if let Ok(upstream) = branch.upstream() {
            if let Some(upstream_oid) = upstream.get().target() {
                repo.graph_ahead_behind(local_oid, upstream_oid)
                    .ok()
                    .map(|(a, b)| (a as u32, b as u32))
                    .unwrap_or((0, 0))
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    Some(GitBranchInfo::new(branch_name, ahead, behind))
}

/// Load git diff preview for a file.
///
/// Computes the diff between HEAD and working tree for the specified file,
/// returning the patch in unified diff format. Both git operations and diff
/// computation run on thread pool via `smol::unblock` to avoid blocking executor.
///
/// # Arguments
///
/// * `repo_path` - Path to repository root (used to discover repository)
/// * `file_path` - Path to file relative to repository root
///
/// # Returns
///
/// Optional diff preview containing patch text, or None if diff computation fails.
pub async fn load_git_diff(repo_path: &Path, file_path: &Path) -> Option<DiffPreviewData> {
    let repo_path = repo_path.to_path_buf();
    let file_path = file_path.to_path_buf();

    smol::unblock(move || {
        // Open repository
        let repo = git2::Repository::open(&repo_path).ok()?;

        // Get HEAD tree
        let head = repo.head().ok()?;
        let head_tree = head.peel_to_tree().ok()?;

        // Get working tree diff
        let mut diff_options = git2::DiffOptions::new();
        diff_options.pathspec(&file_path);

        let diff = repo
            .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut diff_options))
            .ok()?;

        // Convert diff to patch text
        let mut patch_text = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let origin = line.origin();
            let content = std::str::from_utf8(line.content()).unwrap_or("");

            match origin {
                '+' | '-' | ' ' => {
                    patch_text.push(origin);
                    patch_text.push_str(content);
                },
                '>' | '<' => {
                    // File mode changes, context markers
                    patch_text.push_str(content);
                },
                'F' => {
                    // File header
                    patch_text.push_str("diff --git ");
                    patch_text.push_str(content);
                },
                'H' => {
                    // Hunk header
                    patch_text.push_str("@@ ");
                    patch_text.push_str(content);
                },
                _ => {
                    // Other lines (index, file names, etc)
                    patch_text.push_str(content);
                },
            }

            true // Continue iteration
        })
        .ok()?;

        if patch_text.is_empty() {
            None
        } else {
            Some(DiffPreviewData::new(patch_text))
        }
    })
    .await
}
