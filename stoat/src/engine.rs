//! Editor engine for stateful event processing.
//!
//! The [`EditorEngine`] provides a convenient stateful wrapper around the pure
//! event processing functions. This is the main API that GUI applications
//! will use to interact with the editor core.

use crate::{
    effects::Effect, events::EditorEvent, keymap::Keymap, processor::process_event,
    state::EditorState,
};

/// Stateful editor engine that manages state and processes events.
///
/// The engine provides a convenient API for GUI applications by managing
/// the editor state internally and providing methods to handle events
/// and access current state.
///
/// # Example
///
/// ```rust
/// use stoat::*;
/// use iced::keyboard;
///
/// let mut engine = EditorEngine::new();
///
/// // Enter insert mode first
/// engine.handle_event(EditorEvent::KeyPress {
///     key: keyboard::Key::Character("i".to_string().into()),
///     modifiers: keyboard::Modifiers::default()
/// });
///
/// // Type a character
/// engine.handle_event(EditorEvent::KeyPress {
///     key: keyboard::Key::Character("H".to_string().into()),
///     modifiers: keyboard::Modifiers::default()
/// });
///
/// assert_eq!(engine.text(), "H");
/// ```
pub struct EditorEngine {
    state: EditorState,
    keymap: Keymap,
}

impl EditorEngine {
    /// Creates a new editor engine with empty state.
    pub fn new() -> Self {
        tracing::info!("Creating new EditorEngine with empty state");
        Self {
            state: EditorState::new(),
            keymap: Keymap::new(),
        }
    }

    /// Creates a new editor engine with the given initial text.
    pub fn with_text(text: &str) -> Self {
        tracing::info!(
            "Creating new EditorEngine with {} characters of text",
            text.len()
        );
        Self {
            state: EditorState::with_text(text),
            keymap: Keymap::new(),
        }
    }

    /// Creates a new editor engine with the given initial state.
    pub fn with_state(state: EditorState) -> Self {
        Self {
            state,
            keymap: Keymap::new(),
        }
    }

    /// Handles an event and returns any effects that should be executed.
    ///
    /// This is the main entry point for processing user input and other events.
    /// The engine will update its internal state and return a list of effects
    /// that the caller should execute (like file I/O, clipboard operations, etc.).
    ///
    /// # Arguments
    ///
    /// * `event` - The event to process
    ///
    /// # Returns
    ///
    /// Vector of effects that should be executed by the caller
    pub fn handle_event(&mut self, event: EditorEvent) -> Vec<Effect> {
        tracing::debug!("Engine handling event: {:?}", event);

        let (new_state, effects) = process_event(self.state.clone(), event, &self.keymap);

        // Log state changes
        if self.state.is_dirty != new_state.is_dirty {
            tracing::debug!(
                "Dirty state changed: {} -> {}",
                self.state.is_dirty,
                new_state.is_dirty
            );
        }

        self.state = new_state;

        if !effects.is_empty() {
            tracing::debug!("Engine generated {} effects", effects.len());
        }

        effects
    }

    /// Returns a reference to the current editor state.
    ///
    /// This provides read-only access to the complete editor state for
    /// rendering and other purposes.
    pub fn state(&self) -> &EditorState {
        &self.state
    }

    /// Returns the current text content of the buffer.
    pub fn text(&self) -> &str {
        self.state.text()
    }

    /// Returns whether the buffer has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.state.is_dirty
    }

    /// Returns the current cursor position.
    pub fn cursor_position(&self) -> crate::actions::TextPosition {
        self.state.cursor_position()
    }

    /// Returns the current editing mode.
    pub fn mode(&self) -> crate::actions::EditMode {
        self.state.mode
    }

    /// Returns the current file path, if any.
    pub fn file_path(&self) -> Option<&std::path::Path> {
        self.state.file.path.as_deref()
    }

    /// Returns the display name for the current file.
    pub fn file_name(&self) -> &str {
        &self.state.file.name
    }

    /// Returns the number of lines in the buffer.
    pub fn line_count(&self) -> usize {
        self.state.line_count()
    }

    /// Returns a specific line from the buffer.
    pub fn line(&self, index: usize) -> Option<&str> {
        self.state.line(index)
    }

    /// Returns all lines as an iterator.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.state.buffer.lines()
    }

    /// Replaces the engine's state entirely.
    ///
    /// This is useful for implementing undo/redo or loading saved state.
    pub fn set_state(&mut self, state: EditorState) {
        self.state = state;
    }

    /// Processes a sequence of events and returns all accumulated effects.
    ///
    /// This is a convenience method for handling multiple events at once,
    /// such as when processing a batch of simulated input.
    ///
    /// # Arguments
    ///
    /// * `events` - Iterator of events to process
    ///
    /// # Returns
    ///
    /// Vector of all effects from processing all events
    pub fn handle_events<I>(&mut self, events: I) -> Vec<Effect>
    where
        I: IntoIterator<Item = EditorEvent>,
    {
        let mut all_effects = Vec::new();

        for event in events {
            let effects = self.handle_event(event);
            all_effects.extend(effects);
        }

        all_effects
    }

    /// Creates a snapshot of the current state.
    ///
    /// The returned state can be used later with [`set_state`] to restore
    /// the editor to this exact point, enabling undo/redo functionality.
    pub fn snapshot(&self) -> EditorState {
        self.state.clone()
    }
}

impl Default for EditorEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EditorEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorEngine")
            .field("state", &self.state)
            .finish()
    }
}

/// Convenience functions for creating common events.
///
/// These functions make it easier to create events for testing and simulation.
pub mod events {
    use crate::events::EditorEvent;
    use iced::{keyboard, mouse};

    /// Creates a key press event.
    pub fn key_press(key: keyboard::Key, modifiers: keyboard::Modifiers) -> EditorEvent {
        EditorEvent::KeyPress { key, modifiers }
    }

    /// Creates a key press event for a character with no modifiers.
    pub fn char_key(ch: char) -> EditorEvent {
        EditorEvent::KeyPress {
            key: keyboard::Key::Character(ch.to_string().into()),
            modifiers: keyboard::Modifiers::default(),
        }
    }

    /// Creates a key press event for a named key with no modifiers.
    pub fn named_key(key: keyboard::key::Named) -> EditorEvent {
        EditorEvent::KeyPress {
            key: keyboard::Key::Named(key),
            modifiers: keyboard::Modifiers::default(),
        }
    }

    /// Creates a text paste event.
    pub fn paste(content: &str) -> EditorEvent {
        EditorEvent::TextPasted {
            content: content.to_string(),
        }
    }

    /// Creates a mouse click event.
    pub fn mouse_click(x: f32, y: f32) -> EditorEvent {
        EditorEvent::MouseClick {
            position: iced::Point::new(x, y),
            button: mouse::Button::Left,
        }
    }

    /// Creates a new file event.
    pub fn new_file() -> EditorEvent {
        EditorEvent::NewFile
    }

    /// Creates an exit event.
    pub fn exit() -> EditorEvent {
        EditorEvent::Exit
    }

    /// Creates a viewport resize event.
    pub fn resize(width: f32, height: f32) -> EditorEvent {
        EditorEvent::Resize { width, height }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{EditMode, TextPosition};
    use iced::keyboard;

    #[test]
    fn new_engine_starts_empty() {
        let engine = EditorEngine::new();
        assert_eq!(engine.text(), "");
        assert_eq!(engine.cursor_position(), TextPosition::start());
        assert_eq!(engine.mode(), EditMode::Normal);
        assert!(!engine.is_dirty());
    }

    #[test]
    fn with_text_sets_initial_content() {
        let engine = EditorEngine::with_text("Hello, world!");
        assert_eq!(engine.text(), "Hello, world!");
        assert_eq!(engine.line_count(), 1);
    }

    #[test]
    fn handle_insert_mode_typing() {
        let mut engine = EditorEngine::new();

        // Enter insert mode
        let effects = engine.handle_event(events::char_key('i'));
        assert!(effects.is_empty());
        assert_eq!(engine.mode(), EditMode::Insert);

        // Type some text
        let effects = engine.handle_event(events::char_key('H'));
        assert!(effects.is_empty());
        assert_eq!(engine.text(), "H");

        let effects = engine.handle_event(events::char_key('i'));
        assert!(effects.is_empty());
        assert_eq!(engine.text(), "Hi");

        // Exit insert mode
        let effects = engine.handle_event(events::named_key(keyboard::key::Named::Escape));
        assert!(effects.is_empty());
        assert_eq!(engine.mode(), EditMode::Normal);
    }

    #[test]
    fn handle_multiple_events() {
        let mut engine = EditorEngine::new();

        let events = vec![
            events::char_key('i'), // Enter insert mode
            events::char_key('H'), // Type 'H'
            events::char_key('e'), // Type 'e'
            events::char_key('l'), // Type 'l'
            events::char_key('l'), // Type 'l'
            events::char_key('o'), // Type 'o'
        ];

        let effects = engine.handle_events(events);
        assert!(effects.is_empty());
        assert_eq!(engine.text(), "Hello");
        assert_eq!(engine.mode(), EditMode::Insert);
    }

    #[test]
    fn snapshot_and_restore() {
        let mut engine = EditorEngine::with_text("Original");
        let snapshot = engine.snapshot();

        // Modify the state
        engine.handle_event(events::char_key('i'));
        engine.handle_event(events::char_key('X'));
        assert_ne!(engine.text(), "Original");

        // Restore from snapshot
        engine.set_state(snapshot);
        assert_eq!(engine.text(), "Original");
        assert_eq!(engine.mode(), EditMode::Normal);
    }

    #[test]
    fn cursor_movement_in_normal_mode() {
        let mut engine = EditorEngine::with_text("Hello\nWorld");
        assert_eq!(engine.cursor_position(), TextPosition::new(0, 0));

        // Move right
        engine.handle_event(events::char_key('l'));
        assert_eq!(engine.cursor_position(), TextPosition::new(0, 1));

        // Move down
        engine.handle_event(events::char_key('j'));
        assert_eq!(engine.cursor_position(), TextPosition::new(1, 1));

        // Move left
        engine.handle_event(events::char_key('h'));
        assert_eq!(engine.cursor_position(), TextPosition::new(1, 0));

        // Move up
        engine.handle_event(events::char_key('k'));
        assert_eq!(engine.cursor_position(), TextPosition::new(0, 0));
    }
}
