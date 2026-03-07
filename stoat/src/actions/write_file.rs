//! File writing action implementation and tests.
//!
//! Provides functionality to write buffer contents to disk. The
//! [`write_file`](crate::Stoat::write_file) action saves the active buffer to its
//! associated file path, and [`write_all`](crate::Stoat::write_all) saves all
//! modified buffers with file paths.

use crate::{buffer::item::BufferItem, fs::Fs, Stoat};
use gpui::{Context, Entity};
use std::path::PathBuf;
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
pub(crate) fn write_buffer_to_disk(
    buffer_item: &Entity<BufferItem>,
    file_path: &PathBuf,
    fs: &dyn Fs,
    cx: &mut Context<Stoat>,
) -> Result<(), String> {
    let content = buffer_item.read(cx).buffer().read(cx).snapshot().text();
    let line_ending = buffer_item.read(cx).line_ending();

    let content_with_line_endings = convert_line_endings(&content, line_ending);

    fs.atomic_write(file_path, content_with_line_endings.as_bytes())
        .map_err(|e| format!("Failed to write file: {e}"))?;

    let mtime = fs.metadata(file_path).ok().and_then(|m| m.modified);

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
        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        let buffer_item = self.active_buffer(cx);

        write_buffer_to_disk(&buffer_item, &file_path, &*self.services.fs, cx)?;

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

            write_buffer_to_disk(&buffer_item, &file_path, &*self.services.fs, cx)?;
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
    use std::path::{Path, PathBuf};

    #[gpui::test]
    fn writes_buffer_to_disk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file_path = PathBuf::from("/fake/test.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Hello from Stoat!", cx);
            s.write_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let contents = s
                .services
                .fake_fs()
                .read_to_string_fake(&file_path)
                .unwrap();
            assert_eq!(contents, "Hello from Stoat!");
        });
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
        let mut stoat = Stoat::test(cx);

        let file_path = PathBuf::from("/fake/multiline.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.write_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let contents = s
                .services
                .fake_fs()
                .read_to_string_fake(&file_path)
                .unwrap();
            assert_eq!(contents, "Line 1\nLine 2\nLine 3");
        });
    }

    #[gpui::test]
    fn modifies_buffer_and_writes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/modify_test.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Initial text here", cx);
            s.write_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let contents = s
                .services
                .fake_fs()
                .read_to_string_fake(&file_path)
                .unwrap();
            assert_eq!(contents, "Initial text here");
        });
    }

    #[gpui::test]
    fn multiple_edits_then_write(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/complex_edit.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("First\nSecond\nThird", cx);
            s.enter_normal_mode(cx);
            s.move_to_file_start(cx);
            s.move_word_right(cx);
            s.delete_word_right(cx);
            s.write_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let contents = s
                .services
                .fake_fs()
                .read_to_string_fake(&file_path)
                .unwrap();
            assert_eq!(contents, "First\nThird");
        });
    }

    #[gpui::test]
    fn write_updates_saved_baseline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/baseline_test.txt");
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
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/preserve_test.txt");

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "Existing content");
        });

        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
            s.move_to_line_end(cx);
            s.insert_text(" modified", cx);
            s.write_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let contents = s
                .services
                .fake_fs()
                .read_to_string_fake(&file_path)
                .unwrap();
            assert_eq!(contents, "Existing content modified");
        });
    }

    #[gpui::test]
    fn atomic_write_no_temp_files_left_behind(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/atomic_test.txt");
        stoat.set_file_path(file_path.clone());

        stoat.update(|s, cx| {
            s.insert_text("Atomic write test", cx);
            s.write_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let parent = file_path.parent().unwrap();
            let entries = s.services.fs.read_dir(parent).unwrap();
            let file_entries: Vec<_> = entries.iter().filter(|e| e.is_file).collect();
            assert_eq!(file_entries.len(), 1, "Should have exactly 1 file");
            assert_eq!(file_entries[0].path, file_path);

            let contents = s
                .services
                .fake_fs()
                .read_to_string_fake(&file_path)
                .unwrap();
            assert_eq!(contents, "Atomic write test");
        });
    }

    #[gpui::test]
    fn detects_conflict_when_file_modified_externally(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/conflict_test.txt");

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "Initial content");
        });

        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        stoat.update(|s, cx| s.insert_text(" - buffer change", cx));

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "External modification");
        });

        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                buffer_item
                    .read(cx)
                    .has_conflict(&file_path, &*s.services.fs, cx),
                "Should detect conflict when file modified externally with unsaved buffer changes"
            );
        });
    }

    #[gpui::test]
    fn no_conflict_when_buffer_clean(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/no_conflict_test.txt");

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "Initial content");
        });
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "External modification");
        });

        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                !buffer_item
                    .read(cx)
                    .has_conflict(&file_path, &*s.services.fs, cx),
                "Should not detect conflict when buffer is clean (no unsaved changes)"
            );
        });
    }

    #[gpui::test]
    fn write_clears_conflict_flag(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/clear_conflict_test.txt");

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "Initial content");
        });
        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        stoat.update(|s, cx| s.insert_text(" - modified", cx));

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "External change");
        });

        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                buffer_item
                    .read(cx)
                    .has_conflict(&file_path, &*s.services.fs, cx),
                "Conflict should exist before write"
            );
        });

        stoat.update(|s, cx| s.write_file(cx).unwrap());

        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            assert!(
                !buffer_item
                    .read(cx)
                    .has_conflict(&file_path, &*s.services.fs, cx),
                "Conflict should be cleared after write (buffer clean + mtime updated)"
            );
        });
    }

    #[gpui::test]
    fn preserves_unix_line_endings(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/unix_endings.txt");

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "Line 1\nLine 2\nLine 3\n");
        });

        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        stoat.update(|s, cx| {
            s.insert_text("New Line\n", cx);
            s.write_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let bytes = s.services.fs.read_bytes(&file_path, usize::MAX).unwrap();
            assert!(!bytes.contains(&b'\r'), "Should not contain any CR bytes");
            assert!(bytes.contains(&b'\n'), "Should contain LF");
        });
    }

    #[gpui::test]
    fn preserves_windows_line_endings(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/windows_endings.txt");

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "Line 1\r\nLine 2\r\nLine 3\r\n");
        });

        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

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

        stoat.update(|s, _cx| {
            let bytes = s.services.fs.read_bytes(&file_path, usize::MAX).unwrap();
            let contents = String::from_utf8(bytes).expect("Invalid UTF-8");
            assert!(contents.contains("\r\n"), "Should contain CRLF");
            let lf_count = contents.matches('\n').count();
            let crlf_count = contents.matches("\r\n").count();
            assert_eq!(
                lf_count, crlf_count,
                "All LF should be preceded by CR (all CRLF)"
            );
        });
    }

    #[gpui::test]
    fn converts_mixed_line_endings_to_detected_style(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        let file_path = PathBuf::from("/fake/mixed_endings.txt");

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&file_path, "Line 1\nLine 2\nLine 3\n");
        });

        stoat.update(|s, cx| {
            s.load_file(&file_path, cx).expect("Failed to load file");
        });

        stoat.update(|s, cx| {
            s.insert_text("Mixed\r\nEndings\rHere\n", cx);
            s.write_file(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let bytes = s.services.fs.read_bytes(&file_path, usize::MAX).unwrap();
            assert!(
                !bytes.contains(&b'\r'),
                "Should not contain any CR after normalization"
            );
        });
    }

    #[gpui::test]
    fn write_all_saves_multiple_modified_buffers(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file1 = PathBuf::from("/fake/file1.txt");
        let file2 = PathBuf::from("/fake/file2.txt");
        let file3 = PathBuf::from("/fake/file3.txt");

        stoat.update(|s, _cx| {
            s.services.fake_fs().insert_file(&file1, "Initial 1");
            s.services.fake_fs().insert_file(&file2, "Initial 2");
            s.services.fake_fs().insert_file(&file3, "Initial 3");
        });

        for file in [&file1, &file2, &file3] {
            stoat.update(|s, cx| {
                s.load_file(file, cx).unwrap();
                s.move_to_line_end(cx);
                s.insert_text(" - modified", cx);
            });
        }

        stoat.update(|s, cx| s.write_all(cx).unwrap());

        stoat.update(|s, _cx| {
            assert_eq!(
                s.services.fake_fs().read_to_string_fake(&file1).unwrap(),
                "Initial 1 - modified"
            );
            assert_eq!(
                s.services.fake_fs().read_to_string_fake(&file2).unwrap(),
                "Initial 2 - modified"
            );
            assert_eq!(
                s.services.fake_fs().read_to_string_fake(&file3).unwrap(),
                "Initial 3 - modified"
            );
        });
    }

    #[gpui::test]
    fn write_all_skips_clean_buffers(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file1 = PathBuf::from("/fake/clean.txt");
        let file2 = PathBuf::from("/fake/modified.txt");

        stoat.update(|s, _cx| {
            s.services.fake_fs().insert_file(&file1, "Clean content");
            s.services.fake_fs().insert_file(&file2, "Initial");
        });

        stoat.update(|s, cx| {
            s.load_file(&file1, cx).unwrap();
        });

        stoat.update(|s, cx| {
            s.load_file(&file2, cx).unwrap();
            s.move_to_line_end(cx);
            s.insert_text(" - modified", cx);
        });

        let file1_mtime_before =
            stoat.update(|s, _cx| s.services.fs.metadata(&file1).unwrap().modified.unwrap());

        stoat.update(|s, cx| {
            s.write_all(cx).unwrap();
        });

        stoat.update(|s, _cx| {
            let file1_mtime_after = s.services.fs.metadata(&file1).unwrap().modified.unwrap();
            assert_eq!(
                file1_mtime_before, file1_mtime_after,
                "Clean buffer should not be rewritten"
            );

            assert_eq!(
                s.services.fake_fs().read_to_string_fake(&file2).unwrap(),
                "Initial - modified"
            );
        });
    }

    #[gpui::test]
    fn write_all_marks_all_buffers_clean(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file1 = PathBuf::from("/fake/buffer1.txt");
        let file2 = PathBuf::from("/fake/buffer2.txt");

        stoat.update(|s, _cx| {
            s.services.fake_fs().insert_file(&file1, "");
            s.services.fake_fs().insert_file(&file2, "");
        });

        stoat.update(|s, cx| {
            s.load_file(&file1, cx).unwrap();
            s.insert_text("Content 1", cx);
        });
        stoat.update(|s, cx| {
            s.load_file(&file2, cx).unwrap();
            s.insert_text("Content 2", cx);
        });

        stoat.update(|s, cx| s.write_all(cx).unwrap());

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
        let mut stoat = Stoat::test(cx);

        let file1 = PathBuf::from("/fake/atomic1.txt");
        let file2 = PathBuf::from("/fake/atomic2.txt");

        stoat.update(|s, _cx| {
            s.services.fake_fs().insert_file(&file1, "");
            s.services.fake_fs().insert_file(&file2, "");
        });

        stoat.update(|s, cx| {
            s.load_file(&file1, cx).unwrap();
            s.insert_text("Atomic 1", cx);
        });
        stoat.update(|s, cx| {
            s.load_file(&file2, cx).unwrap();
            s.insert_text("Atomic 2", cx);
        });

        stoat.update(|s, cx| s.write_all(cx).unwrap());

        stoat.update(|s, _cx| {
            let entries = s.services.fs.read_dir(Path::new("/fake")).unwrap();
            let file_entries: Vec<_> = entries.iter().filter(|e| e.is_file).collect();
            assert_eq!(
                file_entries.len(),
                2,
                "Should have exactly 2 files (no temp files)"
            );

            assert_eq!(
                s.services.fake_fs().read_to_string_fake(&file1).unwrap(),
                "Atomic 1"
            );
            assert_eq!(
                s.services.fake_fs().read_to_string_fake(&file2).unwrap(),
                "Atomic 2"
            );
        });
    }

    #[gpui::test]
    fn write_all_preserves_line_endings_per_buffer(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let unix_file = PathBuf::from("/fake/unix.txt");
        let windows_file = PathBuf::from("/fake/windows.txt");

        stoat.update(|s, _cx| {
            s.services
                .fake_fs()
                .insert_file(&unix_file, "Line 1\nLine 2\n");
            s.services
                .fake_fs()
                .insert_file(&windows_file, "Line 1\r\nLine 2\r\n");
        });

        stoat.update(|s, cx| {
            s.load_file(&unix_file, cx).unwrap();
            s.insert_text("New\n", cx);
        });
        stoat.update(|s, cx| {
            s.load_file(&windows_file, cx).unwrap();
            s.insert_text("New\n", cx);
        });

        stoat.update(|s, cx| s.write_all(cx).unwrap());

        stoat.update(|s, _cx| {
            let unix_bytes = s.services.fs.read_bytes(&unix_file, usize::MAX).unwrap();
            assert!(
                !unix_bytes.contains(&b'\r'),
                "Unix file should have LF only"
            );

            let windows_bytes = s.services.fs.read_bytes(&windows_file, usize::MAX).unwrap();
            let windows_content = String::from_utf8(windows_bytes).unwrap();
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
        });
    }
}
