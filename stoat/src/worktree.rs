//! Worktree abstraction for file discovery.
//!
//! This module provides a Worktree/Snapshot pattern similar to Zed's architecture,
//! enabling instant file finder access through pre-computed file lists.
//!
//! # Architecture
//!
//! The worktree module follows Zed's design pattern:
//! - [`Worktree`] manages the file tree state and performs discovery
//! - [`Snapshot`] provides immutable access to the file list
//! - [`Entry`] represents individual file/directory metadata
//!
//! # Usage
//!
//! ```ignore
//! let worktree = Worktree::new(PathBuf::from("."));
//! let snapshot = worktree.snapshot();
//! let files = snapshot.files(); // Fast read access
//! ```
//!
//! # Future Extension
//!
//! This architecture supports adding background file watching similar to Zed:
//! - Background scanner can run continuously
//! - Updates [`Snapshot`] atomically when FS changes detected
//! - All readers see consistent state via snapshot cloning

use std::path::{Path, PathBuf};

/// Worktree manages the file tree for a directory.
///
/// Similar to Zed's `LocalWorktree`, this struct owns the file discovery state
/// and provides access via immutable [`Snapshot`]s. Currently performs a single
/// initial scan on construction, but the architecture supports adding background
/// file watching in the future.
///
/// # Design
///
/// The worktree maintains a [`Snapshot`] of all discovered files. In Zed's
/// implementation, background scanner tasks continuously update this snapshot
/// based on filesystem events. For now, we perform one-time initialization.
///
/// # Usage in Stoat
///
/// The [`Worktree`] is stored in [`crate::Stoat`] as `Arc<Mutex<Worktree>>`,
/// allowing multiple clones of Stoat to share the same file list. The file finder
/// ([`crate::actions::shell::open_file_finder`]) reads from the snapshot for
/// instant file access.
///
/// # Related
///
/// See also:
/// - [`Snapshot`] - immutable file tree data
/// - [`Entry`] - individual file metadata
/// - [`crate::actions::shell::open_file_finder`] - primary consumer
pub struct Worktree {
    /// Current snapshot of the file tree
    snapshot: Snapshot,
    /// Root directory being tracked
    root: PathBuf,
}

/// Immutable snapshot of files in the worktree.
///
/// Similar to Zed's `Snapshot`, this provides read-only access to the file tree.
/// Snapshots are cheap to clone and provide a consistent view of the file tree
/// at a point in time. This enables background updates (in the future) without
/// blocking readers.
///
/// # Design
///
/// The snapshot is intentionally simple - just a sorted list of paths. Zed's
/// implementation uses `SumTree<Entry>` for efficient searching and updates,
/// but for our current needs a `Vec<PathBuf>` is sufficient and simpler.
///
/// # Usage
///
/// Obtained via [`Worktree::snapshot()`] and used by file finder for fuzzy
/// matching and filtering.
#[derive(Clone)]
pub struct Snapshot {
    /// All discovered files, sorted alphabetically
    files: Vec<PathBuf>,
    /// Root directory of this snapshot
    root: PathBuf,
}

/// Metadata for a single file or directory entry.
///
/// Simplified version of Zed's `Entry` struct. Zed tracks extensive metadata
/// including git ignore status, symlinks, char bags for fuzzy matching, etc.
/// We start with the essentials and can expand as needed.
///
/// # Future expansion
///
/// Could add:
/// - `is_ignored: bool` - git ignore status
/// - `char_bag: CharBag` - for faster fuzzy matching
/// - `mtime: SystemTime` - modification time
/// - `size: u64` - file size
#[allow(dead_code)]
pub struct Entry {
    /// Path relative to worktree root
    pub path: PathBuf,
    /// Whether this entry is a directory
    pub is_dir: bool,
}

impl Worktree {
    /// Create a new worktree by discovering files in the given root directory.
    ///
    /// Performs synchronous I/O to walk the directory tree. This should be called
    /// during initialization, not in performance-critical paths. In Zed, this is
    /// done by background scanner tasks, but we do it synchronously on creation.
    ///
    /// # Arguments
    ///
    /// * `root` - Root directory to scan
    ///
    /// # Filters
    ///
    /// Automatically excludes:
    /// - Hidden files/directories (starting with `.`)
    /// - Common ignore patterns (`node_modules`, `target`, `.git`, etc.)
    /// - Files larger than 10MB
    /// - Directories deeper than 10 levels
    ///
    /// # Related
    ///
    /// This is called from [`crate::Stoat::new()`] during editor initialization.
    pub fn new(root: PathBuf) -> Self {
        let mut files = Vec::new();
        Self::walk_directory(&root, &mut files, 0);
        files.sort();

        let snapshot = Snapshot {
            files,
            root: root.clone(),
        };

        Self { snapshot, root }
    }

    /// Get an immutable snapshot of the current file tree.
    ///
    /// Returns a cheap clone of the current snapshot. In Zed's implementation,
    /// this allows background updates to occur while readers access a consistent
    /// view. For now, the snapshot never changes after initialization.
    ///
    /// # Usage
    ///
    /// ```ignore
    /// let snapshot = worktree.snapshot();
    /// let files = snapshot.files();
    /// ```
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    /// Get the root directory of this worktree.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Manually refresh the file list by re-scanning the filesystem.
    ///
    /// This is synchronous and will block. In Zed, refreshes happen automatically
    /// via background scanner responding to FS events. We provide this for manual
    /// updates if needed.
    ///
    /// # Future
    ///
    /// When file watching is added, this could be triggered by FS events or
    /// called manually to force a refresh.
    #[allow(dead_code)]
    pub fn refresh(&mut self) {
        let mut files = Vec::new();
        Self::walk_directory(&self.root, &mut files, 0);
        files.sort();

        self.snapshot = Snapshot {
            files,
            root: self.root.clone(),
        };
    }

    /// Recursively walk a directory and collect file paths.
    ///
    /// Similar to Zed's `BackgroundScanner::scan_dir()`, but much simpler.
    /// Zed handles git repos, gitignore files, symlinks, metadata, etc.
    /// We just collect file paths with basic filtering.
    ///
    /// # Arguments
    ///
    /// * `dir` - Directory to walk
    /// * `files` - Accumulator for discovered files
    /// * `depth` - Current recursion depth (for limiting depth)
    fn walk_directory(dir: &Path, files: &mut Vec<PathBuf>, depth: usize) {
        // Limit recursion depth to avoid infinite loops
        if depth > 10 {
            return;
        }

        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            // Skip hidden files/directories
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') {
                    continue;
                }

                // Skip common ignore patterns
                if matches!(
                    name,
                    "node_modules" | "target" | "dist" | "build" | ".git" | ".svn" | ".hg"
                ) {
                    continue;
                }
            }

            if path.is_file() {
                // Skip very large files (> 10MB)
                if let Ok(metadata) = entry.metadata() {
                    if metadata.len() > 10 * 1024 * 1024 {
                        continue;
                    }
                }

                files.push(path);
            } else if path.is_dir() {
                Self::walk_directory(&path, files, depth + 1);
            }
        }
    }
}

impl Snapshot {
    /// Get read-only access to the file list.
    ///
    /// Returns a slice of all files in the snapshot, sorted alphabetically.
    /// Used by file finder ([`crate::actions::shell::open_file_finder`]) to
    /// populate the file list for fuzzy filtering.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Get the root directory of this snapshot.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_discovers_rust_files() {
        let worktree = Worktree::new(PathBuf::from("src"));
        let snapshot = worktree.snapshot();
        let files = snapshot.files();

        assert!(!files.is_empty(), "Should discover files in src/");

        // Should find lib.rs
        assert!(
            files.iter().any(|p| p.ends_with("lib.rs")),
            "Should find lib.rs"
        );
    }

    #[test]
    fn snapshot_ignores_hidden() {
        let worktree = Worktree::new(PathBuf::from("."));
        let snapshot = worktree.snapshot();
        let files = snapshot.files();

        // Should not include .git directory files
        assert!(
            !files.iter().any(|p| p.to_string_lossy().contains(".git/")),
            "Should not include .git directory"
        );
    }

    #[test]
    fn worktree_refresh_updates_list() {
        let mut worktree = Worktree::new(PathBuf::from("src"));
        let initial_count = worktree.snapshot().files().len();

        // Refresh should work (may or may not change count depending on FS state)
        worktree.refresh();
        let after_count = worktree.snapshot().files().len();

        // Should still have files
        assert!(after_count > 0, "Should still have files after refresh");

        // For a stable directory, count should be same or similar
        let diff = (initial_count as i32 - after_count as i32).abs();
        assert!(
            diff < 100,
            "File count shouldn't drastically change on refresh"
        );
    }
}
