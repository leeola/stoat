//! File finder navigation commands
//!
//! Handles navigation within the file finder modal: moving selection up/down and dismissing.

use crate::Stoat;
use tracing::debug;

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
        }
    }

    /// Dismiss the file finder and return to the previous mode.
    ///
    /// Closes the file finder modal, clears all file finder state, and restores
    /// the mode that was active before opening the file finder.
    ///
    /// # Behavior
    ///
    /// - Restores previous mode (typically Normal)
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

        // Restore previous mode
        if let Some(previous_mode) = self.file_finder_previous_mode.take() {
            self.set_mode(&previous_mode);
        } else {
            self.set_mode("normal");
        }

        // Clear file finder state
        self.file_finder_input = None;
        self.file_finder_files.clear();
        self.file_finder_filtered.clear();
        self.file_finder_selected = 0;
    }

    /// Select the currently highlighted file in the file finder.
    ///
    /// Opens the selected file in the editor and dismisses the file finder.
    ///
    /// # Behavior
    ///
    /// - Loads the selected file into the buffer
    /// - Dismisses the file finder
    /// - Returns to previous mode
    /// - No-op if file finder is not open or no file is selected
    ///
    /// # Implementation Note
    ///
    /// Currently a placeholder - file loading will be implemented when we have
    /// multi-file support in the core.
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::Stoat::file_finder_dismiss`] - close without selecting
    /// - [`crate::Stoat::open_file_finder`] - open file finder
    pub fn file_finder_select(&mut self) {
        if self.mode() != "file_finder" {
            return;
        }

        if self.file_finder_selected < self.file_finder_filtered.len() {
            let selected_file = &self.file_finder_filtered[self.file_finder_selected];
            debug!(file = ?selected_file, "File finder: select");

            // FIXME: Load file into buffer when multi-file support is added
            // For now, just dismiss the file finder
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
        s.open_file_finder(&mut s.cx);

        // Set up some test files
        s.file_finder_filtered = vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
        ];
        s.file_finder_selected = 0;

        s.file_finder_next();
        assert_eq!(s.file_finder_selected, 1);

        s.file_finder_next();
        assert_eq!(s.file_finder_selected, 2);

        // Should not go past end
        s.file_finder_next();
        assert_eq!(s.file_finder_selected, 2);
    }

    #[test]
    fn file_finder_prev_decrements() {
        let mut s = Stoat::test();
        s.open_file_finder(&mut s.cx);

        // Set up some test files
        s.file_finder_filtered = vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
        ];
        s.file_finder_selected = 2;

        s.file_finder_prev();
        assert_eq!(s.file_finder_selected, 1);

        s.file_finder_prev();
        assert_eq!(s.file_finder_selected, 0);

        // Should not go below 0
        s.file_finder_prev();
        assert_eq!(s.file_finder_selected, 0);
    }

    #[test]
    fn file_finder_dismiss_clears_state() {
        let mut s = Stoat::test();
        s.open_file_finder(&mut s.cx);

        assert_eq!(s.mode(), "file_finder");
        assert!(s.file_finder_input.is_some());

        s.file_finder_dismiss();

        assert_eq!(s.mode(), "normal");
        assert!(s.file_finder_input.is_none());
        assert!(s.file_finder_files.is_empty());
        assert!(s.file_finder_filtered.is_empty());
        assert_eq!(s.file_finder_selected, 0);
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
