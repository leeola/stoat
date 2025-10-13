//! Buffer item for text editing with syntax highlighting.
//!
//! Wraps [`text::Buffer`] with syntax highlighting tokens from tree-sitter and optional git diff
//! state.

use crate::git_diff::BufferDiff;
use gpui::{App, Entity};
use parking_lot::Mutex;
use std::{collections::HashSet, sync::Arc};
use stoat_rope::{TokenMap, TokenSnapshot};
use stoat_text::{Language, Parser};
use text::{Buffer, BufferSnapshot};

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

    /// Set of expanded diff hunk indices (for showing deleted content inline)
    expanded_hunks: HashSet<usize>,

    /// Saved text content for modification tracking (None for unnamed buffers never saved)
    saved_text: Option<String>,
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
            expanded_hunks: HashSet::new(),
            saved_text: None,
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

    /// Toggle expansion of diff hunk at given row.
    ///
    /// If a hunk exists at the specified row, toggles its expansion state. When expanded,
    /// the hunk's deleted content from git HEAD will be displayed inline above the hunk.
    ///
    /// # Arguments
    ///
    /// * `row` - Buffer row to check for hunk
    /// * `buffer_snapshot` - Buffer snapshot for converting anchors to positions
    ///
    /// # Returns
    ///
    /// `true` if a hunk was toggled, `false` if no hunk exists at this row
    pub fn toggle_diff_hunk_at_row(&mut self, row: u32, buffer_snapshot: &BufferSnapshot) -> bool {
        if let Some(hunk_idx) = self
            .diff
            .as_ref()
            .and_then(|d| d.hunk_for_row(row, buffer_snapshot))
        {
            if self.expanded_hunks.contains(&hunk_idx) {
                self.expanded_hunks.remove(&hunk_idx);
            } else {
                self.expanded_hunks.insert(hunk_idx);
            }
            true
        } else {
            false
        }
    }

    /// Check if a diff hunk at given row is expanded.
    ///
    /// # Arguments
    ///
    /// * `row` - Buffer row to check
    /// * `buffer_snapshot` - Buffer snapshot for converting anchors to positions
    ///
    /// # Returns
    ///
    /// `true` if the hunk at this row is expanded, `false` otherwise
    pub fn is_hunk_expanded_at_row(&self, row: u32, buffer_snapshot: &BufferSnapshot) -> bool {
        self.diff
            .as_ref()
            .and_then(|d| d.hunk_for_row(row, buffer_snapshot))
            .map(|hunk_idx| self.expanded_hunks.contains(&hunk_idx))
            .unwrap_or(false)
    }

    /// Get information about expanded diff hunks for rendering.
    ///
    /// Returns an iterator over tuples of (hunk_index, hunk) for all expanded hunks.
    /// The GUI layer uses this to construct [`DiffBlock`](crate::DiffBlock) structures
    /// for inline display of deleted content.
    ///
    /// # Returns
    ///
    /// Iterator yielding (index, hunk) pairs for expanded hunks
    pub fn expanded_hunks(&self) -> impl Iterator<Item = (usize, &crate::git_diff::DiffHunk)> {
        let hunks = self.diff.as_ref().map(|d| &d.hunks);
        self.expanded_hunks
            .iter()
            .filter_map(move |&idx| hunks.and_then(|h| h.get(idx).map(|hunk| (idx, hunk))))
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
