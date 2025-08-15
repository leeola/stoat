//! Testing utilities and helpers for stoat_v2.
//!
//! This module provides convenient builders, assertion helpers, and other
//! utilities to make testing editor functionality easier and more readable.

use crate::{
    actions::{EditMode, TextPosition, TextRange},
    effects::Effect,
    engine::EditorEngine,
    events::EditorEvent,
    state::EditorState,
};
use iced::{keyboard, mouse};
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
        self.state.cursor.position = TextPosition::new(line, column);
        self.state.cursor.desired_column = column;
        self
    }

    /// Sets the editing mode.
    pub fn in_mode(mut self, mode: EditMode) -> Self {
        self.state.mode = mode;
        self
    }

    /// Sets text selection range.
    pub fn with_selection(mut self, start: TextPosition, end: TextPosition) -> Self {
        self.state.cursor.selection = Some(TextRange::new(start, end));
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
        self.state.cursor.position = TextPosition::new(line, column);
        self.state.cursor.desired_column = column;
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
                key: keyboard::Key::Character(ch.to_string().into()),
                modifiers: keyboard::Modifiers::default(),
            })
            .collect()
    }

    /// Creates a vim command sequence.
    ///
    /// Parses a vim command string and returns the corresponding events.
    /// Special keys can be represented as `<Esc>`, `<Enter>`, etc.
    pub fn vim_sequence(cmd: &str) -> Vec<EditorEvent> {
        let mut events = Vec::new();
        let mut chars = cmd.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '<' {
                // Handle special key sequences like <Esc>, <Enter>
                let mut key_name = String::new();
                while let Some(&next_ch) = chars.peek() {
                    if next_ch == '>' {
                        chars.next(); // consume '>'
                        break;
                    }
                    key_name.push(chars.next().unwrap());
                }

                let key = match key_name.as_str() {
                    "Esc" | "Escape" => keyboard::Key::Named(keyboard::key::Named::Escape),
                    "Enter" | "Return" => keyboard::Key::Named(keyboard::key::Named::Enter),
                    "Backspace" | "BS" => keyboard::Key::Named(keyboard::key::Named::Backspace),
                    "Tab" => keyboard::Key::Named(keyboard::key::Named::Tab),
                    "Space" => keyboard::Key::Named(keyboard::key::Named::Space),
                    "Left" => keyboard::Key::Named(keyboard::key::Named::ArrowLeft),
                    "Right" => keyboard::Key::Named(keyboard::key::Named::ArrowRight),
                    "Up" => keyboard::Key::Named(keyboard::key::Named::ArrowUp),
                    "Down" => keyboard::Key::Named(keyboard::key::Named::ArrowDown),
                    _ => keyboard::Key::Character(ch.to_string().into()), // Fallback
                };

                events.push(EditorEvent::KeyPress {
                    key,
                    modifiers: keyboard::Modifiers::default(),
                });
            } else {
                events.push(EditorEvent::KeyPress {
                    key: keyboard::Key::Character(ch.to_string().into()),
                    modifiers: keyboard::Modifiers::default(),
                });
            }
        }

        events
    }

    /// Creates a key press event.
    pub fn key(key: keyboard::Key) -> EditorEvent {
        EditorEvent::KeyPress {
            key,
            modifiers: keyboard::Modifiers::default(),
        }
    }

    /// Creates a key press event with modifiers.
    pub fn key_with_mods(key: keyboard::Key, modifiers: keyboard::Modifiers) -> EditorEvent {
        EditorEvent::KeyPress { key, modifiers }
    }

    /// Creates a character key press event.
    pub fn char(ch: char) -> EditorEvent {
        EditorEvent::KeyPress {
            key: keyboard::Key::Character(ch.to_string().into()),
            modifiers: keyboard::Modifiers::default(),
        }
    }

    /// Creates an escape key press event.
    pub fn escape() -> EditorEvent {
        EditorEvent::KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modifiers: keyboard::Modifiers::default(),
        }
    }

    /// Creates an enter key press event.
    pub fn enter() -> EditorEvent {
        EditorEvent::KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modifiers: keyboard::Modifiers::default(),
        }
    }

    /// Creates a backspace key press event.
    pub fn backspace() -> EditorEvent {
        EditorEvent::KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Backspace),
            modifiers: keyboard::Modifiers::default(),
        }
    }

    /// Creates a mouse click event.
    pub fn click(x: f32, y: f32) -> EditorEvent {
        EditorEvent::MouseClick {
            position: iced::Point::new(x, y),
            button: mouse::Button::Left,
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
    pub fn assert_text(engine: &EditorEngine, expected: &str) {
        assert_eq!(engine.text(), expected, "Editor text mismatch");
    }

    /// Asserts that the cursor is at the expected position.
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

    /// Asserts that the editor is in the expected mode.
    pub fn assert_mode(engine: &EditorEngine, expected: EditMode) {
        assert_eq!(engine.mode(), expected, "Editor mode mismatch");
    }

    /// Asserts that the editor dirty flag matches expected value.
    pub fn assert_dirty(engine: &EditorEngine, expected: bool) {
        assert_eq!(engine.is_dirty(), expected, "Editor dirty flag mismatch");
    }

    /// Asserts that the given effects contain a specific effect.
    pub fn assert_has_effect(effects: &[Effect], expected: &Effect) {
        assert!(
            effects.contains(expected),
            "Expected effect {:?} not found in {:?}",
            expected,
            effects
        );
    }

    /// Asserts that no effects were generated.
    pub fn assert_no_effects(effects: &[Effect]) {
        assert!(
            effects.is_empty(),
            "Expected no effects, but got: {:?}",
            effects
        );
    }

    /// Asserts that effects contain an error message.
    pub fn assert_error_effect(effects: &[Effect], message_contains: &str) {
        let has_error = effects.iter().any(|effect| {
            matches!(effect, Effect::ShowError { message } if message.contains(message_contains))
        });
        assert!(
            has_error,
            "Expected error effect containing '{}', got: {:?}",
            message_contains, effects
        );
    }
}

/// Test scenario helpers for common editing workflows.
pub mod scenarios {
    use super::*;

    /// Executes a complete vim command sequence and returns final state.
    ///
    /// This simulates typing a vim command sequence starting from normal mode,
    /// useful for integration testing of complex operations.
    pub fn execute_vim_command(initial_text: &str, command: &str) -> (EditorEngine, Vec<Effect>) {
        let mut engine = EngineBuilder::new()
            .with_text(initial_text)
            .in_mode(EditMode::Normal)
            .build();

        let events = events::vim_sequence(command);
        let effects = engine.handle_events(events);

        (engine, effects)
    }

    /// Simulates typing text in insert mode.
    pub fn type_in_insert_mode(initial_text: &str, to_type: &str) -> (EditorEngine, Vec<Effect>) {
        let mut engine = EngineBuilder::new()
            .with_text(initial_text)
            .in_mode(EditMode::Insert)
            .build();

        let events = events::type_text(to_type);
        let effects = engine.handle_events(events);

        (engine, effects)
    }

    /// Tests a complete editing workflow: enter insert mode, type, exit.
    pub fn edit_workflow(
        initial_text: &str,
        cursor_line: usize,
        cursor_col: usize,
        text_to_insert: &str,
    ) -> EditorEngine {
        let mut engine = EngineBuilder::new()
            .with_text(initial_text)
            .with_cursor(cursor_line, cursor_col)
            .in_mode(EditMode::Normal)
            .build();

        // Enter insert mode
        engine.handle_event(events::char('i'));

        // Type text
        let type_events = events::type_text(text_to_insert);
        engine.handle_events(type_events);

        // Exit insert mode
        engine.handle_event(events::escape());

        engine
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
    fn vim_sequence_parsing() {
        let events = events::vim_sequence("i<Esc>:wq<Enter>");
        assert_eq!(events.len(), 6); // i, Esc, :, w, q, Enter

        // Check first event is 'i'
        if let EditorEvent::KeyPress { key, .. } = &events[0] {
            assert_eq!(*key, keyboard::Key::Character("i".into()));
        } else {
            panic!("Expected KeyPress event");
        }

        // Check second event is Escape
        if let EditorEvent::KeyPress { key, .. } = &events[1] {
            assert_eq!(*key, keyboard::Key::Named(keyboard::key::Named::Escape));
        } else {
            panic!("Expected KeyPress event");
        }
    }

    #[test]
    fn type_text_creates_char_events() {
        let events = events::type_text("Hello");
        assert_eq!(events.len(), 5);

        for (i, ch) in "Hello".chars().enumerate() {
            if let EditorEvent::KeyPress { key, .. } = &events[i] {
                assert_eq!(*key, keyboard::Key::Character(ch.to_string().into()));
            } else {
                panic!("Expected KeyPress event");
            }
        }
    }

    #[test]
    fn execute_vim_command_scenario() {
        let (engine, _effects) = scenarios::execute_vim_command("hello world", "iwow <Esc>");
        assertions::assert_text(&engine, "wow hello world");
        assertions::assert_mode(&engine, EditMode::Normal);
    }

    #[test]
    fn edit_workflow_scenario() {
        let engine = scenarios::edit_workflow("line1\nline2", 1, 0, "NEW ");
        assertions::assert_text(&engine, "line1\nNEW line2");
        assertions::assert_mode(&engine, EditMode::Normal);
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
}
