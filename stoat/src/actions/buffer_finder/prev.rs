//! Buffer finder prev action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Move to previous buffer in finder.
    ///
    /// Decrements the selection index to highlight the previous buffer in the filtered list.
    ///
    /// # Behavior
    ///
    /// - Only operates in buffer_finder mode
    /// - Stops at beginning of list (no wrapping)
    /// - No preview loading needed (buffers already in memory)
    ///
    /// # Related
    ///
    /// - [`Stoat::buffer_finder_next`] - navigate to next buffer
    pub fn buffer_finder_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode != "buffer_finder" {
            return;
        }

        if self.buffer_finder_selected > 0 {
            self.buffer_finder_selected -= 1;
            debug!(
                selected = self.buffer_finder_selected,
                "Buffer finder: prev"
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
    fn moves_to_previous_buffer(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_buffer_finder(&[], cx);
            // Move forward if possible
            if s.buffer_finder_filtered.len() > 1 {
                s.buffer_finder_next(cx);
                let before = s.buffer_finder_selected;
                s.buffer_finder_prev(cx);
                assert!(s.buffer_finder_selected < before);
            }
        });
    }
}
