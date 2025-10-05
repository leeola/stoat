//! File index for fast file finder access.
//!
//! This module provides a pre-computed index of all files in the project, enabling
//! instant file finder performance. The index is built once during editor initialization
//! and can be refreshed manually or (in future) automatically via file watching.
//!
//! # Architecture
//!
//! The [`FileIndex`] is designed to be shared across all editor instances via
//! `Arc<Mutex<FileIndex>>`. This ensures:
//! - All panes see the same file list
//! - File finder opens instantly (no I/O)
//! - Future file watching can update all instances atomically
//!
//! # Future Extension
//!
//! The architecture supports adding file system watching:
//! ```ignore
//! file_index.start_watching(); // Subscribe to FS events
//! file_index.refresh();        // Manual refresh
//! ```

use std::path::{Path, PathBuf};

/// Pre-computed index of all files in the project.
///
/// Discovers files recursively from the root directory, applying filters for
/// hidden files, ignored patterns, and size limits. The index is sorted
/// alphabetically for consistent presentation.
///
/// # Usage
///
/// ```ignore
/// let index = FileIndex::new(PathBuf::from("."));
/// let files = index.files(); // Fast read access
/// ```
///
/// # Future
///
/// This struct is designed to support file watching in the future:
/// - [`refresh()`](Self::refresh) can be called manually or on file events
/// - Could add `start_watching()` to subscribe to filesystem changes
pub struct FileIndex {
    /// All discovered files, sorted alphabetically
    files: Vec<PathBuf>,
    /// Root directory for file discovery
    root: PathBuf,
}

impl FileIndex {
    /// Create a new file index by discovering files in the given root directory.
    ///
    /// This performs synchronous I/O and should be called during initialization,
    /// not in performance-critical paths.
    ///
    /// # Arguments
    ///
    /// * `root` - Root directory to index
    ///
    /// # Filters
    ///
    /// Automatically excludes:
    /// - Hidden files/directories (starting with `.`)
    /// - Common ignore patterns (`node_modules`, `target`, `.git`, etc.)
    /// - Files larger than 10MB
    /// - Directories deeper than 10 levels
    pub fn new(root: PathBuf) -> Self {
        let mut files = Vec::new();
        Self::walk_directory(&root, &mut files, 0);
        files.sort();

        Self { files, root }
    }

    /// Get read-only access to the file list.
    ///
    /// Returns a slice of all indexed files, sorted alphabetically.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Get the root directory of this index.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Manually refresh the file index by re-scanning the filesystem.
    ///
    /// This is a synchronous operation that will block while walking the directory tree.
    /// In the future, this could be called in response to file system events for
    /// incremental updates.
    ///
    /// # Performance
    ///
    /// O(n) where n is the number of files/directories in the project.
    #[allow(dead_code)]
    pub fn refresh(&mut self) {
        self.files.clear();
        Self::walk_directory(&self.root, &mut self.files, 0);
        self.files.sort();
    }

    /// Recursively walk a directory and collect file paths.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_index_discovers_rust_files() {
        let index = FileIndex::new(PathBuf::from("src"));
        let files = index.files();

        assert!(!files.is_empty(), "Should discover files in src/");

        // Should find lib.rs
        assert!(
            files.iter().any(|p| p.ends_with("lib.rs")),
            "Should find lib.rs"
        );
    }

    #[test]
    fn file_index_ignores_hidden() {
        let index = FileIndex::new(PathBuf::from("."));
        let files = index.files();

        // Should not include .git directory files
        assert!(
            !files.iter().any(|p| p.to_string_lossy().contains(".git/")),
            "Should not include .git directory"
        );
    }

    #[test]
    fn file_index_refresh_updates_list() {
        let mut index = FileIndex::new(PathBuf::from("src"));
        let initial_count = index.files().len();

        // Refresh should work (may or may not change count depending on FS state)
        index.refresh();
        let after_count = index.files().len();

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
