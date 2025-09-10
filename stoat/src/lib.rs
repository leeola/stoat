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
    pub fn test() -> crate::test_stoat::TestStoat {
        crate::test_stoat::TestStoat::new()
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

#[cfg(test)]
mod stoat_tests {
    use super::*;
    use crate::{actions::EditMode, input::Modifiers};

    #[test]
    fn stoat_new_creates_empty_editor() {
        Stoat::test()
            .assert_text("")
            .assert_cursor(0, 0)
            .assert_mode(EditMode::Normal)
            .assert_dirty(false);
    }

    #[test]
    fn stoat_with_text_initializes_content() {
        Stoat::test()
            .with_text("Hello\nWorld")
            .assert_text("Hello\nWorld");
    }

    #[test]
    fn keyboard_input_with_modifiers() {
        let _editor = Stoat::new();

        // Test Ctrl+key
        let events = key_notation::parse_sequence("<C-a>");
        assert_eq!(events.len(), 1);
        if let EditorEvent::KeyPress { modifiers, .. } = &events[0] {
            assert!(modifiers.control);
        }

        // Test Alt+key
        let events = key_notation::parse_sequence("<A-x>");
        assert_eq!(events.len(), 1);
        if let EditorEvent::KeyPress { modifiers, .. } = &events[0] {
            assert!(modifiers.alt);
        }
    }

    #[test]
    fn test_literal_space_key_event() {
        Stoat::test()
            .type_keys("i") // Enter insert mode
            .assert_mode(EditMode::Insert)
            .type_keys(" ") // Type a literal space
            .assert_text(" ");
    }

    #[test]
    fn test_keyboard_input_with_literal_space() {
        Stoat::test()
            .type_keys("iHello World")
            .assert_text("Hello World");
    }

    #[test]
    fn test_keyboard_input_with_literal_tab() {
        let test = Stoat::test().type_keys("iHello\tWorld");

        let contents = test.stoat().buffer_contents();
        assert!(
            contents == "Hello\tWorld" || contents.starts_with("Hello "),
            "Literal tab should be inserted as tab or spaces, got: {contents:?}"
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
            key: input::keys::TAB.to_string(),
            modifiers: Modifiers::default(),
        };
        editor.engine_mut().handle_event(tab_event);

        // The cursor should be at column 8 (tab aligns to column 8)
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

        // Insert tab - this should move cursor to next tab stop (column 8 in display)
        let tab_event = EditorEvent::KeyPress {
            key: input::keys::TAB.to_string(),
            modifiers: Modifiers::default(),
        };
        editor.engine_mut().handle_event(tab_event);

        // Check that desired_column reflects display position
        // "abc" = 3 display columns, then tab advances to column 8 (next tab stop)
        // So cursor should be at display column 8
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 8,
            "Desired column should be 8 after 'abc<tab>'"
        );

        // Character position should be 4 (a, b, c, \t)
        let (_, col) = editor.cursor_position();
        assert_eq!(col, 4, "Character position should be 4");

        // Type more text
        editor.keyboard_input("def");

        // Desired column should now be 11 (8 + 3 characters)
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 11,
            "Desired column should be 11 after 'abc<tab>def'"
        );
    }

    #[test]
    fn test_tab_insertion_scenarios() {
        let mut editor = Stoat::new();

        // Test 1: Tab at beginning of line
        editor.keyboard_input("i");
        editor.keyboard_input("\t");
        assert_eq!(editor.buffer_contents(), "\t");
        let (_, col) = editor.cursor_position();
        assert_eq!(col, 1, "Tab at start should put cursor at position 1");

        // Test 2: Continue typing after tab
        editor.keyboard_input("hello");
        assert_eq!(editor.buffer_contents(), "\thello");
        let (_, col) = editor.cursor_position();
        assert_eq!(col, 6, "Should be at position 6 after tab+hello");

        // Clear and test another scenario
        editor = Stoat::new();
        editor.keyboard_input("i");

        // Test 3: Tab in middle of text
        editor.keyboard_input("ab");
        editor.keyboard_input("\t");
        editor.keyboard_input("cd");
        assert_eq!(editor.buffer_contents(), "ab\tcd");
        let (_, col) = editor.cursor_position();
        assert_eq!(col, 5, "Should be at position 5 after ab<tab>cd");

        // Test 4: Multiple tabs
        editor = Stoat::new();
        editor.keyboard_input("i");
        editor.keyboard_input("\t\t");
        assert_eq!(editor.buffer_contents(), "\t\t");
        let (_, col) = editor.cursor_position();
        assert_eq!(col, 2, "Should be at position 2 after two tabs");
    }

    #[test]
    fn test_tab_stop_alignment() {
        let mut editor = Stoat::new();

        // Test that tabs align to tab stops correctly
        // Tab width is 8, so tab stops are at columns 0, 8, 16, 24, etc.

        editor.keyboard_input("i");

        // Test 1: "a\t" should put cursor at column 8
        editor.keyboard_input("a\t");
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 8,
            "After 'a<tab>', cursor should be at display column 8"
        );
        assert_eq!(
            editor.cursor_position().1,
            2,
            "Character position should be 2"
        );

        // Clear for next test
        editor = Stoat::new();
        editor.keyboard_input("i");

        // Test 2: "ab\t" should also put cursor at column 8
        editor.keyboard_input("ab\t");
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 8,
            "After 'ab<tab>', cursor should be at display column 8"
        );
        assert_eq!(
            editor.cursor_position().1,
            3,
            "Character position should be 3"
        );

        // Clear for next test
        editor = Stoat::new();
        editor.keyboard_input("i");

        // Test 3: "abc\t" should also put cursor at column 8
        editor.keyboard_input("abc\t");
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 8,
            "After 'abc<tab>', cursor should be at display column 8"
        );
        assert_eq!(
            editor.cursor_position().1,
            4,
            "Character position should be 4"
        );

        // Clear for next test
        editor = Stoat::new();
        editor.keyboard_input("i");

        // Test 4: "abcd\t" should put cursor at column 8 (same tab stop since 4 < 8)
        editor.keyboard_input("abcd\t");
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 8,
            "After 'abcd<tab>', cursor should be at display column 8"
        );
        assert_eq!(
            editor.cursor_position().1,
            5,
            "Character position should be 5"
        );
    }

    #[test]
    fn test_tab_insertion_middle_of_text() {
        let mut editor = Stoat::new();

        // Start with "abc", then insert tab, then add "def"
        // This avoids cursor movement issues and focuses on tab behavior
        editor.keyboard_input("i");
        editor.keyboard_input("abc");
        editor.keyboard_input("\t");

        // Check display column after tab
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 8,
            "After 'abc<tab>', display column should be 8"
        );

        // Now type more text
        editor.keyboard_input("def");

        // Buffer should be "abc\tdef"
        assert_eq!(editor.buffer_contents(), "abc\tdef");

        // Cursor character position should be 7 (a,b,c,\t,d,e,f)
        let (_, col) = editor.cursor_position();
        assert_eq!(col, 7, "Character position should be 7");

        // Display column should be 11 (8 for tab stop + 3 for "def")
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.desired_column, 11,
            "Display column should be 11 after 'abc<tab>def'"
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
            key: input::keys::TAB.to_string(),
            modifiers: Modifiers::default(),
        };

        editor.engine_mut().handle_event(tab_event);

        // Verify that a tab was inserted (or spaces if tab expansion is enabled)
        let contents = editor.buffer_contents();
        assert!(
            contents == "\t" || contents == "    " || contents == "  ",
            "Tab key should insert tab character or spaces, got: {contents:?}"
        );
    }

    #[test]
    fn test_tab_backspace_cursor_position() {
        let mut editor = Stoat::new();

        // Enter insert mode
        editor.keyboard_input("i");
        assert_eq!(editor.mode(), "insert");

        // Type: a<Tab>a<Tab>
        editor.keyboard_input("a");
        editor.keyboard_input("<Tab>");
        editor.keyboard_input("a");
        editor.keyboard_input("<Tab>");

        // At this point we should have "a\ta\t"
        let contents = editor.buffer_contents();
        assert_eq!(contents, "a\ta\t", "Should have 'a<tab>a<tab>'");

        // Check cursor position before backspace (should be at visual column 16)
        // a=1, tab aligns to 8, a=9, tab aligns to 16
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.position().visual_column,
            16,
            "Visual column should be 16 after 'a<tab>a<tab>'"
        );
        assert_eq!(
            state.cursor.position().column,
            4,
            "Character column should be 4"
        );
        assert_eq!(
            state.cursor.position().byte_offset,
            4,
            "Byte offset should be 4"
        );

        // Now press backspace
        editor.keyboard_input("<Backspace>");

        // Should have "a\ta" now
        let contents = editor.buffer_contents();
        assert_eq!(contents, "a\ta", "Should have 'a<tab>a' after backspace");

        // Check cursor position after backspace
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.position().visual_column,
            9,
            "Visual column should be 9 after backspace"
        );
        assert_eq!(
            state.cursor.position().column,
            3,
            "Character column should be 3 after backspace"
        );
        assert_eq!(
            state.cursor.position().byte_offset,
            3,
            "Byte offset should be 3 after backspace"
        );
        assert_eq!(
            state.cursor.desired_column, 9,
            "Desired column should match visual column"
        );
    }

    #[test]
    fn test_backspace_after_single_tab() {
        let mut editor = Stoat::new();

        editor.keyboard_input("i");
        editor.keyboard_input("<Tab>");

        // Should have a single tab
        assert_eq!(editor.buffer_contents(), "\t");
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.position().visual_column,
            8,
            "Tab should move to column 8"
        );

        // Backspace should delete the tab
        editor.keyboard_input("<Backspace>");
        assert_eq!(editor.buffer_contents(), "");
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.position().visual_column,
            0,
            "Should be back at column 0"
        );
        assert_eq!(state.cursor.position().column, 0);
    }

    #[test]
    fn test_backspace_in_middle_of_tabs() {
        let mut editor = Stoat::new();

        editor.keyboard_input("i");
        editor.keyboard_input("<Tab>");
        editor.keyboard_input("<Tab>");
        editor.keyboard_input("<Tab>");

        // Three tabs
        assert_eq!(editor.buffer_contents(), "\t\t\t");
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.position().visual_column,
            24,
            "Three tabs: 8, 16, 24"
        );

        // Move left twice
        editor.keyboard_input("<Esc>"); // Back to normal mode
        editor.keyboard_input("h");
        editor.keyboard_input("h");

        // Should be at position 1 (after first tab)
        let state = editor.engine().state();
        assert_eq!(state.cursor.position().column, 1);

        // Enter insert mode and backspace
        editor.keyboard_input("i");
        editor.keyboard_input("<Backspace>");

        // Should have deleted the first tab
        assert_eq!(editor.buffer_contents(), "\t\t");
        let state = editor.engine().state();
        assert_eq!(state.cursor.position().column, 0);
        assert_eq!(state.cursor.position().visual_column, 0);
    }

    #[test]
    fn test_multiple_backspaces_with_tabs() {
        let mut editor = Stoat::new();

        editor.keyboard_input("i");
        editor.keyboard_input("abc");
        editor.keyboard_input("<Tab>");
        editor.keyboard_input("def");
        editor.keyboard_input("<Tab>");
        editor.keyboard_input("ghi");

        // "abc\tdef\tghi"
        assert_eq!(editor.buffer_contents(), "abc\tdef\tghi");

        // Multiple backspaces
        editor.keyboard_input("<Backspace>"); // Delete 'i'
        editor.keyboard_input("<Backspace>"); // Delete 'h'
        editor.keyboard_input("<Backspace>"); // Delete 'g'
        assert_eq!(editor.buffer_contents(), "abc\tdef\t");

        editor.keyboard_input("<Backspace>"); // Delete tab
        assert_eq!(editor.buffer_contents(), "abc\tdef");
        let state = editor.engine().state();
        assert_eq!(
            state.cursor.position().visual_column,
            11,
            "abc=3, tab to 8, def=11"
        );

        editor.keyboard_input("<Backspace>"); // Delete 'f'
        editor.keyboard_input("<Backspace>"); // Delete 'e'
        editor.keyboard_input("<Backspace>"); // Delete 'd'
        editor.keyboard_input("<Backspace>"); // Delete tab
        assert_eq!(editor.buffer_contents(), "abc");
        let state = editor.engine().state();
        assert_eq!(state.cursor.position().visual_column, 3);
    }
}
