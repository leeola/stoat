//! Open file finder action implementation and tests.

use crate::Stoat;
use gpui::{AppContext, Context};
use std::{num::NonZeroU64, path::PathBuf};
use text::{Buffer, BufferId};
use tracing::debug;

impl Stoat {
    /// Open file finder.
    ///
    /// Initializes the file finder modal with all files from the worktree. Creates an input
    /// buffer for fuzzy search and loads a preview for the first file. This action integrates
    /// with [`crate::stoat::KeyContext::FileFinder`] to display the finder UI and with
    /// [`Stoat::filter_files`] for query-based filtering.
    ///
    /// # Workflow
    ///
    /// 1. Saves current mode for restoration when dismissed
    /// 2. Creates input buffer (BufferId 2) for search queries
    /// 3. Scans worktree for all files
    /// 4. Initializes filtered list with all files
    /// 5. Loads preview for first file using [`Stoat::load_preview_for_selected`]
    ///
    /// # Related
    ///
    /// - [`Stoat::file_finder_next`] - navigate to next file
    /// - [`Stoat::file_finder_prev`] - navigate to previous file
    /// - [`Stoat::file_finder_select`] - select and open file
    /// - [`Stoat::file_finder_dismiss`] - close finder
    /// - [`Stoat::filter_files`] - filter files by query
    pub fn open_file_finder(&mut self, cx: &mut Context<Self>) {
        debug!("Opening file finder");

        // Save current mode
        self.file_finder_previous_mode = Some(self.mode.clone());
        self.key_context = crate::stoat::KeyContext::FileFinder;
        self.mode = "file_finder".to_string();

        // Create input buffer
        let buffer_id = BufferId::from(NonZeroU64::new(2).unwrap());
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
        self.file_finder_input = Some(input_buffer);

        // Scan worktree
        let entries = self.worktree.lock().snapshot().entries(false);
        debug!(file_count = entries.len(), "Loaded files from worktree");

        self.file_finder_files = entries;
        self.file_finder_filtered = self
            .file_finder_files
            .iter()
            .map(|e| PathBuf::from(e.path.as_unix_str()))
            .collect();
        self.file_finder_selected = 0;

        // Load preview for first file
        self.load_preview_for_selected(cx);

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opens_file_finder(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "normal".to_string();
            s.open_file_finder(cx);
            assert_eq!(s.mode(), "file_finder");
            assert!(s.file_finder_input.is_some());
        });
    }
}
