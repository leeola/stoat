//! File writing action implementation and tests.
//!
//! Provides functionality to write buffer contents to disk. The
//! [`write_file`](crate::Stoat::write_file) action saves the active buffer to its
//! associated file path, and [`write_all`](crate::Stoat::write_all) saves all
//! modified buffers with file paths.

use crate::{buffer::item::BufferItem, Stoat};
use gpui::{Context, Entity};
use std::{io::Write, path::PathBuf};
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

/// Write a single buffer to disk with all safety mechanisms.
///
/// Performs atomic write to prevent corruption, updates modification time tracking,
/// and converts line endings to the buffer's detected style. This is the core
/// implementation used by both [`Stoat::write_file`] and [`Stoat::write_all`].
///
/// # Safety Mechanisms
///
/// 1. **Atomic Write**: Uses tempfile pattern - writes to temp file then renames
/// 2. **mtime Tracking**: Updates saved_mtime after write for conflict detection
/// 3. **Line Ending Preservation**: Converts to buffer's detected line ending style
///
/// # Arguments
///
/// * `buffer_item` - The buffer to write
/// * `file_path` - Destination path on disk
/// * `cx` - Context for reading buffer and emitting events
///
/// # Returns
///
/// `Ok(())` if write succeeds, or `Err(String)` if the write operation fails
fn write_file_internal(
    buffer_item: &Entity<BufferItem>,
    file_path: &PathBuf,
    cx: &mut Context<Stoat>,
) -> Result<(), String> {
    // Get buffer content and line ending
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
        .persist(file_path)
        .map_err(|e| format!("Failed to persist temp file: {e}"))?;

    // Get mtime after successful write
    let mtime = std::fs::metadata(file_path)
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

    Ok(())
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

        // Get active buffer
        let buffer_item = self.active_buffer(cx);

        // Write using shared implementation
        write_file_internal(&buffer_item, &file_path, cx)?;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();

        Ok(())
    }

    /// Write all modified buffers to disk.
    ///
    /// Iterates through all open buffers and writes each modified buffer that has
    /// an associated file path. Uses the same safety mechanisms as [`Self::write_file`]
    /// for each buffer: atomic writes, mtime tracking, and line ending preservation.
    ///
    /// # Workflow
    ///
    /// 1. Gets all buffer IDs from [`crate::buffer::store::BufferStore`]
    /// 2. For each buffer:
    ///    - Skips buffers without file paths (unnamed/scratch buffers)
    ///    - Skips buffers that are not modified (already saved)
    ///    - Writes modified buffers using [`write_file_internal`]
    /// 3. Emits Changed event and triggers UI refresh
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::WriteAll`] action, typically bound to keybindings
    /// like `:wa` or `:wall`. Each buffer is written independently - failure to write
    /// one buffer doesn't prevent others from being written.
    ///
    /// # Related
    ///
    /// - [`Self::write_file`] - writes only the active buffer
    /// - [`write_file_internal`] - core write implementation with safety mechanisms
    ///
    /// # Returns
    ///
    /// `Ok(())` if all writes succeed, or `Err(String)` with an error message for
    /// the first buffer that fails to write
    pub fn write_all(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        // Collect buffer IDs that need writing (modified + have file path)
        let buffer_ids: Vec<_> = self
            .buffer_store
            .read(cx)
            .buffer_ids_by_activation()
            .to_vec();

        let mut wrote_any = false;

        // Collect (buffer_item, path) pairs for all modified buffers with paths
        let buffers_to_write: Vec<_> = {
            let buffer_store = self.buffer_store.read(cx);
            buffer_ids
                .iter()
                .filter_map(|&buffer_id| {
                    let buffer_item = buffer_store.get_buffer(buffer_id)?;
                    let file_path = buffer_store.get_path(buffer_id)?.clone();
                    Some((buffer_item, file_path))
                })
                .collect()
        };

        // Write each modified buffer
        for (buffer_item, file_path) in buffers_to_write {
            // Skip buffers that are not modified
            if !buffer_item.read(cx).is_modified(cx) {
                continue;
            }

            // Write buffer using shared implementation
            write_file_internal(&buffer_item, &file_path, cx)?;
            wrote_any = true;
        }

        if wrote_any {
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn writes_buffer_to_disk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let file_path = stoat.repo_path().unwrap().join("test.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Hello from Stoat!", cx);
            s.write_file(cx).unwrap();
        });

        assert!(file_path.exists(), "File should exist after write");
        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Hello from Stoat!");
    }

    #[gpui::test]
    #[should_panic(expected = "No file path set for current buffer")]
    fn write_fails_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.write_file(cx).unwrap();
        });
    }

    #[gpui::test]
    fn writes_multiline_content(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();

        let file_path = stoat.repo_path().unwrap().join("multiline.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.write_file(cx).unwrap();
        });

        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Line 1\nLine 2\nLine 3");
    }

    #[gpui::test]
    fn modifies_buffer_and_writes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("modify_test.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Initial text here", cx);
            s.write_file(cx).unwrap();
        });

        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Initial text here");
    }

    #[gpui::test]
    fn multiple_edits_then_write(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("complex_edit.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("First\nSecond\nThird", cx);
            s.enter_normal_mode(cx);
            s.move_to_file_start(cx);
            s.move_word_right(cx);
            s.delete_word_right(cx);
            s.write_file(cx).unwrap();
        });

        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "First\nThird");
    }

    #[gpui::test]
    fn write_updates_saved_baseline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("baseline_test.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Content", cx);
            s.write_file(cx).unwrap();
        });

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

        std::fs::write(&file_path, "Existing content").expect("Failed to write initial file");

        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
            s.move_to_line_end(cx);
            s.insert_text(" modified", cx);
            s.write_file(cx).unwrap();
        });

        let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
        assert_eq!(contents, "Existing content modified");
    }

    #[gpui::test]
    fn atomic_write_no_temp_files_left_behind(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let file_path = stoat.repo_path().unwrap().join("atomic_test.txt");
        let parent_dir = file_path.parent().unwrap();

        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Atomic write test", cx);
            s.write_file(cx).unwrap();
        });

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

        stoat.update(|s, cx| s.insert_text(" - buffer change", cx));

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

        stoat.update(|s, cx| s.insert_text(" - modified", cx));

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

        stoat.update(|s, cx| s.write_file(cx).unwrap());

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

        stoat.update(|s, cx| {
            s.insert_text("New Line\n", cx);
            s.write_file(cx).unwrap();
        });

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

        stoat.update(|s, cx| {
            s.insert_text("New Line\n", cx);
            s.write_file(cx).unwrap();
        });

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

        stoat.update(|s, cx| {
            s.insert_text("Mixed\r\nEndings\rHere\n", cx);
            s.write_file(cx).unwrap();
        });

        // Verify all line endings are Unix
        let bytes = std::fs::read(&file_path).expect("Failed to read file");
        assert!(
            !bytes.contains(&b'\r'),
            "Should not contain any CR after normalization"
        );
    }

    #[gpui::test]
    fn write_all_saves_multiple_modified_buffers(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap().to_path_buf();

        // Create and modify multiple buffers
        let file1 = repo_path.join("file1.txt");
        let file2 = repo_path.join("file2.txt");
        let file3 = repo_path.join("file3.txt");

        // Create initial files
        std::fs::write(&file1, "Initial 1").unwrap();
        std::fs::write(&file2, "Initial 2").unwrap();
        std::fs::write(&file3, "Initial 3").unwrap();

        for file in [&file1, &file2, &file3] {
            stoat.update(|s, cx| {
                s.load_file(file, cx).unwrap();
                s.move_to_line_end(cx);
                s.insert_text(" - modified", cx);
            });
        }

        stoat.update(|s, cx| s.write_all(cx).unwrap());

        // Verify all files were written
        assert_eq!(
            std::fs::read_to_string(&file1).unwrap(),
            "Initial 1 - modified"
        );
        assert_eq!(
            std::fs::read_to_string(&file2).unwrap(),
            "Initial 2 - modified"
        );
        assert_eq!(
            std::fs::read_to_string(&file3).unwrap(),
            "Initial 3 - modified"
        );
    }

    #[gpui::test]
    fn write_all_skips_clean_buffers(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap().to_path_buf();

        let file1 = repo_path.join("clean.txt");
        let file2 = repo_path.join("modified.txt");

        // Create initial files
        std::fs::write(&file1, "Clean content").unwrap();
        std::fs::write(&file2, "Initial").unwrap();

        // Load first file (will remain clean)
        stoat.update(|s, cx| {
            s.load_file(&file1, cx).unwrap();
        });

        stoat.update(|s, cx| {
            s.load_file(&file2, cx).unwrap();
            s.move_to_line_end(cx);
            s.insert_text(" - modified", cx);
        });

        // Get mtime of clean file before write_all
        let file1_mtime_before = std::fs::metadata(&file1).unwrap().modified().unwrap();

        // Sleep to ensure mtime would change if file was written
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Write all
        stoat.update(|s, cx| {
            s.write_all(cx).unwrap();
        });

        // Verify clean file was not rewritten (mtime unchanged)
        let file1_mtime_after = std::fs::metadata(&file1).unwrap().modified().unwrap();
        assert_eq!(
            file1_mtime_before, file1_mtime_after,
            "Clean buffer should not be rewritten"
        );

        // Verify modified file was written
        assert_eq!(
            std::fs::read_to_string(&file2).unwrap(),
            "Initial - modified"
        );
    }

    #[gpui::test]
    fn write_all_marks_all_buffers_clean(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap().to_path_buf();

        let file1 = repo_path.join("buffer1.txt");
        let file2 = repo_path.join("buffer2.txt");

        std::fs::write(&file1, "").unwrap();
        std::fs::write(&file2, "").unwrap();

        stoat.update(|s, cx| {
            s.load_file(&file1, cx).unwrap();
            s.insert_text("Content 1", cx);
        });
        stoat.update(|s, cx| {
            s.load_file(&file2, cx).unwrap();
            s.insert_text("Content 2", cx);
        });

        stoat.update(|s, cx| s.write_all(cx).unwrap());

        // Verify both buffers are marked as clean
        stoat.update(|s, cx| {
            let buffer_store = s.buffer_store.read(cx);
            for buffer_id in buffer_store.buffer_ids_by_activation() {
                if let Some(buffer_item) = buffer_store.get_buffer(*buffer_id) {
                    assert!(
                        !buffer_item.read(cx).is_modified(cx),
                        "All buffers should be clean after write_all"
                    );
                }
            }
        });
    }

    #[gpui::test]
    fn write_all_uses_atomic_writes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap().to_path_buf();

        let file1 = repo_path.join("atomic1.txt");
        let file2 = repo_path.join("atomic2.txt");

        std::fs::write(&file1, "").unwrap();
        std::fs::write(&file2, "").unwrap();

        stoat.update(|s, cx| {
            s.load_file(&file1, cx).unwrap();
            s.insert_text("Atomic 1", cx);
        });
        stoat.update(|s, cx| {
            s.load_file(&file2, cx).unwrap();
            s.insert_text("Atomic 2", cx);
        });

        stoat.update(|s, cx| s.write_all(cx).unwrap());

        // Verify no temp files left behind
        let entries: Vec<_> = std::fs::read_dir(&repo_path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().is_file()
                    && e.file_name()
                        .to_str()
                        .map(|s| !s.starts_with('.'))
                        .unwrap_or(false)
            })
            .collect();

        // Should only have our two target files
        assert_eq!(
            entries.len(),
            2,
            "Should have exactly 2 files (no temp files)"
        );

        // Verify content is correct
        assert_eq!(std::fs::read_to_string(&file1).unwrap(), "Atomic 1");
        assert_eq!(std::fs::read_to_string(&file2).unwrap(), "Atomic 2");
    }

    #[gpui::test]
    fn write_all_preserves_line_endings_per_buffer(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap().to_path_buf();

        let unix_file = repo_path.join("unix.txt");
        let windows_file = repo_path.join("windows.txt");

        // Create files with different line endings
        std::fs::write(&unix_file, "Line 1\nLine 2\n").unwrap();
        std::fs::write(&windows_file, "Line 1\r\nLine 2\r\n").unwrap();

        stoat.update(|s, cx| {
            s.load_file(&unix_file, cx).unwrap();
            s.insert_text("New\n", cx);
        });
        stoat.update(|s, cx| {
            s.load_file(&windows_file, cx).unwrap();
            s.insert_text("New\n", cx);
        });

        stoat.update(|s, cx| s.write_all(cx).unwrap());

        // Verify Unix file still has Unix line endings
        let unix_bytes = std::fs::read(&unix_file).unwrap();
        assert!(
            !unix_bytes.contains(&b'\r'),
            "Unix file should have LF only"
        );

        // Verify Windows file still has Windows line endings
        let windows_content = std::fs::read_to_string(&windows_file).unwrap();
        assert!(
            windows_content.contains("\r\n"),
            "Windows file should have CRLF"
        );
        let lf_count = windows_content.matches('\n').count();
        let crlf_count = windows_content.matches("\r\n").count();
        assert_eq!(
            lf_count, crlf_count,
            "All LF should be CRLF in Windows file"
        );
    }
}
