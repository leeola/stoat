//! File writing action implementation and tests.
//!
//! Provides functionality to write the current buffer contents to disk. The
//! [`write_file`](crate::Stoat::write_file) action saves the active buffer to its
//! associated file path and updates the saved text baseline.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Write the current buffer to disk.
    ///
    /// Writes the contents of the active buffer to its associated file path on disk.
    /// After a successful write, the buffer's saved text baseline is updated to mark
    /// the buffer as "clean" (no unsaved changes).
    ///
    /// # Workflow
    ///
    /// 1. Verifies a file path is associated with the current buffer
    /// 2. Reads the complete buffer contents
    /// 3. Writes contents to disk at the file path
    /// 4. Updates the saved text baseline to mark buffer as clean
    /// 5. Emits Changed event and triggers UI refresh
    ///
    /// # Behavior
    ///
    /// - Only works when a file path is set (via [`Self::load_file`])
    /// - Overwrites the existing file contents completely
    /// - Updates internal state to mark buffer as "saved"
    /// - Triggers status bar update to clear dirty indicator
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::WriteFile`] action, typically bound to keybindings
    /// like `:w` or `Ctrl-S`. The status bar shows whether the buffer has unsaved
    /// changes based on the saved text baseline updated by this action.
    ///
    /// # Related
    ///
    /// - [`Self::load_file`] - loads file from disk and sets file path
    /// - Status bar dirty indicator - reflects saved state
    ///
    /// # Returns
    ///
    /// `Ok(())` if the write succeeds, or `Err(String)` with an error message if:
    /// - No file path is associated with the current buffer
    /// - The write operation fails
    ///
    /// # Example
    ///
    /// ```ignore
    /// stoat.update(|s, cx| {
    ///     s.write_file(cx).expect("Failed to write file");
    /// });
    /// ```
    pub fn write_file(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        // Get the current file path
        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        // Get buffer content
        let buffer_item = self.active_buffer(cx);
        let content = buffer_item.read(cx).buffer().read(cx).snapshot().text();

        // Write to disk
        std::fs::write(&file_path, &content).map_err(|e| format!("Failed to write file: {}", e))?;

        // Update saved text baseline to mark buffer as clean
        buffer_item.update(cx, |item, _cx| {
            item.set_saved_text(content);
        });

        tracing::info!("Wrote buffer to {:?}", file_path);

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn writes_buffer_to_disk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        // Set file path in test repo
        let file_path = stoat.repo_path().unwrap().join("test.txt");
        stoat.set_file_path(file_path.clone());

        // Write content to buffer using action dispatch
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Hello from Stoat!".to_string()));

        // Call write_file action using dispatch
        stoat.dispatch(WriteFile);

        // Verify file exists on disk
        assert!(file_path.exists(), "File should exist after write");

        // Verify file contents match buffer
        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Hello from Stoat!");
    }

    #[gpui::test]
    #[should_panic(expected = "WriteFile action failed: No file path set for current buffer")]
    fn write_fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        // Write content to buffer but don't set file path
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Hello".to_string()));

        // Call write_file action - should panic
        stoat.dispatch(WriteFile);
    }

    #[gpui::test]
    fn writes_multiline_content(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let file_path = stoat.repo_path().unwrap().join("multiline.txt");
        stoat.set_file_path(file_path.clone());

        // Write multiline content using action dispatch
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Line 1".to_string()));
        stoat.dispatch(NewLine);
        stoat.dispatch(InsertText("Line 2".to_string()));
        stoat.dispatch(NewLine);
        stoat.dispatch(InsertText("Line 3".to_string()));

        // Write to disk using action dispatch
        stoat.dispatch(WriteFile);

        // Verify file contents
        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Line 1\nLine 2\nLine 3");
    }
}
