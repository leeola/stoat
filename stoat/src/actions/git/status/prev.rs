//! Git status prev action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Move to previous file in git status list.
    ///
    /// Decrements the selection index to highlight the previous file in the filtered list,
    /// and loads a diff preview for the newly selected file. The preview is loaded
    /// asynchronously via [`Stoat::load_git_diff_preview`].
    ///
    /// # Behavior
    ///
    /// - Only operates in git_status mode
    /// - Stops at beginning of list (no wrapping)
    /// - Triggers async diff preview load for new selection
    ///
    /// # Related
    ///
    /// - [`Stoat::git_status_next`] - navigate to next file
    /// - [`Stoat::load_git_diff_preview`] - async preview loader
    pub fn git_status_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode != "git_status" {
            return;
        }

        if self.git_status_selected > 0 {
            self.git_status_selected -= 1;
            debug!(selected = self.git_status_selected, "Git status: prev");
            self.load_git_diff_preview(cx);
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
            s.open_git_status(cx);
            if s.mode() == "git_status" && s.git_status_filtered.len() > 1 {
                s.git_status_next(cx);
                let before = s.git_status_selected;
                s.git_status_prev(cx);
                assert!(s.git_status_selected < before);
            }
        });
    }
}
