//! Buffer item for text editing with syntax highlighting.
//!
//! Wraps [`text::Buffer`] with syntax highlighting tokens from tree-sitter and optional git diff
//! state.

use crate::git_diff::BufferDiff;
use gpui::{App, Entity};
use parking_lot::Mutex;
use std::sync::Arc;
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
}
