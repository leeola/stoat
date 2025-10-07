//! Buffer item for text editing with syntax highlighting.
//!
//! Wraps [`text::Buffer`] with syntax highlighting tokens from tree-sitter.

use gpui::{App, Entity};
use parking_lot::Mutex;
use std::sync::Arc;
use stoat_rope::{TokenMap, TokenSnapshot};
use stoat_text::{Language, Parser};
use text::{Buffer, BufferSnapshot};

/// A text buffer with syntax highlighting.
///
/// Combines text buffer, token map for syntax highlighting, and parser for language support.
pub struct BufferItem {
    /// Text buffer entity
    buffer: Entity<Buffer>,

    /// Syntax highlighting tokens (shared for concurrent access)
    token_map: Arc<Mutex<TokenMap>>,

    /// Tree-sitter parser for current language
    parser: Parser,

    /// Current language setting
    language: Language,
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
                Err(format!("Parse error: {}", e))
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
}
