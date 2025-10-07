//! File finder navigation commands
//!
//! Handles navigation within the file finder modal: moving selection up/down and dismissing.

use crate::Stoat;
use std::path::Path;
use stoat_rope::TokenSnapshot;
use stoat_text::{Language, Parser};
use text::{Buffer, BufferId};
use tracing::debug;

/// Preview data containing both text and syntax highlighting tokens.
///
/// This structure holds a file preview along with its parsed tokens for efficient
/// syntax highlighting in the file finder preview panel.
#[derive(Clone)]
pub struct PreviewData {
    /// The preview text content (up to 100KB/1000 lines)
    pub text: String,
    /// Parsed syntax tokens for highlighting
    pub tokens: TokenSnapshot,
}

/// Load a preview of a file's contents with syntax highlighting.
///
/// Reads up to the first 1000 lines or 100KB of a file for preview display,
/// parses it for syntax highlighting, and returns both text and tokens.
/// Returns [`None`] if the file cannot be read, is binary, or contains invalid UTF-8.
///
/// # Arguments
///
/// * `path` - Path to the file to preview
///
/// # Returns
///
/// [`PreviewData`] with text and tokens, or [`None`] if preview cannot be generated
pub fn load_file_preview(path: &Path) -> Option<PreviewData> {
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

    // Detect language from file extension
    let language = Language::from_extension(path.extension()?.to_str()?);

    // Parse for syntax highlighting
    let mut parser = Parser::new(language).ok()?;
    let buffer = Buffer::new(0, BufferId::new(1).ok()?, result.clone());
    let snapshot = buffer.snapshot();
    let parsed_tokens = parser.parse(&result, &snapshot).ok()?;

    // Build token snapshot
    let mut token_map = stoat_rope::TokenMap::new(&snapshot);
    token_map.replace_tokens(parsed_tokens, &snapshot);
    let tokens = token_map.snapshot();

    Some(PreviewData {
        text: result,
        tokens,
    })
}

impl Stoat {
    /// Move to the next file in the file finder list.
    ///
    /// Moves the selection highlight down to the next file in the filtered list.
    /// If at the end of the list, stays at the last file.
    ///
    /// # Behavior
    ///
    /// - Increments selected index if not at end
    /// - Clamps to list bounds
    /// - No-op if file finder is not open
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::Stoat::file_finder_prev`] - move selection up
    /// - [`crate::Stoat::open_file_finder`] - open file finder
    pub fn file_finder_next(&mut self) {
        if self.mode() != "file_finder" {
            return;
        }

        if self.file_finder_selected + 1 < self.file_finder_filtered.len() {
            self.file_finder_selected += 1;
            debug!(selected = self.file_finder_selected, "File finder: next");

            // Load preview for newly selected file
            self.load_preview_for_selected();
        }
    }

    /// Move to the previous file in the file finder list.
    ///
    /// Moves the selection highlight up to the previous file in the filtered list.
    /// If at the beginning of the list, stays at the first file.
    ///
    /// # Behavior
    ///
    /// - Decrements selected index if not at start
    /// - Clamps to list bounds
    /// - No-op if file finder is not open
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::Stoat::file_finder_next`] - move selection down
    /// - [`crate::Stoat::open_file_finder`] - open file finder
    pub fn file_finder_prev(&mut self) {
        if self.mode() != "file_finder" {
            return;
        }

        if self.file_finder_selected > 0 {
            self.file_finder_selected -= 1;
            debug!(selected = self.file_finder_selected, "File finder: prev");

            // Load preview for newly selected file
            self.load_preview_for_selected();
        }
    }

    /// Dismiss the file finder and return to normal mode.
    ///
    /// Closes the file finder modal, clears all file finder state, and returns
    /// to normal mode.
    ///
    /// # Behavior
    ///
    /// - Returns to normal mode
    /// - Clears input buffer
    /// - Clears file lists
    /// - Resets selection index
    /// - No-op if file finder is not open
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::Stoat::open_file_finder`] - open file finder
    /// - [`crate::Stoat::file_finder_select`] - select current file
    pub fn file_finder_dismiss(&mut self) {
        if self.mode() != "file_finder" {
            return;
        }

        debug!("Dismissing file finder");

        // Always return to normal mode
        self.set_mode("normal");

        // Clear file finder state
        self.file_finder_input = None;
        self.file_finder_files.clear();
        self.file_finder_filtered.clear();
        self.file_finder_selected = 0;
        self.file_finder_preview = None;
        self.file_finder_previous_mode = None;
    }

    /// Select the currently highlighted file in the file finder.
    ///
    /// Opens the selected file in the editor and dismisses the file finder.
    ///
    /// # Behavior
    ///
    /// - Loads the selected file into the buffer
    /// - Resets cursor to start of file
    /// - Dismisses the file finder
    /// - Returns to previous mode
    /// - No-op if file finder is not open or no file is selected
    ///
    /// # Implementation
    ///
    /// Uses [`crate::Stoat::load_files`] to handle file loading, which:
    /// - Reads file contents
    /// - Detects language from extension
    /// - Updates parser and tokens
    /// - Replaces buffer contents
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::Stoat::file_finder_dismiss`] - close without selecting
    /// - [`crate::Stoat::open_file_finder`] - open file finder
    /// - [`crate::Stoat::load_files`] - file loading implementation
    pub fn file_finder_select(&mut self, cx: &mut gpui::App) {
        if self.mode() != "file_finder" {
            return;
        }

        if self.file_finder_selected < self.file_finder_filtered.len() {
            let relative_path = &self.file_finder_filtered[self.file_finder_selected];
            debug!(file = ?relative_path, "File finder: select");

            // Build absolute path from worktree root
            let root = self.worktree.lock().snapshot().root().to_path_buf();
            let abs_path = root.join(relative_path);

            // Load file into buffer
            self.load_files(&[&abs_path], cx);

            // Move cursor to start of file
            self.set_cursor_position(text::Point::new(0, 0));
        }

        self.file_finder_dismiss();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use std::path::PathBuf;

    #[test]
    fn file_finder_next_increments() {
        let mut s = Stoat::test();
        s.open_file_finder();

        // Set up some test files
        s.set_file_finder_filtered(vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
        ]);
        s.set_file_finder_selected(0);

        s.file_finder_next();
        assert_eq!(s.file_finder_selected(), 1);

        s.file_finder_next();
        assert_eq!(s.file_finder_selected(), 2);

        // Should not go past end
        s.file_finder_next();
        assert_eq!(s.file_finder_selected(), 2);
    }

    #[test]
    fn file_finder_prev_decrements() {
        let mut s = Stoat::test();
        s.open_file_finder();

        // Set up some test files
        s.set_file_finder_filtered(vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
        ]);
        s.set_file_finder_selected(2);

        s.file_finder_prev();
        assert_eq!(s.file_finder_selected(), 1);

        s.file_finder_prev();
        assert_eq!(s.file_finder_selected(), 0);

        // Should not go below 0
        s.file_finder_prev();
        assert_eq!(s.file_finder_selected(), 0);
    }

    #[test]
    fn file_finder_dismiss_clears_state() {
        let mut s = Stoat::test();
        s.open_file_finder();

        assert_eq!(s.mode(), "file_finder");
        assert!(s.file_finder_input().is_some());

        s.file_finder_dismiss();

        assert_eq!(s.mode(), "normal");
        assert!(s.file_finder_input().is_none());
        assert!(s.file_finder_files().is_empty());
        assert!(s.file_finder_filtered().is_empty());
        assert_eq!(s.file_finder_selected(), 0);
    }

    #[test]
    fn file_finder_actions_noop_outside_mode() {
        let mut s = Stoat::test();
        assert_eq!(s.mode(), "normal");

        // These should be no-ops
        s.file_finder_next();
        s.file_finder_prev();
        s.file_finder_dismiss();

        assert_eq!(s.mode(), "normal");
    }
}
