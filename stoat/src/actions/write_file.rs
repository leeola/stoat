//! File writing action implementation and tests.
//!
//! Provides functionality to write the current buffer contents to disk. The
//! [`write_file`](crate::Stoat::write_file) action saves the active buffer to its
//! associated file path and updates the saved text baseline.

use crate::Stoat;
use gpui::Context;
use std::io::Write;
use text::LineEnding;

/// Convert text line endings to the specified style.
///
/// Takes text that may have mixed or Unix line endings and converts all line
/// endings to the specified style. This ensures consistent line endings when
/// writing files.
///
/// # Arguments
///
/// * `text` - The text to convert
/// * `line_ending` - Target line ending style
///
/// # Returns
///
/// Text with converted line endings
fn convert_line_endings(text: &str, line_ending: LineEnding) -> String {
    let target = line_ending.as_str();

    // Already using target line ending everywhere - no conversion needed
    if !text.contains('\r') && line_ending == LineEnding::Unix {
        return text.to_string();
    }

    // Normalize to Unix first, then convert to target
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");

    if line_ending == LineEnding::Unix {
        normalized
    } else {
        normalized.replace('\n', target)
    }
}

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

        // Get buffer content and line ending
        let buffer_item = self.active_buffer(cx);
        let content = buffer_item.read(cx).buffer().read(cx).snapshot().text();
        let line_ending = buffer_item.read(cx).line_ending();

        // Convert line endings to preserve original file format
        let content_with_line_endings = convert_line_endings(&content, line_ending);

        // Atomic write: write to temp file, then rename
        let parent_dir = file_path
            .parent()
            .ok_or_else(|| "File path has no parent directory".to_string())?;

        let mut tmp_file = tempfile::NamedTempFile::new_in(parent_dir)
            .map_err(|e| format!("Failed to create temp file: {e}"))?;

        tmp_file
            .write_all(content_with_line_endings.as_bytes())
            .map_err(|e| format!("Failed to write to temp file: {e}"))?;

        tmp_file
            .persist(&file_path)
            .map_err(|e| format!("Failed to persist temp file: {e}"))?;

        // Get mtime after successful write
        let mtime = std::fs::metadata(&file_path)
            .ok()
            .and_then(|m| m.modified().ok());

        // Update saved text baseline and mtime to mark buffer as clean
        buffer_item.update(cx, |item, _cx| {
            item.set_saved_text(content);
            if let Some(mtime) = mtime {
                item.set_saved_mtime(mtime);
            }
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

    #[gpui::test]
    fn modifies_buffer_and_writes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("modify_test.txt");
        stoat.set_file_path(file_path.clone());

        // Insert initial text
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Initial".to_string()));
        stoat.dispatch(EnterNormalMode);

        // Modify: append more text
        stoat.dispatch(MoveToLineEnd);
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText(" text here".to_string()));

        // Write to disk
        stoat.dispatch(WriteFile);

        // Verify on disk
        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Initial text here");
    }

    #[gpui::test]
    fn multiple_edits_then_write(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("complex_edit.txt");
        stoat.set_file_path(file_path.clone());

        // Complex editing sequence: insert, delete, move, insert again
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("First".to_string()));
        stoat.dispatch(NewLine);
        stoat.dispatch(InsertText("Second".to_string()));
        stoat.dispatch(NewLine);
        stoat.dispatch(InsertText("Third".to_string()));
        stoat.dispatch(EnterNormalMode);

        // Move and delete a word
        stoat.dispatch(MoveToFileStart);
        stoat.dispatch(MoveWordRight);
        stoat.dispatch(DeleteWordRight);

        // Write to disk
        stoat.dispatch(WriteFile);

        // Verify complex edit result on disk (Second line should be deleted)
        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "First\nThird");
    }

    #[gpui::test]
    fn write_updates_saved_baseline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("baseline_test.txt");
        stoat.set_file_path(file_path.clone());

        // Insert text (buffer becomes dirty)
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Content".to_string()));
        stoat.dispatch(EnterNormalMode);

        // Write to disk (should mark buffer as clean)
        stoat.dispatch(WriteFile);

        // Verify buffer is marked as clean by checking saved text baseline
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                !buffer_item.read(cx).is_modified(cx),
                "Buffer should be clean (not modified) after write"
            );
        });
    }

    #[gpui::test]
    fn write_preserves_existing_content(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("preserve_test.txt");

        // Create file with existing content
        std::fs::write(&file_path, "Existing content").expect("Failed to write initial file");

        // Load the file
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        // Modify the buffer
        stoat.dispatch(EnterNormalMode);
        stoat.dispatch(MoveToLineEnd);
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText(" modified".to_string()));

        // Write back to disk
        stoat.dispatch(WriteFile);

        // Verify file has updated content
        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Existing content modified");
    }

    #[gpui::test]
    fn atomic_write_no_temp_files_left_behind(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("atomic_test.txt");
        let parent_dir = file_path.parent().unwrap();

        stoat.set_file_path(file_path.clone());

        // Write content
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Atomic write test".to_string()));
        stoat.dispatch(WriteFile);

        // Count files in directory - should only be our target file
        let entries: Vec<_> = std::fs::read_dir(parent_dir)
            .expect("Failed to read directory")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().is_file()
                    && e.file_name()
                        .to_str()
                        .map(|s| !s.starts_with('.'))
                        .unwrap_or(false)
            })
            .collect();

        // Should only have our target file, no leftover temp files
        assert_eq!(
            entries.len(),
            1,
            "Expected 1 file (target), found {} files",
            entries.len()
        );
        assert_eq!(entries[0].file_name(), file_path.file_name().unwrap());

        // Verify content is correct
        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Atomic write test");
    }

    #[gpui::test]
    fn detects_conflict_when_file_modified_externally(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("conflict_test.txt");

        // Create initial file
        std::fs::write(&file_path, "Initial content").expect("Failed to create file");

        // Load the file (this sets saved_mtime)
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        // Modify buffer (without saving)
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText(" - buffer change".to_string()));
        stoat.dispatch(EnterNormalMode);

        // Sleep briefly to ensure mtime changes
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Modify file externally
        std::fs::write(&file_path, "External modification")
            .expect("Failed to modify file externally");

        // Check for conflict
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                buffer_item.read(cx).has_conflict(&file_path, cx),
                "Should detect conflict when file modified externally with unsaved buffer changes"
            );
        });
    }

    #[gpui::test]
    fn no_conflict_when_buffer_clean(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("no_conflict_test.txt");

        // Create and load file
        std::fs::write(&file_path, "Initial content").expect("Failed to create file");
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        // Sleep briefly to ensure mtime changes
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Modify file externally (but buffer is clean)
        std::fs::write(&file_path, "External modification")
            .expect("Failed to modify file externally");

        // No conflict because buffer has no unsaved changes
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                !buffer_item.read(cx).has_conflict(&file_path, cx),
                "Should not detect conflict when buffer is clean (no unsaved changes)"
            );
        });
    }

    #[gpui::test]
    fn write_clears_conflict_flag(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("clear_conflict_test.txt");

        // Create and load file
        std::fs::write(&file_path, "Initial content").expect("Failed to create file");
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        // Modify buffer
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText(" - modified".to_string()));
        stoat.dispatch(EnterNormalMode);

        // Sleep and modify externally (create conflict)
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&file_path, "External change").expect("Failed to modify externally");

        // Verify conflict exists
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                buffer_item.read(cx).has_conflict(&file_path, cx),
                "Conflict should exist before write"
            );
        });

        // Write buffer (this should clear conflict by updating mtime)
        stoat.dispatch(WriteFile);

        // Verify conflict is cleared
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                !buffer_item.read(cx).has_conflict(&file_path, cx),
                "Conflict should be cleared after write (buffer clean + mtime updated)"
            );
        });
    }

    #[gpui::test]
    fn preserves_unix_line_endings(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("unix_endings.txt");

        // Create file with Unix line endings
        let unix_content = "Line 1\nLine 2\nLine 3\n";
        std::fs::write(&file_path, unix_content).expect("Failed to create file");

        // Load file (should detect Unix line endings)
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        // Modify buffer
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("New Line\n".to_string()));
        stoat.dispatch(EnterNormalMode);

        // Write back to disk
        stoat.dispatch(WriteFile);

        // Verify line endings are still Unix (LF only)
        let bytes = std::fs::read(&file_path).expect("Failed to read file");
        let contents = String::from_utf8(bytes.clone()).expect("Invalid UTF-8");

        assert!(!contents.contains("\r\n"), "Should not contain CRLF");
        assert!(contents.contains('\n'), "Should contain LF");

        // Verify no carriage returns at all
        assert!(!bytes.contains(&b'\r'), "Should not contain any CR bytes");
    }

    #[gpui::test]
    fn preserves_windows_line_endings(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("windows_endings.txt");

        // Create file with Windows line endings
        let windows_content = "Line 1\r\nLine 2\r\nLine 3\r\n";
        std::fs::write(&file_path, windows_content).expect("Failed to create file");

        // Load file (should detect Windows line endings)
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        // Verify line ending was detected as Windows
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert_eq!(
                buffer_item.read(cx).line_ending(),
                text::LineEnding::Windows,
                "Should detect Windows line endings"
            );
        });

        // Modify buffer (buffer uses \n internally)
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("New Line\n".to_string()));
        stoat.dispatch(EnterNormalMode);

        // Write back to disk (should convert \n to \r\n)
        stoat.dispatch(WriteFile);

        // Verify line endings are still Windows (CRLF)
        let bytes = std::fs::read(&file_path).expect("Failed to read file");
        let contents = String::from_utf8(bytes).expect("Invalid UTF-8");

        assert!(contents.contains("\r\n"), "Should contain CRLF");

        // Count line endings to verify all are CRLF
        let lf_count = contents.matches('\n').count();
        let crlf_count = contents.matches("\r\n").count();
        assert_eq!(
            lf_count, crlf_count,
            "All LF should be preceded by CR (all CRLF)"
        );
    }

    #[gpui::test]
    fn converts_mixed_line_endings_to_detected_style(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("mixed_endings.txt");

        // Create file with Unix line endings (first detected wins)
        let unix_content = "Line 1\nLine 2\nLine 3\n";
        std::fs::write(&file_path, unix_content).expect("Failed to create file");

        // Load file
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        // Simulate mixed line endings in buffer by direct manipulation
        // (in practice this shouldn't happen, but testing the conversion)
        stoat.dispatch(EnterInsertMode);
        stoat.dispatch(InsertText("Mixed\r\nEndings\rHere\n".to_string()));
        stoat.dispatch(EnterNormalMode);

        // Write to disk (should normalize to Unix)
        stoat.dispatch(WriteFile);

        // Verify all line endings are Unix
        let bytes = std::fs::read(&file_path).expect("Failed to read file");
        assert!(
            !bytes.contains(&b'\r'),
            "Should not contain any CR after normalization"
        );
    }
}
