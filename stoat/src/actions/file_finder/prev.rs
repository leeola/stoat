//! File finder prev action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Move to previous file in finder.
    ///
    /// Decrements the selection index to highlight the previous file in the filtered list.
    /// Automatically loads a preview for the newly selected file.
    ///
    /// # Behavior
    ///
    /// - Only operates in file_finder mode
    /// - Stops at beginning of list (no wrapping)
    /// - Spawns async task to load preview via [`Stoat::load_preview_for_selected`]
    ///
    /// # Related
    ///
    /// - [`Stoat::file_finder_next`] - navigate to next file
    /// - [`Stoat::load_preview_for_selected`] - async preview loading
    pub fn file_finder_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected > 0 {
            self.file_finder_selected -= 1;
            debug!(selected = self.file_finder_selected, "File finder: prev");

            // Load preview for newly selected file
            self.load_preview_for_selected(cx);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_previous_file(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_file_finder(cx);
            s.file_finder_next(cx);
            let before = s.file_finder_selected;
            s.file_finder_prev(cx);
            assert!(s.file_finder_selected < before);
        });
    }
}
