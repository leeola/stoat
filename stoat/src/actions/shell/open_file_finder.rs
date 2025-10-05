//! Open file finder command
//!
//! Opens the file finder modal for quick file navigation. Discovers files in the current
//! directory recursively and sets up the file finder state with an input buffer for filtering.

use crate::Stoat;
use gpui::{App, AppContext};
use std::num::NonZeroU64;
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

        // Read files from pre-built index (instant!)
        let files = self.file_index.lock().files().to_vec();
        debug!(file_count = files.len(), "Loaded files from index");

        // Create input buffer for search query
        let buffer_id = BufferId::from(NonZeroU64::new(2).unwrap()); // Use ID 2 for input buffer
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));

        // Initialize file finder state
        self.file_finder_input = Some(input_buffer);
        self.file_finder_filtered = files.clone();
        self.file_finder_files = files;
        self.file_finder_selected = 0;

        // Skip initial preview for instant open (load on selection change)
        self.file_finder_preview = None;

        // Enter file_finder mode
        self.set_mode("file_finder");
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn open_file_finder_creates_state() {
        let mut s = Stoat::test();
        assert_eq!(s.mode(), "normal");
        assert!(s.file_finder_input().is_none());

        s.open_file_finder();

        assert_eq!(s.mode(), "file_finder");
        assert!(s.file_finder_input().is_some());
        assert_eq!(s.file_finder_previous_mode(), Some("normal".to_string()));
        assert_eq!(s.file_finder_selected(), 0);
    }
}
