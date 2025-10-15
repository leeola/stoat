//! File finder select action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Select file in finder.
    ///
    /// Loads the currently selected file from the file finder into the active buffer.
    /// Builds the absolute path from the worktree root and uses [`Stoat::load_file`]
    /// to ensure git diff computation. Automatically dismisses the file finder after
    /// selection.
    ///
    /// # Workflow
    ///
    /// 1. Gets selected file path from filtered list
    /// 2. Builds absolute path from worktree root
    /// 3. Updates current file path for status bar display
    /// 4. Loads file using [`Stoat::load_file`] (triggers diff computation)
    /// 5. Dismisses file finder via [`Stoat::file_finder_dismiss`]
    ///
    /// # Behavior
    ///
    /// - Only operates in file_finder mode
    /// - Logs error if file load fails (doesn't block dismissal)
    /// - Always dismisses finder after selection attempt
    ///
    /// # Related
    ///
    /// - [`Stoat::load_file`] - file loading with diff computation
    /// - [`Stoat::file_finder_dismiss`] - cleanup and mode restoration
    pub fn file_finder_select(&mut self, cx: &mut Context<Self>) {
        if self.mode != "file_finder" {
            return;
        }

        if self.file_finder_selected < self.file_finder_filtered.len() {
            let relative_path = &self.file_finder_filtered[self.file_finder_selected];
            debug!(file = ?relative_path, "File finder: select");

            // Build absolute path
            let root = self.worktree.lock().snapshot().root().to_path_buf();
            let abs_path = root.join(relative_path);

            // Store file path for status bar
            self.current_file_path = Some(relative_path.clone());

            // Load file (uses load_file to ensure diff computation)
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::error!("Failed to load file {:?}: {}", abs_path, e);
            }
        }

        self.file_finder_dismiss(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn selects_file(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.open_file_finder(cx);
            s.file_finder_select(cx);
            // Dismiss clears state but doesn't change mode (SetKeyContext handles mode transitions)
            assert!(s.file_finder_input.is_none());
        });
    }
}
