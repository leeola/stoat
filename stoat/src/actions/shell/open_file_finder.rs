//! Open file finder command
//!
//! Opens the file finder modal for quick file navigation. Discovers files in the current
//! directory recursively and sets up the file finder state with an input buffer for filtering.

use crate::Stoat;
use gpui::{App, AppContext};
use std::{
    num::NonZeroU64,
    path::{Path, PathBuf},
};
use text::{Buffer, BufferId};
use tracing::debug;

impl Stoat {
    /// Open the file finder modal.
    ///
    /// Discovers all files in the current directory (recursively), creates an input buffer
    /// for the search query, and transitions to file_finder mode.
    ///
    /// # Behavior
    ///
    /// - Saves current mode to restore later
    /// - Discovers files recursively from current directory
    /// - Creates empty input buffer for search query
    /// - Initializes filtered files list (initially all files)
    /// - Sets mode to "file_finder"
    ///
    /// # File Discovery
    ///
    /// Discovers files using simple recursive directory walking, ignoring:
    /// - Hidden files/directories (starting with `.`)
    /// - Common ignore patterns (node_modules, target, .git, etc.)
    /// - Files larger than 10MB
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::Stoat::file_finder_dismiss`] - close file finder
    /// - [`crate::Stoat::file_finder_next`] - navigate down
    /// - [`crate::Stoat::file_finder_prev`] - navigate up
    pub fn open_file_finder(&mut self, cx: &mut App) {
        debug!(from_mode = self.mode(), "Opening file finder");

        // Save current mode to restore later
        self.file_finder_previous_mode = Some(self.current_mode.clone());

        // Discover files in current directory
        let files = discover_files(Path::new("."));
        debug!(file_count = files.len(), "Discovered files");

        // Create input buffer for search query
        let buffer_id = BufferId::from(NonZeroU64::new(2).unwrap()); // Use ID 2 for input buffer
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));

        // Initialize file finder state
        self.file_finder_input = Some(input_buffer);
        self.file_finder_filtered = files.clone();
        self.file_finder_files = files;
        self.file_finder_selected = 0;

        // Load preview for first file
        self.file_finder_preview = self
            .file_finder_filtered
            .first()
            .and_then(|path| load_file_preview(path));

        // Enter file_finder mode
        self.set_mode("file_finder");
    }
}

/// Load a preview of a file's contents.
///
/// Reads up to the first 1000 lines or 100KB of a file for preview display.
/// Returns [`None`] if the file cannot be read, is binary, or contains invalid UTF-8.
///
/// # Arguments
///
/// * `path` - Path to the file to preview
///
/// # Returns
///
/// File contents as a string, or [`None`] if preview cannot be generated
fn load_file_preview(path: &Path) -> Option<String> {
    const MAX_BYTES: usize = 100 * 1024; // 100KB
    const MAX_LINES: usize = 1000;

    // Read file with size limit
    let contents = std::fs::read(path).ok()?;

    // Check for binary content (null bytes in first 1KB)
    let check_size = contents.len().min(1024);
    if contents[..check_size].contains(&0) {
        return None; // Binary file
    }

    // Try to decode as UTF-8
    let text = String::from_utf8(contents).ok()?;

    // Limit to first MAX_BYTES or MAX_LINES
    let mut result = String::new();
    let mut byte_count = 0;
    let mut line_count = 0;

    for line in text.lines() {
        if line_count >= MAX_LINES || byte_count >= MAX_BYTES {
            result.push_str("\n\n... (preview truncated)");
            break;
        }

        if line_count > 0 {
            result.push('\n');
        }
        result.push_str(line);

        byte_count += line.len() + 1; // +1 for newline
        line_count += 1;
    }

    Some(result)
}

/// Discover files recursively from the given root directory.
///
/// Performs simple recursive directory walking, filtering out hidden files,
/// common ignore patterns, and very large files.
///
/// # Arguments
///
/// * `root` - Root directory to start discovery from
///
/// # Returns
///
/// Vector of discovered file paths, sorted alphabetically
fn discover_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_directory(root, &mut files, 0);
    files.sort();
    files
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
            walk_directory(&path, files, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stoat;

    #[test]
    fn open_file_finder_creates_state() {
        let mut s = Stoat::test();
        assert_eq!(s.mode(), "normal");
        assert!(s.file_finder_input.is_none());

        s.open_file_finder(&mut s.cx);

        assert_eq!(s.mode(), "file_finder");
        assert!(s.file_finder_input.is_some());
        assert_eq!(s.file_finder_previous_mode, Some("normal".to_string()));
        assert_eq!(s.file_finder_selected, 0);
    }

    #[test]
    fn discover_files_finds_rust_files() {
        let files = discover_files(Path::new("src"));
        assert!(!files.is_empty(), "Should find files in src directory");

        // Should find lib.rs
        assert!(
            files.iter().any(|p| p.ends_with("lib.rs")),
            "Should find lib.rs"
        );
    }

    #[test]
    fn discover_files_ignores_hidden() {
        let files = discover_files(Path::new("."));

        // Should not include .git directory files
        assert!(
            !files.iter().any(|p| p.to_string_lossy().contains(".git/")),
            "Should not include .git directory"
        );
    }
}
