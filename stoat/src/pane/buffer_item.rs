//! Buffer item implementation for text editing.
//!
//! This module provides [`BufferItem`], a concrete implementation of the [`super::item::Item`]
//! trait for text buffer editing. It wraps a text buffer along with associated parsing state
//! (tokens, syntax tree, language info) needed for syntax highlighting and editing.
//!
//! # Architecture
//!
//! [`BufferItem`] combines several components:
//!
//! - [`text::Buffer`] - Core text buffer with rope data structure
//! - [`TokenMap`] - Syntax highlighting tokens from tree-sitter
//! - [`Parser`] - Tree-sitter parser for current language
//! - [`Language`] - Language configuration (determines which parser/grammar to use)
//!
//! These are grouped together because they're tightly coupled - when the buffer changes,
//! tokens need updating; when language changes, parser needs reinitializing.
//!
//! # Usage
//!
//! ```ignore
//! // Create a buffer item
//! let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
//! let item = cx.new(|cx| BufferItem::new(buffer, Language::Rust, cx));
//!
//! // Use as Item trait object
//! let handle: Box<dyn ItemHandle> = Box::new(item);
//! ```
//!
//! # Related
//!
//! See also:
//! - [`super::item::Item`] - Trait this implements
//! - [`crate::Stoat`] - Manages collection of BufferItems
//! - [`text::Buffer`] - Underlying text storage

use super::item::Item;
use gpui::{
    div, App, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, SharedString,
    Window,
};
use parking_lot::Mutex;
use std::sync::Arc;
use stoat_rope::{TokenMap, TokenSnapshot};
use stoat_text::{Language, Parser};
use text::{Buffer, BufferSnapshot};

/// Events emitted by a buffer item.
///
/// Currently minimal - can be extended for dirty state changes, title changes, etc.
#[derive(Clone, Debug)]
pub enum BufferItemEvent {
    /// Buffer content was edited
    Edited,
    /// Dirty state changed
    DirtyChanged,
}

/// A text buffer item for display in editor panes.
///
/// Wraps a [`text::Buffer`] along with syntax highlighting state (tokens, parser, language).
/// Implements [`Item`] trait to participate in pane item system.
///
/// # Fields
///
/// - `buffer` - Core text storage
/// - `token_map` - Syntax highlighting tokens (shared, can be accessed from render thread)
/// - `parser` - Tree-sitter parser for current language
/// - `current_language` - Active language (Rust, JavaScript, etc.)
///
/// # Lifecycle
///
/// 1. Create with [`new`](Self::new) providing buffer and language
/// 2. Parser automatically initializes for the language
/// 3. Call [`reparse`](Self::reparse) after buffer edits to update tokens
/// 4. Render uses [`token_snapshot`](Self::token_snapshot) for syntax highlighting
///
/// # Example
///
/// ```ignore
/// let buffer = cx.new(|_| Buffer::new(0, buffer_id, "fn main() {}"));
/// let mut item = BufferItem::new(buffer, Language::Rust, cx);
///
/// // Parse initial content
/// item.reparse(cx).ok();
///
/// // Get tokens for rendering
/// let tokens = item.token_snapshot();
/// ```
pub struct BufferItem {
    /// Text buffer entity
    buffer: Entity<Buffer>,

    /// Syntax highlighting tokens (shared for concurrent access)
    token_map: Arc<Mutex<TokenMap>>,

    /// Tree-sitter parser for current language
    parser: Parser,

    /// Current language setting
    current_language: Language,
}

impl BufferItem {
    /// Create a new buffer item.
    ///
    /// Initializes parser for the specified language and creates empty token map.
    ///
    /// # Arguments
    ///
    /// * `buffer` - Text buffer entity to wrap
    /// * `language` - Initial language for syntax highlighting
    /// * `cx` - App context for reading buffer snapshot
    ///
    /// # Returns
    ///
    /// New buffer item ready for use. Call [`reparse`](Self::reparse) to populate tokens.
    ///
    /// # Panics
    ///
    /// Panics if parser initialization fails for the given language.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));
    /// let item = BufferItem::new(buffer, Language::PlainText, cx);
    /// ```
    pub fn new(buffer: Entity<Buffer>, language: Language, cx: &App) -> Self {
        let buffer_snapshot = buffer.read(cx).snapshot();
        let token_map = Arc::new(Mutex::new(TokenMap::new(&buffer_snapshot)));
        let parser = Parser::new(language).expect("Failed to create parser");

        Self {
            buffer,
            token_map,
            parser,
            current_language: language,
        }
    }

    /// Get reference to the underlying buffer entity.
    ///
    /// Used by editor logic that needs direct buffer access.
    ///
    /// # Returns
    ///
    /// Reference to buffer entity
    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    /// Get a snapshot of the buffer state.
    ///
    /// Snapshot provides immutable view of buffer content, useful for rendering
    /// and other read-only operations.
    ///
    /// # Arguments
    ///
    /// * `cx` - App context for reading buffer
    ///
    /// # Returns
    ///
    /// Immutable buffer snapshot
    pub fn buffer_snapshot(&self, cx: &App) -> BufferSnapshot {
        self.buffer.read(cx).snapshot()
    }

    /// Get a snapshot of syntax highlighting tokens.
    ///
    /// Token snapshot provides immutable view of current tokens, safe to use
    /// from render thread without blocking.
    ///
    /// # Returns
    ///
    /// Immutable token snapshot
    pub fn token_snapshot(&self) -> TokenSnapshot {
        self.token_map.lock().snapshot()
    }

    /// Reparse buffer content and update syntax highlighting tokens.
    ///
    /// Should be called after buffer edits to keep tokens in sync with buffer content.
    /// Parses full buffer text using tree-sitter and updates token map.
    ///
    /// # Arguments
    ///
    /// * `cx` - App context for reading buffer
    ///
    /// # Returns
    ///
    /// `Ok(())` if parsing succeeded, `Err(message)` if parsing failed
    ///
    /// # Errors
    ///
    /// Returns error if tree-sitter parsing fails (malformed syntax, parser bug, etc.).
    /// Tokens are left in previous state on error.
    ///
    /// # Example
    ///
    /// ```ignore
    /// buffer.update(cx, |b, _| b.edit([(0..0, "new text")]));
    /// item.reparse(cx).ok(); // Update tokens after edit
    /// ```
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
    /// Updates language setting and creates new parser for that language.
    /// Call [`reparse`](Self::reparse) after to regenerate tokens.
    ///
    /// # Arguments
    ///
    /// * `language` - New language to use
    ///
    /// # Panics
    ///
    /// Panics if parser initialization fails for the new language.
    ///
    /// # Example
    ///
    /// ```ignore
    /// item.set_language(Language::Rust);
    /// item.reparse(cx).ok();
    /// ```
    pub fn set_language(&mut self, language: Language) {
        if language != self.current_language {
            self.current_language = language;
            self.parser = Parser::new(language).expect("Failed to create parser");
        }
    }

    /// Get current language setting.
    ///
    /// # Returns
    ///
    /// Active language
    pub fn language(&self) -> Language {
        self.current_language
    }
}

impl EventEmitter<BufferItemEvent> for BufferItem {}

impl Item for BufferItem {
    type Event = BufferItemEvent;

    fn tab_content_text(&self, _cx: &App) -> SharedString {
        // FIXME: Get actual filename from buffer file path
        // For now return "untitled" placeholder
        "untitled".into()
    }

    fn is_dirty(&self, _cx: &App) -> bool {
        // FIXME: Track dirty state - text::Buffer doesn't have is_dirty()
        // Need to track modifications separately or extend Buffer
        false
    }

    fn can_save(&self, _cx: &App) -> bool {
        // FIXME: Check if buffer has file backing
        // For now always return false (no save support yet)
        false
    }
}

impl Render for BufferItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // FIXME: Actual buffer rendering with syntax highlighting
        // This is a minimal placeholder for now
        div().child("Buffer content")
    }
}
