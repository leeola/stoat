//! File finder dismiss action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Dismiss file finder.
    ///
    /// Clears all file finder state including input buffer, file lists, selection index,
    /// preview data, and background tasks. Mode and KeyContext transitions are now handled
    /// by the [`SetKeyContext`](crate::actions::SetKeyContext) action bound to Escape.
    ///
    /// # State Cleared
    ///
    /// - `file_finder_input` - search input buffer
    /// - `file_finder_files` - full file list from worktree
    /// - `file_finder_filtered` - filtered file list from fuzzy matching
    /// - `file_finder_selected` - selection index
    /// - `file_finder_preview` - preview data (plain or highlighted)
    /// - `file_finder_preview_task` - background preview loading task
    /// - `file_finder_previous_mode` - saved mode for restoration
    ///
    /// # Behavior
    ///
    /// - Only operates in file_finder mode
    /// - Cancels any running preview loading tasks
    /// - Does not restore previous mode (handled by SetKeyContext action)
    ///
    /// # Related
    ///
    /// - [`crate::actions::SetKeyContext`] - handles mode/context transitions
    /// - [`Stoat::open_file_finder`] - initializes finder state
    pub fn file_finder_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        debug!("Dismissing file finder");

        // Clear state
        self.file_finder_input = None;
        self.file_finder_files.clear();
        self.file_finder_filtered.clear();
        self.file_finder_selected = 0;
        self.file_finder_preview = None;
        self.file_finder_preview_task = None;
        self.file_finder_previous_mode = None;

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn dismisses_file_finder(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_file_finder(cx);
            assert!(s.file_finder_input.is_some());
            s.file_finder_dismiss(cx);
            assert!(s.file_finder_input.is_none());
            assert!(s.file_finder_files.is_empty());
        });
    }
}
