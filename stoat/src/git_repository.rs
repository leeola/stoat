//! Git repository operations.
//!
//! This module provides a wrapper around libgit2 for git operations needed by the editor,
//! primarily focused on reading file content from different git refs (HEAD, index) for diff
//! visualization.
//!
//! # Architecture
//!
//! [`Repository`] wraps [`git2::Repository`] and provides high-level operations:
//! - Finding repository for a file path
//! - Reading file content from HEAD
//! - Reading file content from index (staging area)
//!
//! These operations support the git diff visualization system by providing the base content
//! to compare against the current working tree.
//!
//! # Usage
//!
//! ```ignore
//! use std::path::Path;
//! use stoat::git_repository::Repository;
//!
//! // Find repository for a file
//! if let Ok(repo) = Repository::discover(Path::new("src/main.rs")) {
//!     // Read file content from HEAD
//!     if let Ok(content) = repo.head_content(Path::new("src/main.rs")) {
//!         println!("HEAD content: {}", content);
//!     }
//! }
//! ```
//!
//! # Related
//!
//! - [`git_diff`](crate::git_diff) - Uses this to get base content for diff computation
//! - [`BufferItem`](crate::BufferItem) - Stores computed diffs

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use thiserror::Error;

/// Errors that can occur during git operations.
#[derive(Debug, Error)]
pub enum GitError {
    /// Git repository not found
    #[error("Git repository not found for path: {0}")]
    RepositoryNotFound(PathBuf),

    /// Failed to open repository
    #[error("Failed to open repository: {0}")]
    OpenFailed(String),

    /// File not found in git
    #[error("File not found in git: {0}")]
    FileNotFound(PathBuf),

    /// Failed to read file content from git
    #[error("Failed to read content: {0}")]
    ReadFailed(String),

    /// Git operation failed
    #[error("Git error: {0}")]
    GitOperationFailed(String),
}

/// A git repository wrapper providing access to file content at different refs.
///
/// Wraps [`git2::Repository`] and provides high-level operations for the editor's needs.
/// The primary use case is reading file content from HEAD or index for diff computation.
///
/// # Lifetime
///
/// A [`Repository`] instance represents a connection to a git repository on disk.
/// Multiple operations can be performed using the same instance.
///
/// # Thread Safety
///
/// [`Repository`] is not thread-safe. Create separate instances for different threads.
///
/// # Example
///
/// ```ignore
/// let repo = Repository::discover(Path::new("src/lib.rs"))?;
/// let head_content = repo.head_content(Path::new("src/lib.rs"))?;
/// let index_content = repo.index_content(Path::new("src/lib.rs"))?;
/// ```
pub struct Repository {
    /// Underlying libgit2 repository
    repo: git2::Repository,
    /// Working directory path (cached for relative path resolution)
    workdir: PathBuf,
}

impl Repository {
    /// Discover and open a git repository containing the given path.
    ///
    /// Searches upward from the given path to find a git repository.
    /// This is the primary way to create a [`Repository`] instance.
    ///
    /// # Arguments
    ///
    /// * `path` - File or directory path to start searching from
    ///
    /// # Returns
    ///
    /// [`Repository`] if found, [`GitError::RepositoryNotFound`] otherwise.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Finds repository even if path is deep in the tree
    /// let repo = Repository::discover(Path::new("src/deeply/nested/file.rs"))?;
    /// ```
    pub fn discover(path: &Path) -> Result<Self, GitError> {
        let repo = git2::Repository::discover(path).map_err(|e| {
            if e.code() == git2::ErrorCode::NotFound {
                GitError::RepositoryNotFound(path.to_path_buf())
            } else {
                GitError::OpenFailed(e.message().to_string())
            }
        })?;

        let workdir = repo
            .workdir()
            .ok_or_else(|| GitError::OpenFailed("Repository has no working directory".to_string()))?
            .to_path_buf();

        Ok(Self { repo, workdir })
    }

    /// Open a repository at a specific path.
    ///
    /// Opens the repository at the exact path given, without searching upward.
    /// Use [`discover`](Self::discover) instead if you have a file path.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to .git directory or working tree root
    ///
    /// # Returns
    ///
    /// [`Repository`] if successfully opened
    pub fn open(path: &Path) -> Result<Self, GitError> {
        let repo = git2::Repository::open(path)
            .map_err(|e| GitError::OpenFailed(e.message().to_string()))?;

        let workdir = repo
            .workdir()
            .ok_or_else(|| GitError::OpenFailed("Repository has no working directory".to_string()))?
            .to_path_buf();

        Ok(Self { repo, workdir })
    }

    /// Read file content from HEAD.
    ///
    /// Gets the content of the file as it exists in the most recent commit (HEAD).
    /// This is used as the base for computing diffs against the working tree.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to file, can be absolute or relative to working directory
    ///
    /// # Returns
    ///
    /// File content as a string, or error if file doesn't exist in HEAD
    ///
    /// # Errors
    ///
    /// Returns [`GitError::FileNotFound`] if the file doesn't exist in HEAD.
    /// Returns [`GitError::ReadFailed`] if the file exists but can't be read.
    pub fn head_content(&self, path: &Path) -> Result<String, GitError> {
        let relative_path = self.make_relative(path)?;

        let head = self
            .repo
            .head()
            .map_err(|e| GitError::GitOperationFailed(format!("Failed to get HEAD: {e}")))?;

        let tree = head
            .peel_to_tree()
            .map_err(|e| GitError::GitOperationFailed(format!("Failed to get tree: {e}")))?;

        let entry = tree
            .get_path(&relative_path)
            .map_err(|_| GitError::FileNotFound(path.to_path_buf()))?;

        let object = entry
            .to_object(&self.repo)
            .map_err(|e| GitError::ReadFailed(format!("Failed to get object: {e}")))?;

        let blob = object
            .as_blob()
            .ok_or_else(|| GitError::ReadFailed("Object is not a blob".to_string()))?;

        String::from_utf8(blob.content().to_vec())
            .map_err(|e| GitError::ReadFailed(format!("Invalid UTF-8: {e}")))
    }

    /// Read file content from the index (staging area).
    ///
    /// Gets the content of the file as it exists in the index. This is used
    /// for computing diffs between HEAD and index (staged changes).
    ///
    /// # Arguments
    ///
    /// * `path` - Path to file, can be absolute or relative to working directory
    ///
    /// # Returns
    ///
    /// File content as a string, or error if file doesn't exist in index
    pub fn index_content(&self, path: &Path) -> Result<String, GitError> {
        let relative_path = self.make_relative(path)?;

        let index = self
            .repo
            .index()
            .map_err(|e| GitError::GitOperationFailed(format!("Failed to get index: {e}")))?;

        let entry = index
            .get_path(&relative_path, 0)
            .ok_or_else(|| GitError::FileNotFound(path.to_path_buf()))?;

        let object = self
            .repo
            .find_object(entry.id, Some(git2::ObjectType::Blob))
            .map_err(|e| GitError::ReadFailed(format!("Failed to find object: {e}")))?;

        let blob = object
            .as_blob()
            .ok_or_else(|| GitError::ReadFailed("Object is not a blob".to_string()))?;

        String::from_utf8(blob.content().to_vec())
            .map_err(|e| GitError::ReadFailed(format!("Invalid UTF-8: {e}")))
    }

    /// Get the working directory path of this repository.
    ///
    /// This is the root directory of the working tree.
    ///
    /// # Returns
    ///
    /// Reference to the working directory path
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Get a reference to the inner git2 repository.
    ///
    /// Provides access to the underlying [`git2::Repository`] for operations
    /// not directly exposed by this wrapper.
    ///
    /// # Returns
    ///
    /// Reference to the underlying git2 repository
    pub fn inner(&self) -> &git2::Repository {
        &self.repo
    }

    /// Count hunks per file for a given diff comparison mode.
    ///
    /// Uses git2's diff API to efficiently compute hunk counts for all modified files.
    /// This is much faster than manually reading files and computing diffs, and correctly
    /// handles new untracked files (compares them against an empty base).
    ///
    /// # Arguments
    ///
    /// * `comparison_mode` - Which git refs to compare (WorkingVsHead, WorkingVsIndex, IndexVsHead)
    ///
    /// # Returns
    ///
    /// HashMap mapping file paths (relative to repo root) to their hunk counts.
    /// Returns empty map if no files have changes.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let repo = Repository::discover(Path::new("."))?;
    /// let counts = repo.count_hunks_by_file(DiffComparisonMode::WorkingVsHead)?;
    /// for (path, count) in counts {
    ///     println!("{:?}: {} hunks", path, count);
    /// }
    /// ```
    pub fn count_hunks_by_file(
        &self,
        comparison_mode: crate::diff_review::DiffComparisonMode,
    ) -> Result<HashMap<PathBuf, usize>, GitError> {
        use crate::diff_review::DiffComparisonMode;

        // Create diff based on comparison mode
        // Configure diff options to include untracked files with full content
        // Use 0 context lines to match BufferDiff behavior (only show changed lines)
        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true);
        opts.recurse_untracked_dirs(true);
        opts.show_untracked_content(true);
        opts.context_lines(0);

        let diff = match comparison_mode {
            DiffComparisonMode::WorkingVsHead => {
                let head = self.repo.head().map_err(|e| {
                    GitError::GitOperationFailed(format!("Failed to get HEAD: {e}"))
                })?;
                let tree = head.peel_to_tree().map_err(|e| {
                    GitError::GitOperationFailed(format!("Failed to get tree: {e}"))
                })?;
                self.repo
                    .diff_tree_to_workdir_with_index(Some(&tree), Some(&mut opts))
                    .map_err(|e| {
                        GitError::GitOperationFailed(format!("Failed to create diff: {e}"))
                    })?
            },
            DiffComparisonMode::WorkingVsIndex => self
                .repo
                .diff_index_to_workdir(None, Some(&mut opts))
                .map_err(|e| GitError::GitOperationFailed(format!("Failed to create diff: {e}")))?,
            DiffComparisonMode::IndexVsHead => {
                let head = self.repo.head().map_err(|e| {
                    GitError::GitOperationFailed(format!("Failed to get HEAD: {e}"))
                })?;
                let tree = head.peel_to_tree().map_err(|e| {
                    GitError::GitOperationFailed(format!("Failed to get tree: {e}"))
                })?;
                // IndexVsHead doesn't need untracked files (they're not in the index)
                self.repo
                    .diff_tree_to_index(Some(&tree), None, Some(&mut opts))
                    .map_err(|e| {
                        GitError::GitOperationFailed(format!("Failed to create diff: {e}"))
                    })?
            },
        };

        // Count hunks per file using foreach
        // Use RefCell for interior mutability since both closures need mutable access
        use std::cell::RefCell;
        let hunk_counts: RefCell<HashMap<PathBuf, usize>> = RefCell::new(HashMap::new());

        diff.foreach(
            &mut |delta, _progress| {
                // File callback - initialize count for this file
                if let Some(path) = delta.new_file().path() {
                    hunk_counts.borrow_mut().insert(path.to_path_buf(), 0);
                }
                true
            },
            None,
            Some(&mut |delta, _hunk| {
                // Hunk callback - increment count for this file
                if let Some(path) = delta.new_file().path() {
                    let mut counts = hunk_counts.borrow_mut();
                    *counts.entry(path.to_path_buf()).or_insert(0) += 1;
                }
                true
            }),
            None,
        )
        .map_err(|e| GitError::GitOperationFailed(format!("Failed to iterate diff: {e}")))?;

        Ok(hunk_counts.into_inner())
    }

    /// Convert an absolute or relative path to a path relative to the repository root.
    ///
    /// Helper method for converting file paths to the format expected by libgit2.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to convert (absolute or relative to current directory)
    ///
    /// # Returns
    ///
    /// Path relative to repository root
    fn make_relative(&self, path: &Path) -> Result<PathBuf, GitError> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| GitError::GitOperationFailed(format!("Failed to get cwd: {e}")))?
                .join(path)
        };

        // Canonicalize both paths to handle symlinks (e.g., /var vs /private/var on macOS)
        // If the file doesn't exist, we can't canonicalize it, so just use the path as-is
        let (absolute_canonical, workdir_canonical) =
            match (absolute.canonicalize(), self.workdir.canonicalize()) {
                (Ok(abs), Ok(work)) => (abs, work),
                _ => {
                    // If canonicalization fails (file doesn't exist), use the parent directory
                    // This allows us to handle missing files that might exist in git
                    let parent = absolute.parent().ok_or_else(|| {
                        GitError::GitOperationFailed("Path has no parent directory".to_string())
                    })?;
                    let parent_canonical = parent
                        .canonicalize()
                        .unwrap_or_else(|_| parent.to_path_buf());
                    let workdir_canonical = self
                        .workdir
                        .canonicalize()
                        .unwrap_or_else(|_| self.workdir.clone());
                    let filename = absolute.file_name().ok_or_else(|| {
                        GitError::GitOperationFailed("Path has no filename".to_string())
                    })?;
                    (parent_canonical.join(filename), workdir_canonical)
                },
            };

        absolute_canonical
            .strip_prefix(&workdir_canonical)
            .map(|p| p.to_path_buf())
            .map_err(|_| {
                GitError::GitOperationFailed(format!(
                    "Path {absolute_canonical:?} is not in repository {workdir_canonical:?}"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    /// Helper to create a temporary git repository for testing.
    fn create_test_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let path = dir.path().to_path_buf();

        Command::new("git")
            .args(&["init"])
            .current_dir(&path)
            .output()
            .expect("Failed to init git repo");

        Command::new("git")
            .args(&["config", "user.name", "Test"])
            .current_dir(&path)
            .output()
            .expect("Failed to configure git");

        Command::new("git")
            .args(&["config", "user.email", "test@example.com"])
            .current_dir(&path)
            .output()
            .expect("Failed to configure git");

        (dir, path)
    }

    #[test]
    fn discover_repository() {
        let (_dir, path) = create_test_repo();

        let repo = Repository::discover(&path).expect("Should find repository");
        // Canonicalize both paths for comparison (handles /var vs /private/var on macOS)
        let expected = path.canonicalize().expect("Failed to canonicalize path");
        let actual = repo
            .workdir()
            .canonicalize()
            .expect("Failed to canonicalize workdir");
        assert_eq!(actual, expected);
    }

    #[test]
    fn discover_repository_not_found() {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let result = Repository::discover(dir.path());

        assert!(matches!(result, Err(GitError::RepositoryNotFound(_))));
    }

    #[test]
    fn read_head_content() {
        let (_dir, path) = create_test_repo();

        let file_path = path.join("test.txt");
        fs::write(&file_path, "hello world\n").expect("Failed to write file");

        Command::new("git")
            .args(&["add", "test.txt"])
            .current_dir(&path)
            .output()
            .expect("Failed to git add");

        Command::new("git")
            .args(&["commit", "-m", "Initial commit"])
            .current_dir(&path)
            .output()
            .expect("Failed to git commit");

        let repo = Repository::open(&path).expect("Should open repository");
        let content = repo
            .head_content(&file_path)
            .expect("Should read HEAD content");

        assert_eq!(content, "hello world\n");
    }

    #[test]
    fn read_head_content_file_not_found() {
        let (_dir, path) = create_test_repo();

        let file_path = path.join("test.txt");
        fs::write(&file_path, "hello\n").expect("Failed to write file");

        Command::new("git")
            .args(&["add", "test.txt"])
            .current_dir(&path)
            .output()
            .expect("Failed to git add");

        Command::new("git")
            .args(&["commit", "-m", "Initial commit"])
            .current_dir(&path)
            .output()
            .expect("Failed to git commit");

        let repo = Repository::open(&path).expect("Should open repository");

        let missing_path = path.join("missing.txt");
        let result = repo.head_content(&missing_path);

        assert!(matches!(result, Err(GitError::FileNotFound(_))));
    }
}
