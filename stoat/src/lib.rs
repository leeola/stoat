//! Stoat v2: Core text editor library.
//!
//! This crate provides a data-driven architecture where all business logic
//! is implemented as pure functions operating on immutable state. The core
//! principle is that all state transitions are predictable and testable.
//!
//! # Architecture Overview
//!
//! - [`EditorState`]: Immutable state containing buffer, cursor, modes, etc.
//! - [`EditorEvent`]: Input events using iced types directly
//! - [`Effect`]: Side effects as data (file I/O, clipboard, etc.)
//! - [`EditorAction`]: Pure state transformations
//! - [`EditorEngine`]: Stateful wrapper for convenient API
//! - [`Stoat`]: High-level API for testing and simplified usage
//!
//! # Example
//!
//! ```rust
//! use stoat::*;
//!
//! let mut engine = EditorEngine::new();
//! let effects = engine.handle_event(EditorEvent::KeyPress {
//!     key: "i".to_string(),
//!     modifiers: input::Modifiers::default()
//! });
//! ```
//!
//! # Simplified API Example
//!
//! ```rust
//! use stoat::Stoat;
//!
//! let mut editor = Stoat::new();
//! editor.keyboard_input("iHello World<Esc>");
//! assert_eq!(editor.buffer_contents(), "Hello World");
//! ```

pub mod actions;
pub mod cli;
pub mod command;
pub mod config;
pub mod effects;
pub mod engine;
pub mod events;
pub mod input;
pub mod key_notation;
pub mod keymap;
pub mod log;
pub mod processor;
pub mod state;

pub mod test_stoat;

// Re-export core types for convenient use
use actions::EditMode;
pub use actions::EditorAction;
pub use command::Command;
pub use effects::Effect;
pub use engine::EditorEngine;
pub use events::EditorEvent;
// Re-export input types for consumers
pub use input::{Key, Modifiers, MouseButton, Point};
pub use keymap::Keymap;
// Note: process_event now requires a keymap parameter
pub use processor::process_event;
pub use state::EditorState;

/// High-level API for the Stoat editor with simplified keyboard input.
///
/// This struct provides a user-friendly interface for both testing and regular usage.
/// It wraps the [`EditorEngine`] and provides convenient methods for keyboard input
/// and buffer inspection.
///
/// # Example
///
/// ```rust
/// use stoat::Stoat;
///
/// let mut editor = Stoat::new();
///
/// // Type text using vim-like sequences
/// editor.keyboard_input("iHello, World!<Esc>");
///
/// // Check buffer contents
/// assert_eq!(editor.buffer_contents(), "Hello, World!");
/// assert_eq!(editor.mode(), "normal");
/// ```
pub struct Stoat {
    engine: EditorEngine,
}

impl Stoat {
    /// Creates a new Stoat editor instance with empty buffer.
    pub fn new() -> Self {
        Self {
            engine: EditorEngine::new(),
        }
    }

    /// Creates a new Stoat editor with initial text content.
    pub fn with_text(text: &str) -> Self {
        Self {
            engine: EditorEngine::with_text(text),
        }
    }

    /// Creates a new Stoat editor with initial text content and specified language.
    pub fn with_text_and_language(text: &str, language: stoat_text::parser::Language) -> Self {
        Self {
            engine: EditorEngine::with_text_and_language(text, language),
        }
    }

    /// Creates a new test instance for fluent testing.
    ///
    /// Returns a [`TestStoat`] wrapper that provides convenient
    /// methods for writing concise, readable tests.
    ///
    /// # Example
    ///
    /// ```rust
    /// use stoat::Stoat;
    ///
    /// Stoat::test()
    ///     .with_text("hello")
    ///     .type_keys("i world")
    ///     .assert_text(" worldhello");
    /// ```
    pub fn test() -> test_stoat::TestStoat {
        test_stoat::TestStoat::new()
    }

    /// Processes keyboard input using vim-like syntax.
    ///
    /// This method accepts a string representation of keyboard input and processes
    /// it as a sequence of key events. Special keys are represented in angle brackets.
    ///
    /// # Supported Special Keys
    ///
    /// - `<Esc>` or `<Escape>` - Escape key
    /// - `<Enter>` or `<Return>` or `<CR>` - Enter key
    /// - `<Tab>` - Tab key
    /// - `<BS>` or `<Backspace>` - Backspace key
    /// - `<Del>` or `<Delete>` - Delete key
    /// - `<Space>` - Space key (alternative to literal space)
    /// - `<Left>`, `<Right>`, `<Up>`, `<Down>` - Arrow keys
    /// - `<Home>`, `<End>` - Navigation keys
    /// - `<PageUp>`, `<PageDown>` - Page navigation
    /// - `<C-x>` - Ctrl+x (where x is any character)
    /// - `<S-Tab>` - Shift+Tab
    /// - `<A-x>` or `<M-x>` - Alt+x (where x is any character)
    ///
    /// # Examples
    ///
    /// ```rust
    /// use stoat::Stoat;
    ///
    /// let mut editor = Stoat::new();
    ///
    /// // Enter insert mode, type text, exit
    /// editor.keyboard_input("iHello<Esc>");
    ///
    /// // Navigation
    /// editor.keyboard_input("gg");  // Go to top
    /// editor.keyboard_input("G");   // Go to bottom
    ///
    /// // With modifiers
    /// editor.keyboard_input("<C-a>"); // Ctrl+A
    /// ```
    pub fn keyboard_input(&mut self, input: &str) -> Vec<Effect> {
        let events = key_notation::parse_sequence(input);
        let mut all_effects = Vec::new();

        for event in events {
            let effects = self.engine.handle_event(event);
            all_effects.extend(effects);
        }

        all_effects
    }

    /// Returns the entire buffer contents as a string.
    pub fn buffer_contents(&self) -> String {
        self.engine.text()
    }

    /// Returns the current cursor position as (line, column) tuple.
    ///
    /// Both line and column are 0-indexed.
    pub fn cursor_position(&self) -> (usize, usize) {
        let pos = self.engine.cursor_position();
        (pos.line, pos.column)
    }

    /// Returns the current editor mode as a string.
    ///
    /// Possible values: "normal", "insert", "command", or custom mode names
    pub fn mode(&self) -> String {
        match self.engine.mode() {
            EditMode::Normal => "normal".to_string(),
            EditMode::Insert => "insert".to_string(),
            EditMode::Command => "command".to_string(),
            EditMode::Custom(name) => name,
        }
    }

    /// Returns whether the buffer has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.engine.is_dirty()
    }

    /// Returns the number of lines in the buffer.
    pub fn line_count(&self) -> usize {
        self.engine.line_count()
    }

    /// Returns a specific line from the buffer.
    pub fn line(&self, index: usize) -> Option<String> {
        self.engine.line(index)
    }

    /// Returns a reference to the underlying [`EditorEngine`].
    ///
    /// This provides access to the full engine API when needed.
    pub fn engine(&self) -> &EditorEngine {
        &self.engine
    }

    /// Returns a mutable reference to the underlying [`EditorEngine`].
    ///
    /// This provides mutable access to the full engine API when needed.
    pub fn engine_mut(&mut self) -> &mut EditorEngine {
        &mut self.engine
    }

    /// Asserts that the buffer contents match the expected text.
    ///
    /// This is a convenience method for testing that panics if the
    /// buffer contents don't match the expected value.
    ///
    /// # Panics
    ///
    /// Panics if the buffer contents don't match the expected text.
    #[cfg(test)]
    pub fn assert_buffer_eq(&self, expected: &str) {
        let actual = self.buffer_contents();
        assert_eq!(
            actual, expected,
            "Buffer content mismatch:\nExpected:\n{expected}\nActual:\n{actual}"
        );
    }

    /// Asserts that the cursor is at the expected position.
    ///
    /// # Panics
    ///
    /// Panics if the cursor position doesn't match.
    #[cfg(test)]
    pub fn assert_cursor_at(&self, line: usize, column: usize) {
        let (actual_line, actual_col) = self.cursor_position();
        assert_eq!(
            (actual_line, actual_col),
            (line, column),
            "Cursor position mismatch: expected ({line}, {column}), got ({actual_line}, {actual_col})"
        );
    }
}

impl Default for Stoat {
    fn default() -> Self {
        Self::new()
    }
}

// Tests have been moved to stoat/tests/ for better organization
// See: basic_editing.rs, tab_handling.rs, language_context.rs,
//      input_handling.rs, keymap.rs, text_operations.rs

#[cfg(test)]
mod stoat_tests {
    // All tests have been moved to stoat/tests/ for better organization
}
