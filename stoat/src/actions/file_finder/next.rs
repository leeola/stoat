//! File finder next action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Move to next file in finder.
    ///
    /// Increments the selection index to highlight the next file in the filtered list.
    /// Automatically loads a preview for the newly selected file using async task spawning
    /// with [`Context<Self>`]. This demonstrates the Context pattern for self-updating tasks.
    ///
    /// # Behavior
    ///
    /// - Only operates in file_finder mode
    /// - Stops at end of list (no wrapping)
    /// - Spawns async task to load preview via [`Stoat::load_preview_for_selected`]
    ///
    /// # Related
    ///
    /// - [`Stoat::file_finder_prev`] - navigate to previous file
    /// - [`Stoat::load_preview_for_selected`] - async preview loading
    pub fn file_finder_next(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected + 1 < self.file_finder_filtered.len() {
            self.file_finder_selected += 1;
            debug!(selected = self.file_finder_selected, "File finder: next");

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
    fn moves_to_next_file(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_file_finder(cx);
            let initial = s.file_finder_selected;
            s.file_finder_next(cx);
            if s.file_finder_filtered.len() > 1 {
                assert_eq!(s.file_finder_selected, initial + 1);
            }
        });
    }
}
