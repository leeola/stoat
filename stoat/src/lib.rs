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
//! use iced::keyboard;
//!
//! let mut engine = EditorEngine::new();
//! let effects = engine.handle_event(EditorEvent::KeyPress {
//!     key: keyboard::Key::Character("i".to_string().into()),
//!     modifiers: keyboard::Modifiers::default()
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
pub mod effects;
pub mod engine;
pub mod events;
pub mod key_notation;
pub mod keymap;
pub mod log;
pub mod processor;
pub mod state;

#[cfg(test)]
pub mod testing;

// Re-export core types for convenient use
use actions::EditMode;
pub use actions::EditorAction;
pub use command::Command;
pub use effects::Effect;
pub use engine::EditorEngine;
pub use events::EditorEvent;
// Re-export commonly used iced types for consumers
pub use iced::{keyboard, mouse, Point};
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
    /// Possible values: "normal", "insert", "visual", "command"
    pub fn mode(&self) -> &str {
        match self.engine.mode() {
            EditMode::Normal => "normal",
            EditMode::Insert => "insert",
            EditMode::Visual { .. } => "visual",
            EditMode::Command => "command",
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
            "Buffer content mismatch:\nExpected:\n{}\nActual:\n{}",
            expected, actual
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
            "Cursor position mismatch: expected ({}, {}), got ({}, {})",
            line,
            column,
            actual_line,
            actual_col
        );
    }
}

impl Default for Stoat {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod stoat_tests {
    use super::*;

    #[test]
    fn stoat_new_creates_empty_editor() {
        let editor = Stoat::new();
        assert_eq!(editor.buffer_contents(), "");
        assert_eq!(editor.cursor_position(), (0, 0));
        assert_eq!(editor.mode(), "normal");
        assert!(!editor.is_dirty());
    }

    #[test]
    fn stoat_with_text_initializes_content() {
        let editor = Stoat::with_text("Hello\nWorld");
        assert_eq!(editor.buffer_contents(), "Hello\nWorld");
    }

    #[test]
    fn keyboard_input_basic_typing() {
        let mut editor = Stoat::new();
        editor.keyboard_input("iHello World<Esc>");

        assert_eq!(editor.buffer_contents(), "Hello World");
        assert_eq!(editor.mode(), "normal");
    }

    #[test]
    fn keyboard_input_navigation() {
        let mut editor = Stoat::with_text("Hello World");
        editor.keyboard_input("l"); // Move right
        assert_eq!(editor.cursor_position(), (0, 1));

        editor.keyboard_input("l"); // Move right again
        assert_eq!(editor.cursor_position(), (0, 2));

        editor.keyboard_input("h"); // Move left
        assert_eq!(editor.cursor_position(), (0, 1));

        editor.keyboard_input("h"); // Move left again
        assert_eq!(editor.cursor_position(), (0, 0));
    }

    #[test]
    fn keyboard_input_with_modifiers() {
        let _editor = Stoat::new();

        // Test Ctrl+key
        let events = key_notation::parse_sequence("<C-a>");
        assert_eq!(events.len(), 1);
        if let EditorEvent::KeyPress { modifiers, .. } = &events[0] {
            assert!(modifiers.control());
        }

        // Test Alt+key
        let events = key_notation::parse_sequence("<A-x>");
        assert_eq!(events.len(), 1);
        if let EditorEvent::KeyPress { modifiers, .. } = &events[0] {
            assert!(modifiers.alt());
        }
    }

    #[test]
    fn assert_buffer_eq_works() {
        let mut editor = Stoat::new();
        editor.keyboard_input("iTest");
        editor.assert_buffer_eq("Test");
    }

    #[test]
    fn assert_cursor_at_works() {
        let mut editor = Stoat::with_text("Hello World");
        editor.keyboard_input("ll"); // Move to column 2
        editor.assert_cursor_at(0, 2);
    }

    #[test]
    fn keyboard_input_delete_operations() {
        let mut editor = Stoat::new();
        editor.keyboard_input("iHello<BS><BS>"); // Type and delete
        assert_eq!(editor.buffer_contents(), "Hel");
    }

    #[test]
    fn test_literal_space_key_event() {
        let mut editor = Stoat::new();
        // Enter insert mode
        editor.keyboard_input("i");
        assert_eq!(editor.mode(), "insert");

        // Manually create and send a Space key event
        let space_event = EditorEvent::KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Space),
            modifiers: keyboard::Modifiers::default(),
        };

        editor.engine_mut().handle_event(space_event);

        // Verify that a space was inserted
        assert_eq!(
            editor.buffer_contents(),
            " ",
            "Space key should insert a space character"
        );
    }

    #[test]
    fn test_keyboard_input_with_literal_space() {
        let mut editor = Stoat::new();
        // Type with literal space in the string
        editor.keyboard_input("iHello World");
        assert_eq!(
            editor.buffer_contents(),
            "Hello World",
            "Literal space should be inserted"
        );
    }

    #[test]
    fn test_keyboard_input_with_literal_tab() {
        let mut editor = Stoat::new();
        // Type with literal tab in the string
        editor.keyboard_input("iHello\tWorld");
        let contents = editor.buffer_contents();
        assert!(
            contents == "Hello\tWorld" || contents.starts_with("Hello "),
            "Literal tab should be inserted as tab or spaces, got: {:?}",
            contents
        );
    }

    #[test]
    fn test_tab_cursor_positioning() {
        let mut editor = Stoat::new();

        // Enter insert mode and type: "a<tab>b"
        editor.keyboard_input("i");
        editor.keyboard_input("a");

        // Insert tab
        let tab_event = EditorEvent::KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Tab),
            modifiers: keyboard::Modifiers::default(),
        };
        editor.engine_mut().handle_event(tab_event);

        // The cursor should be at column 5 (1 for 'a' + 4 for tab display width)
        // But the actual character position is 2 (a + \t)
        let (_, col) = editor.cursor_position();
        assert_eq!(
            col, 2,
            "Cursor character position should be at 2 after 'a' and tab"
        );

        // Now type 'b'
        editor.keyboard_input("b");

        // Buffer should contain "a\tb"
        assert_eq!(
            editor.buffer_contents(),
            "a\tb",
            "Buffer should contain 'a<tab>b'"
        );

        // Cursor should be at character position 3
        let (_, col) = editor.cursor_position();
        assert_eq!(
            col, 3,
            "Cursor should be at character position 3 after 'a<tab>b'"
        );
    }

    #[test]
    fn test_tab_display_column_tracking() {
        let mut editor = Stoat::new();

        // Enter insert mode
        editor.keyboard_input("i");

        // Type "abc<tab>def"
        editor.keyboard_input("abc");

        // Insert tab - this should move cursor to next tab stop (column 4 in display)
        let tab_event = EditorEvent::KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Tab),
            modifiers: keyboard::Modifiers::default(),
        };
        editor.engine_mut().handle_event(tab_event);

        // Check that desired_column reflects display position
        // "abc" = 3 display columns, then tab advances to column 4 (next tab stop)
        // So cursor should be at display column 4
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 4,
            "Desired column should be 4 after 'abc<tab>'"
        );

        // Character position should be 4 (a, b, c, \t)
        let (_, col) = editor.cursor_position();
        assert_eq!(col, 4, "Character position should be 4");

        // Type more text
        editor.keyboard_input("def");

        // Desired column should now be 7 (4 + 3 characters)
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 7,
            "Desired column should be 7 after 'abc<tab>def'"
        );
    }

    #[test]
    fn test_literal_tab_key_event() {
        let mut editor = Stoat::new();
        // Enter insert mode
        editor.keyboard_input("i");
        assert_eq!(editor.mode(), "insert");

        // Manually create and send a Tab key event
        let tab_event = EditorEvent::KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Tab),
            modifiers: keyboard::Modifiers::default(),
        };

        editor.engine_mut().handle_event(tab_event);

        // Verify that a tab was inserted (or spaces if tab expansion is enabled)
        let contents = editor.buffer_contents();
        assert!(
            contents == "\t" || contents == "    " || contents == "  ",
            "Tab key should insert tab character or spaces, got: {:?}",
            contents
        );
    }
}
