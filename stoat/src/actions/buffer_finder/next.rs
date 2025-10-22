//! Buffer finder next action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Move to next buffer in finder.
    ///
    /// Increments the selection index to highlight the next buffer in the filtered list.
    /// Unlike file finder, buffer finder does not load previews (buffers are already loaded).
    ///
    /// # Behavior
    ///
    /// - Only operates in buffer_finder mode
    /// - Stops at end of list (no wrapping)
    /// - No preview loading needed (buffers already in memory)
    ///
    /// # Related
    ///
    /// - [`Stoat::buffer_finder_prev`] - navigate to previous buffer
    /// - [`Stoat::file_finder_next`] - similar action with preview loading
    pub fn buffer_finder_next(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        if self.buffer_finder_selected + 1 < self.buffer_finder_filtered.len() {
            self.buffer_finder_selected += 1;
            debug!(
                selected = self.buffer_finder_selected,
                "Buffer finder: next"
            );
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_next_buffer(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_buffer_finder(&[], cx);
            let initial = s.buffer_finder_selected;
            s.buffer_finder_next(cx);
            if s.buffer_finder_filtered.len() > 1 {
                assert_eq!(s.buffer_finder_selected, initial + 1);
            }
        });
    }
}
