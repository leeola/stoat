//! Buffer item for text editing with syntax highlighting.
//!
//! Wraps [`text::Buffer`] with syntax highlighting tokens from tree-sitter and optional git diff
//! state.

use crate::git_diff::BufferDiff;
use gpui::{App, Entity};
use parking_lot::Mutex;
use std::{sync::Arc, time::SystemTime};
use stoat_rope::{TokenMap, TokenSnapshot};
use stoat_text::{Language, Parser};
use text::{Buffer, BufferSnapshot, LineEnding};

/// A text buffer with syntax highlighting and git diff support.
///
/// Combines text buffer, token map for syntax highlighting, parser for language support,
/// and optional git diff state for visualization.
pub struct BufferItem {
    /// Text buffer entity
    buffer: Entity<Buffer>,

    /// Syntax highlighting tokens (shared for concurrent access)
    token_map: Arc<Mutex<TokenMap>>,

    /// Tree-sitter parser for current language
    parser: Parser,

    /// Current language setting
    language: Language,

    /// Git diff state (None if not in git repo or diff disabled)
    diff: Option<BufferDiff>,

    /// Saved text content for modification tracking (None for unnamed buffers never saved)
    saved_text: Option<String>,

    /// Modification time when file was last saved (None for unnamed buffers or never saved)
    saved_mtime: Option<SystemTime>,

    /// Line ending style for the buffer (detected on load, preserved on save)
    line_ending: LineEnding,
}

impl BufferItem {
    /// Create a new buffer item.
    ///
    /// Initializes parser for the specified language and creates empty token map.
    pub fn new(buffer: Entity<Buffer>, language: Language, cx: &App) -> Self {
        let buffer_snapshot = buffer.read(cx).snapshot();
        let token_map = Arc::new(Mutex::new(TokenMap::new(&buffer_snapshot)));
        let parser = Parser::new(language).expect("Failed to create parser");

        Self {
            buffer,
            token_map,
            parser,
            language,
            diff: None,
            saved_text: None,
            saved_mtime: None,
            line_ending: LineEnding::default(),
        }
    }

    /// Get reference to the underlying buffer entity.
    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    /// Get a snapshot of the buffer state.
    pub fn buffer_snapshot(&self, cx: &App) -> BufferSnapshot {
        self.buffer.read(cx).snapshot()
    }

    /// Create a display buffer with phantom rows for git diffs.
    ///
    /// Returns a [`DisplayBuffer`](crate::DisplayBuffer) that includes both real buffer rows
    /// and optionally phantom rows for deleted content from git diffs. This is used by the
    /// rendering layer to display diffs inline with appropriate styling.
    ///
    /// # Arguments
    ///
    /// * `cx` - Application context for reading buffer state
    /// * `show_phantom_rows` - Whether to show phantom deleted rows (false in normal mode, true in
    ///   review mode)
    ///
    /// # Returns
    ///
    /// A [`DisplayBuffer`](crate::DisplayBuffer) with all rows (real + optionally phantom) built
    ///
    /// # Related
    ///
    /// - [`DisplayBuffer`](crate::DisplayBuffer) - The display buffer abstraction
    /// - [`diff`](#method.diff) - Get the current git diff state
    pub fn display_buffer(&self, cx: &App, show_phantom_rows: bool) -> crate::DisplayBuffer {
        crate::DisplayBuffer::new(
            self.buffer_snapshot(cx),
            self.diff.clone(),
            show_phantom_rows,
        )
    }

    /// Get a snapshot of syntax highlighting tokens.
    pub fn token_snapshot(&self) -> TokenSnapshot {
        self.token_map.lock().snapshot()
    }

    /// Get current language.
    pub fn language(&self) -> Language {
        self.language
    }

    /// Reparse buffer content and update syntax highlighting tokens.
    ///
    /// Should be called after buffer edits to keep tokens in sync.
    pub fn reparse(&mut self, cx: &App) -> Result<(), String> {
        let contents = self.buffer.read(cx).text();
        let buffer_snapshot = self.buffer.read(cx).snapshot();

        match self.parser.parse(&contents, &buffer_snapshot) {
            Ok(tokens) => {
                self.token_map
                    .lock()
                    .replace_tokens(tokens, &buffer_snapshot);
                Ok(())
            },
            Err(e) => {
                tracing::debug!("Failed to parse buffer: {}", e);
                Err(format!("Parse error: {e}"))
            },
        }
    }

    /// Change the language and reinitialize parser.
    ///
    /// Call [`reparse`] after to regenerate tokens.
    pub fn set_language(&mut self, language: Language) {
        if language != self.language {
            self.language = language;
            self.parser = Parser::new(language).expect("Failed to create parser");
        }
    }

    /// Get the git diff state for this buffer.
    ///
    /// Returns [`None`] if the file is not in a git repository or if diff
    /// computation hasn't been performed yet.
    pub fn diff(&self) -> Option<&BufferDiff> {
        self.diff.as_ref()
    }

    /// Set the git diff state for this buffer.
    ///
    /// Call this after computing the diff between HEAD and the current buffer state.
    /// Pass [`None`] to clear the diff state.
    ///
    /// All diff hunks are always visible as phantom rows in the display buffer.
    pub fn set_diff(&mut self, diff: Option<BufferDiff>) {
        self.diff = diff;
    }

    /// Check if the buffer has unsaved modifications.
    ///
    /// Compares current buffer text with saved baseline. Returns `true` if the buffer
    /// has been modified since last save, `false` otherwise. Always returns `false`
    /// if no saved text baseline exists (unnamed buffers).
    pub fn is_modified(&self, cx: &App) -> bool {
        if let Some(saved) = &self.saved_text {
            let current = self.buffer.read(cx).text();
            current != *saved
        } else {
            false
        }
    }

    /// Set the saved text baseline for modification tracking.
    ///
    /// Call this after saving a file or loading file content to establish
    /// the baseline for detecting modifications.
    pub fn set_saved_text(&mut self, text: String) {
        self.saved_text = Some(text);
    }

    /// Set the saved modification time baseline for conflict detection.
    ///
    /// Call this after saving a file to establish the baseline for detecting
    /// external modifications. The mtime represents the modification time on
    /// disk when the file was last saved.
    ///
    /// # Related
    ///
    /// - [`has_conflict`](#method.has_conflict) - Detect concurrent modifications
    pub fn set_saved_mtime(&mut self, mtime: SystemTime) {
        self.saved_mtime = Some(mtime);
    }

    /// Get the line ending style for this buffer.
    ///
    /// Returns the detected line ending from when the file was loaded,
    /// or the platform default for new buffers.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Set the line ending style for this buffer.
    ///
    /// Called by [`Stoat::load_file`](crate::Stoat::load_file) after detecting
    /// the line ending style from file contents. The detected style is preserved
    /// when saving via [`Stoat::write_file`](crate::Stoat::write_file).
    ///
    /// # Arguments
    ///
    /// * `line_ending` - The line ending style to use
    ///
    /// # Related
    ///
    /// - [`line_ending`](#method.line_ending) - Get the current line ending
    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.line_ending = line_ending;
    }

    /// Check if the file has been modified externally since last save.
    ///
    /// Compares the current file modification time on disk with the saved mtime baseline.
    /// Returns `true` if the file has been modified externally while this buffer also has
    /// unsaved changes, indicating a conflict that may result in data loss if the buffer
    /// is saved.
    ///
    /// # Arguments
    ///
    /// * `file_path` - Path to the file to check for conflicts
    /// * `cx` - Application context for reading buffer state
    ///
    /// # Returns
    ///
    /// `true` if there's a conflict (file modified externally + buffer has unsaved changes),
    /// `false` otherwise
    ///
    /// # Conflict Detection Logic
    ///
    /// A conflict exists when all of these conditions are met:
    /// 1. File exists on disk
    /// 2. We have a saved mtime baseline (file was previously saved)
    /// 3. File's current mtime is newer than our saved mtime
    /// 4. Buffer has unsaved modifications
    ///
    /// # Related
    ///
    /// - [`set_saved_mtime`](#method.set_saved_mtime) - Set the baseline mtime
    /// - [`is_modified`](#method.is_modified) - Check for unsaved changes
    pub fn has_conflict(&self, file_path: &std::path::Path, cx: &App) -> bool {
        // Only a conflict if we have unsaved changes
        if !self.is_modified(cx) {
            return false;
        }

        // Need a saved mtime to compare against
        let Some(saved_mtime) = self.saved_mtime else {
            return false;
        };

        // Get current mtime from disk
        let Ok(metadata) = std::fs::metadata(file_path) else {
            return false;
        };

        let Ok(current_mtime) = metadata.modified() else {
            return false;
        };

        // Conflict if file modified since our last save
        current_mtime > saved_mtime
    }

    /// Get the base text for a specific diff hunk.
    ///
    /// Returns the deleted content from git HEAD for the specified hunk index.
    /// Used by the GUI layer to display deleted lines inline.
    ///
    /// # Arguments
    ///
    /// * `hunk_idx` - Index of the hunk
    ///
    /// # Returns
    ///
    /// String slice of the base text, or empty string if hunk doesn't exist
    pub fn base_text_for_hunk(&self, hunk_idx: usize) -> &str {
        self.diff
            .as_ref()
            .map(|d| d.base_text_for_hunk(hunk_idx))
            .unwrap_or("")
    }
}
