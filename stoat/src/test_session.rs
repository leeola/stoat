//! Testing utilities and helpers for stoat_v2.
//!
//! This module provides convenient builders, assertion helpers, and other
//! utilities to make testing editor functionality easier and more readable.

use crate::{
    actions::{EditMode, TextPosition, TextRange},
    effects::Effect,
    engine::EditorEngine,
    events::EditorEvent,
    input::{keys, Key, Modifiers, MouseButton, Point},
    key_notation,
    state::EditorState,
};
use std::path::PathBuf;

/// Builder for creating [`EditorState`] instances in tests.
///
/// This provides a fluent API for constructing editor states with
/// specific configurations for testing purposes.
///
/// # Example
///
/// ```rust
/// use stoat_v2::testing::StateBuilder;
/// use stoat_v2::actions::{EditMode, TextPosition};
///
/// let state = StateBuilder::new()
///     .with_text("Hello\nWorld")
///     .with_cursor(1, 2)
///     .in_mode(EditMode::Insert)
///     .dirty(true)
///     .build();
///
/// assert_eq!(state.text(), "Hello\nWorld");
/// assert_eq!(state.cursor_position(), TextPosition::new(1, 2));
/// ```
pub struct StateBuilder {
    state: EditorState,
}

impl StateBuilder {
    /// Creates a new state builder with default empty state.
    pub fn new() -> Self {
        Self {
            state: EditorState::new(),
        }
    }

    /// Sets the text content of the buffer.
    pub fn with_text<S: AsRef<str>>(mut self, text: S) -> Self {
        self.state = EditorState::with_text(text.as_ref());
        self
    }

    /// Sets the cursor position.
    pub fn with_cursor(mut self, line: usize, column: usize) -> Self {
        let pos = TextPosition::new(line, column);
        self.state.cursor.move_to(pos);
        self
    }

    /// Sets the editing mode.
    pub fn in_mode(mut self, mode: EditMode) -> Self {
        self.state.mode = mode;
        self
    }

    /// Sets text selection range.
    pub fn with_selection(mut self, start: TextPosition, end: TextPosition) -> Self {
        self.state.cursor.anchor = start;
        self.state.cursor.head = end;
        self
    }

    /// Sets the file path.
    pub fn with_file<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.state.file = crate::state::FileInfo::with_path(path.into());
        self
    }

    /// Sets the dirty flag.
    pub fn dirty(mut self, dirty: bool) -> Self {
        self.state.is_dirty = dirty;
        self
    }

    /// Sets viewport dimensions.
    pub fn with_viewport_size(mut self, width: f32, height: f32) -> Self {
        self.state.viewport.width = width;
        self.state.viewport.height = height;
        self
    }

    /// Sets viewport scroll position.
    pub fn with_scroll(mut self, x: f32, y: f32) -> Self {
        self.state.viewport.scroll_x = x;
        self.state.viewport.scroll_y = y;
        self
    }

    /// Builds the final [`EditorState`].
    pub fn build(self) -> EditorState {
        self.state
    }
}

impl Default for StateBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for creating [`EditorEngine`] instances in tests.
pub struct EngineBuilder {
    state: EditorState,
}

impl EngineBuilder {
    pub fn new() -> Self {
        Self {
            state: EditorState::new(),
        }
    }

    pub fn with_text<S: AsRef<str>>(mut self, text: S) -> Self {
        self.state = EditorState::with_text(text.as_ref());
        self
    }

    pub fn with_cursor(mut self, line: usize, column: usize) -> Self {
        let pos = TextPosition::new(line, column);
        self.state.cursor.move_to(pos);
        self
    }

    pub fn in_mode(mut self, mode: EditMode) -> Self {
        self.state.mode = mode;
        self
    }

    pub fn build(self) -> EditorEngine {
        EditorEngine::with_state(self.state)
    }
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Fluent test session for low-LOC test writing.
///
/// [`TestSession`] provides a chainable API for writing concise tests.
/// It wraps an [`EditorEngine`] and provides methods to simulate input,
/// make assertions, and track effects.
///
/// # Example
///
/// ```rust
/// use stoat::testing::TestSession;
///
/// TestSession::new()
///     .with_text("hello world")
///     .cursor(0, 5)
///     .type_keys("viw")  // Select word in vim
///     .assert_selection(0, 5, 0, 11)
///     .type_keys("d")    // Delete selection
///     .assert_text("hello ");
/// ```
pub struct TestSession {
    engine: EditorEngine,
    last_effects: Vec<Effect>,
}

impl TestSession {
    /// Creates a new test session with empty editor.
    pub fn new() -> Self {
        Self {
            engine: EditorEngine::new(),
            last_effects: Vec::new(),
        }
    }

    /// Sets the initial text content.
    pub fn with_text<S: AsRef<str>>(mut self, text: S) -> Self {
        self.engine = EditorEngine::with_text(text.as_ref());
        self
    }

    /// Sets the cursor position.
    pub fn cursor(mut self, line: usize, column: usize) -> Self {
        // Use MoveCursor action to set position properly
        let event = EditorEvent::MouseClick {
            position: Point::new(column as f32 * 8.0, line as f32 * 16.0), // Approximate char sizes
            button: MouseButton::Left,
        };
        self.engine.handle_event(event);
        self
    }

    /// Sets the editing mode.
    pub fn mode(mut self, mode: EditMode) -> Self {
        // FIXME: No direct way to set mode without going through commands
        // For now, use the key sequences to change modes
        match mode {
            EditMode::Normal => self.type_keys("<Esc>"),
            EditMode::Insert => self.type_keys("i"),
            EditMode::Command => self.type_keys(":"),
            _ => self,
        }
    }

    /// Types a sequence of keys using vim-like notation.
    ///
    /// Uses [`key_notation::parse_sequence`] to convert strings like
    /// "iHello<Esc>" into key events.
    pub fn type_keys<S: AsRef<str>>(mut self, keys: S) -> Self {
        let events = key_notation::parse_sequence(keys.as_ref());
        self.last_effects = self.engine.handle_events(events);
        self
    }

    /// Sends a single key event.
    pub fn key(mut self, key: Key) -> Self {
        let event = EditorEvent::KeyPress {
            key,
            modifiers: Modifiers::default(),
        };
        self.last_effects = self.engine.handle_event(event);
        self
    }

    /// Sends a key with modifiers.
    pub fn key_with_mods(mut self, key: Key, modifiers: Modifiers) -> Self {
        let event = EditorEvent::KeyPress { key, modifiers };
        self.last_effects = self.engine.handle_event(event);
        self
    }

    /// Types literal text (each character as a key press).
    pub fn type_text<S: AsRef<str>>(mut self, text: S) -> Self {
        let events = events::type_text(text.as_ref());
        self.last_effects = self.engine.handle_events(events);
        self
    }

    /// Simulates a mouse click.
    pub fn click(mut self, x: f32, y: f32) -> Self {
        let event = events::click(x, y);
        self.last_effects = self.engine.handle_event(event);
        self
    }

    /// Asserts the current text content.
    #[track_caller]
    pub fn assert_text<S: AsRef<str>>(self, expected: S) -> Self {
        assert_eq!(self.engine.text(), expected.as_ref(), "Text mismatch");
        self
    }

    /// Asserts the cursor position.
    ///
    /// For future multi-cursor support, this checks the primary (first) cursor.
    #[track_caller]
    pub fn assert_cursor(self, line: usize, column: usize) -> Self {
        let pos = self.engine.cursor_position();
        assert_eq!(
            (pos.line, pos.column),
            (line, column),
            "Cursor at ({}, {}) but expected ({}, {})",
            pos.line,
            pos.column,
            line,
            column
        );
        self
    }

    /// Asserts multiple cursor positions.
    ///
    /// Once multi-cursor is implemented, this will check all cursor positions in order.
    /// For now, it only supports asserting a single cursor.
    #[track_caller]
    pub fn assert_cursors(self, positions: &[(usize, usize)]) -> Self {
        // FIXME: When multi-cursor is implemented, check all cursors
        // For now, just check that we have exactly one cursor at the expected position
        assert!(
            !positions.is_empty(),
            "assert_cursors requires at least one position"
        );

        if positions.len() == 1 {
            self.assert_cursor(positions[0].0, positions[0].1)
        } else {
            panic!(
                "Multi-cursor not yet implemented. Expected {} cursors but Stoat currently supports only 1",
                positions.len()
            );
        }
    }

    /// Asserts a specific cursor position by index.
    ///
    /// Once multi-cursor is implemented, this will check the cursor at the given index.
    #[track_caller]
    pub fn assert_cursor_at(self, index: usize, line: usize, column: usize) -> Self {
        // FIXME: When multi-cursor is implemented, check cursor at index
        if index == 0 {
            self.assert_cursor(line, column)
        } else {
            panic!(
                "Multi-cursor not yet implemented. Cannot check cursor at index {}",
                index
            );
        }
    }

    /// Asserts the number of cursors.
    ///
    /// Once multi-cursor is implemented, this will verify the cursor count.
    #[track_caller]
    pub fn assert_cursor_count(self, expected: usize) -> Self {
        // FIXME: When multi-cursor is implemented, check actual cursor count
        let actual = 1; // Stoat currently has exactly one cursor
        assert_eq!(
            actual, expected,
            "Expected {} cursor(s) but found {}",
            expected, actual
        );
        self
    }

    /// Asserts the current mode.
    #[track_caller]
    pub fn assert_mode(self, expected: EditMode) -> Self {
        assert_eq!(self.engine.mode(), expected, "Mode mismatch");
        self
    }

    /// Asserts a text selection exists with the given range.
    ///
    /// For future multi-cursor support, this checks the primary (first) cursor's selection.
    #[track_caller]
    pub fn assert_selection(
        self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
    ) -> Self {
        let selection = self.engine.state().cursor.selection();
        assert!(selection.is_some(), "Expected selection but none exists");

        let range = selection.unwrap();
        let expected_start = TextPosition::new(start_line, start_col);
        let expected_end = TextPosition::new(end_line, end_col);

        assert_eq!(
            range.start, expected_start,
            "Selection start at ({}, {}) but expected ({}, {})",
            range.start.line, range.start.column, start_line, start_col
        );

        assert_eq!(
            range.end, expected_end,
            "Selection end at ({}, {}) but expected ({}, {})",
            range.end.line, range.end.column, end_line, end_col
        );

        self
    }

    /// Asserts multiple selections for multiple cursors.
    ///
    /// Each cursor can have an optional selection. Use `None` for cursors without selections.
    /// Once multi-cursor is implemented, this will check all cursor selections in order.
    #[track_caller]
    pub fn assert_selections(self, selections: &[Option<(usize, usize, usize, usize)>]) -> Self {
        // FIXME: When multi-cursor is implemented, check all cursor selections
        assert!(
            !selections.is_empty(),
            "assert_selections requires at least one cursor"
        );

        if selections.len() == 1 {
            if let Some((start_line, start_col, end_line, end_col)) = selections[0] {
                self.assert_selection(start_line, start_col, end_line, end_col)
            } else {
                self.assert_no_selection()
            }
        } else {
            panic!(
                "Multi-cursor not yet implemented. Expected {} cursors but Stoat currently supports only 1",
                selections.len()
            );
        }
    }

    /// Asserts no selection exists.
    ///
    /// For future multi-cursor support, this checks that the primary cursor has no selection.
    #[track_caller]
    pub fn assert_no_selection(self) -> Self {
        assert!(
            self.engine.state().cursor.selection().is_none(),
            "Expected no selection but found one"
        );
        self
    }

    /// Asserts that no cursors have selections.
    ///
    /// Once multi-cursor is implemented, this will verify none of the cursors have selections.
    #[track_caller]
    pub fn assert_no_selections(self) -> Self {
        // FIXME: When multi-cursor is implemented, check all cursors
        self.assert_no_selection()
    }

    /// Asserts the dirty flag state.
    #[track_caller]
    pub fn assert_dirty(self, expected: bool) -> Self {
        assert_eq!(self.engine.is_dirty(), expected, "Dirty flag mismatch");
        self
    }

    /// Asserts that the last operation produced no effects.
    #[track_caller]
    pub fn assert_no_effects(self) -> Self {
        assert!(
            self.last_effects.is_empty(),
            "Expected no effects but got: {:?}",
            self.last_effects
        );
        self
    }

    /// Asserts that the last operation produced a specific effect.
    #[track_caller]
    pub fn assert_has_effect(self, expected: Effect) -> Self {
        assert!(
            self.last_effects.contains(&expected),
            "Expected effect {:?} not found in {:?}",
            expected,
            self.last_effects
        );
        self
    }

    /// Returns a reference to the engine for direct access.
    pub fn engine(&self) -> &EditorEngine {
        &self.engine
    }

    /// Returns a mutable reference to the engine.
    pub fn engine_mut(&mut self) -> &mut EditorEngine {
        &mut self.engine
    }

    /// Returns the last effects generated.
    pub fn last_effects(&self) -> &[Effect] {
        &self.last_effects
    }

    /// Extracts the engine, consuming the session.
    pub fn into_engine(self) -> EditorEngine {
        self.engine
    }
}

impl Default for TestSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenient event builders for testing.
pub mod events {
    use super::*;

    /// Creates a sequence of key press events from a string.
    ///
    /// Each character in the string becomes a separate key press event.
    /// This is useful for simulating typing sequences.
    pub fn type_text(text: &str) -> Vec<EditorEvent> {
        text.chars()
            .map(|ch| EditorEvent::KeyPress {
                key: ch.to_string(),
                modifiers: Modifiers::default(),
            })
            .collect()
    }

    /// Creates a key press event.
    pub fn key(key: Key) -> EditorEvent {
        EditorEvent::KeyPress {
            key,
            modifiers: Modifiers::default(),
        }
    }

    /// Creates a key press event with modifiers.
    pub fn key_with_mods(key: Key, modifiers: Modifiers) -> EditorEvent {
        EditorEvent::KeyPress { key, modifiers }
    }

    /// Creates a character key press event.
    pub fn char(ch: char) -> EditorEvent {
        EditorEvent::KeyPress {
            key: ch.to_string(),
            modifiers: Modifiers::default(),
        }
    }

    /// Creates an escape key press event.
    pub fn escape() -> EditorEvent {
        EditorEvent::KeyPress {
            key: keys::ESCAPE.to_string(),
            modifiers: Modifiers::default(),
        }
    }

    /// Creates an enter key press event.
    pub fn enter() -> EditorEvent {
        EditorEvent::KeyPress {
            key: keys::ENTER.to_string(),
            modifiers: Modifiers::default(),
        }
    }

    /// Creates a backspace key press event.
    pub fn backspace() -> EditorEvent {
        EditorEvent::KeyPress {
            key: keys::BACKSPACE.to_string(),
            modifiers: Modifiers::default(),
        }
    }

    /// Creates a mouse click event.
    pub fn click(x: f32, y: f32) -> EditorEvent {
        EditorEvent::MouseClick {
            position: Point::new(x, y),
            button: MouseButton::Left,
        }
    }

    /// Creates a paste event.
    pub fn paste(text: &str) -> EditorEvent {
        EditorEvent::TextPasted {
            content: text.to_string(),
        }
    }
}

/// Assertion helpers for testing editor state.
pub mod assertions {
    use super::*;

    /// Asserts that the editor text matches the expected content.
    #[track_caller]
    pub fn assert_text(engine: &EditorEngine, expected: &str) {
        assert_eq!(engine.text(), expected, "Editor text mismatch");
    }

    /// Asserts that the cursor is at the expected position.
    ///
    /// For future multi-cursor support, this checks the primary (first) cursor.
    #[track_caller]
    pub fn assert_cursor(engine: &EditorEngine, line: usize, column: usize) {
        let pos = engine.cursor_position();
        assert_eq!(
            pos,
            TextPosition::new(line, column),
            "Cursor position mismatch: expected ({}, {}), got ({}, {})",
            line,
            column,
            pos.line,
            pos.column
        );
    }

    /// Asserts multiple cursor positions.
    ///
    /// Once multi-cursor is implemented, this will check all cursor positions in order.
    #[track_caller]
    pub fn assert_cursors(engine: &EditorEngine, positions: &[(usize, usize)]) {
        // FIXME: When multi-cursor is implemented, check all cursors
        assert!(
            !positions.is_empty(),
            "assert_cursors requires at least one position"
        );

        if positions.len() == 1 {
            assert_cursor(engine, positions[0].0, positions[0].1);
        } else {
            panic!(
                "Multi-cursor not yet implemented. Expected {} cursors but Stoat currently supports only 1",
                positions.len()
            );
        }
    }

    /// Asserts a specific cursor position by index.
    #[track_caller]
    pub fn assert_cursor_at(engine: &EditorEngine, index: usize, line: usize, column: usize) {
        // FIXME: When multi-cursor is implemented, check cursor at index
        if index == 0 {
            assert_cursor(engine, line, column);
        } else {
            panic!(
                "Multi-cursor not yet implemented. Cannot check cursor at index {}",
                index
            );
        }
    }

    /// Asserts the number of cursors.
    #[track_caller]
    pub fn assert_cursor_count(engine: &EditorEngine, expected: usize) {
        // FIXME: When multi-cursor is implemented, check actual cursor count
        let actual = 1; // Stoat currently has exactly one cursor
        assert_eq!(
            actual, expected,
            "Expected {} cursor(s) but found {}",
            expected, actual
        );
    }

    /// Asserts that the editor is in the expected mode.
    #[track_caller]
    pub fn assert_mode(engine: &EditorEngine, expected: EditMode) {
        assert_eq!(engine.mode(), expected, "Editor mode mismatch");
    }

    /// Asserts that the editor dirty flag matches expected value.
    #[track_caller]
    pub fn assert_dirty(engine: &EditorEngine, expected: bool) {
        assert_eq!(engine.is_dirty(), expected, "Editor dirty flag mismatch");
    }

    /// Asserts that a selection exists with the given range.
    ///
    /// For future multi-cursor support, this checks the primary (first) cursor's selection.
    #[track_caller]
    pub fn assert_selection(
        engine: &EditorEngine,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
    ) {
        let selection = engine.state().cursor.selection();
        assert!(selection.is_some(), "Expected selection but none exists");

        let range = selection.unwrap();
        let expected_start = TextPosition::new(start_line, start_col);
        let expected_end = TextPosition::new(end_line, end_col);

        assert_eq!(
            range.start, expected_start,
            "Selection start mismatch: expected ({}, {}), got ({}, {})",
            start_line, start_col, range.start.line, range.start.column
        );

        assert_eq!(
            range.end, expected_end,
            "Selection end mismatch: expected ({}, {}), got ({}, {})",
            end_line, end_col, range.end.line, range.end.column
        );
    }

    /// Asserts multiple selections for multiple cursors.
    #[track_caller]
    pub fn assert_selections(
        engine: &EditorEngine,
        selections: &[Option<(usize, usize, usize, usize)>],
    ) {
        // FIXME: When multi-cursor is implemented, check all cursor selections
        assert!(
            !selections.is_empty(),
            "assert_selections requires at least one cursor"
        );

        if selections.len() == 1 {
            if let Some((start_line, start_col, end_line, end_col)) = selections[0] {
                assert_selection(engine, start_line, start_col, end_line, end_col);
            } else {
                assert_no_selection(engine);
            }
        } else {
            panic!(
                "Multi-cursor not yet implemented. Expected {} cursors but Stoat currently supports only 1",
                selections.len()
            );
        }
    }

    /// Asserts that no selection exists.
    ///
    /// For future multi-cursor support, this checks the primary cursor.
    #[track_caller]
    pub fn assert_no_selection(engine: &EditorEngine) {
        assert!(
            engine.state().cursor.selection().is_none(),
            "Expected no selection but found one"
        );
    }

    /// Asserts that no cursors have selections.
    #[track_caller]
    pub fn assert_no_selections(engine: &EditorEngine) {
        // FIXME: When multi-cursor is implemented, check all cursors
        assert_no_selection(engine);
    }

    /// Asserts that the given effects contain a specific effect.
    #[track_caller]
    pub fn assert_has_effect(effects: &[Effect], expected: &Effect) {
        assert!(
            effects.contains(expected),
            "Expected effect {expected:?} not found in {effects:?}"
        );
    }

    /// Asserts that no effects were generated.
    #[track_caller]
    pub fn assert_no_effects(effects: &[Effect]) {
        assert!(
            effects.is_empty(),
            "Expected no effects, but got: {effects:?}"
        );
    }

    /// Asserts that effects contain an error message.
    #[track_caller]
    pub fn assert_error_effect(effects: &[Effect], message_contains: &str) {
        let has_error = effects.iter().any(|effect| {
            matches!(effect, Effect::ShowError { message } if message.contains(message_contains))
        });
        assert!(
            has_error,
            "Expected error effect containing '{message_contains}', got: {effects:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{EditMode, TextPosition};

    #[test]
    fn state_builder_creates_expected_state() {
        let state = StateBuilder::new()
            .with_text("Hello\nWorld")
            .with_cursor(1, 2)
            .in_mode(EditMode::Insert)
            .dirty(true)
            .build();

        assert_eq!(state.text(), "Hello\nWorld");
        assert_eq!(state.cursor_position(), TextPosition::new(1, 2));
        assert_eq!(state.mode, EditMode::Insert);
        assert!(state.is_dirty);
    }

    #[test]
    fn type_text_creates_char_events() {
        let events = events::type_text("Hello");
        assert_eq!(events.len(), 5);

        for (i, ch) in "Hello".chars().enumerate() {
            if let EditorEvent::KeyPress { key, .. } = &events[i] {
                assert_eq!(*key, ch.to_string());
            } else {
                panic!("Expected KeyPress event");
            }
        }
    }

    #[test]
    fn assertion_helpers_work() {
        let engine = EngineBuilder::new()
            .with_text("test")
            .with_cursor(0, 2)
            .build();

        assertions::assert_text(&engine, "test");
        assertions::assert_cursor(&engine, 0, 2);
        assertions::assert_mode(&engine, EditMode::Normal);
        assertions::assert_dirty(&engine, false);
    }

    #[test]
    fn test_session_fluent_api() {
        // Demonstrates clean, low-LOC test writing with TestSession
        TestSession::new()
            .with_text("hello world")
            .cursor(0, 5)
            .assert_cursor(0, 5)
            .type_keys("i")
            .assert_mode(EditMode::Insert)
            .type_text(" beautiful")
            .assert_text("hello beautiful world")
            .type_keys("<Esc>")
            .assert_mode(EditMode::Normal);
    }

    #[test]
    fn test_session_chaining() {
        // Shows how assertions and actions can be chained
        TestSession::new()
            .with_text("line1\nline2\nline3")
            .cursor(1, 0)
            .type_keys("i")
            .type_text("new ")
            .assert_text("line1\nnew line2\nline3")
            .assert_cursor(1, 4)
            .type_keys("<Esc>")
            .assert_cursor(1, 4);
    }

    #[test]
    fn multi_cursor_api_single_cursor() {
        // The multi-cursor API should work seamlessly with single cursors
        TestSession::new()
            .with_text("hello world")
            .assert_cursor_count(1)
            .assert_cursors(&[(0, 0)])
            .assert_cursor_at(0, 0, 0)
            .type_keys("i")
            .type_text("Hi ")
            .assert_text("Hi hello world")
            .assert_cursors(&[(0, 3)]);
    }

    #[test]
    #[ignore = "Multi-cursor not yet implemented"]
    fn multi_cursor_api_multiple_cursors() {
        // Demonstrates how multiple cursors would be tested
        // FIXME: Requires implementing multi-cursor support
        TestSession::new()
            .with_text("foo\nbar\nbaz")
            .assert_cursor_count(3) // Would fail - only 1 cursor currently
            .assert_cursors(&[(0, 0), (1, 0), (2, 0)]) // Cursor at start of each line
            .type_keys("i")
            .type_text("prefix_")
            .assert_text("prefix_foo\nprefix_bar\nprefix_baz")
            .assert_cursors(&[(0, 7), (1, 7), (2, 7)]);
    }

    #[test]
    #[ignore = "Multi-cursor not yet implemented"]
    fn multi_cursor_with_selections() {
        // Demonstrates testing multiple cursors with different selections
        // FIXME: Requires implementing multi-cursor and multi-selection support
        TestSession::new()
            .with_text("one two three\nfour five six\nseven eight nine")
            .assert_selections(&[
                Some((0, 0, 0, 3)),  // "one" selected
                None,                // Second cursor has no selection
                Some((2, 6, 2, 11)), // "eight" selected
            ])
            .type_keys("d") // Delete selections
            .assert_text(" two three\nfour five six\nseven  nine")
            .assert_no_selections();
    }

    #[test]
    #[ignore = "Multi-cursor not yet implemented"]
    fn multi_cursor_specific_cursor_assertions() {
        // Test asserting specific cursors by index
        // FIXME: Requires implementing multi-cursor support
        TestSession::new()
            .with_text("line1\nline2\nline3")
            .assert_cursor_at(0, 0, 0) // First cursor at line 0, col 0
            .assert_cursor_at(1, 1, 0) // Second cursor at line 1, col 0
            .assert_cursor_at(2, 2, 0) // Third cursor at line 2, col 0
            .type_keys("<C-d>") // Some command to move specific cursor
            .assert_cursor_at(1, 1, 5); // Check second cursor moved
    }

    #[test]
    fn multi_cursor_api_backwards_compatible() {
        // Existing single-cursor API continues to work
        TestSession::new()
            .with_text("test")
            .assert_cursor(0, 0) // Old API still works
            .assert_no_selection() // Old API for no selection
            .assert_cursors(&[(0, 0)]) // New API also works with single cursor
            .assert_selections(&[None]); // New API for no selection
    }
}
