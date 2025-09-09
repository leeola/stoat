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
///
/// let mut engine = EditorEngine::new();
///
/// // Enter insert mode first
/// engine.handle_event(EditorEvent::KeyPress {
///     key: "i".to_string(),
///     modifiers: input::Modifiers::default()
/// });
///
/// // Type a character
/// engine.handle_event(EditorEvent::KeyPress {
///     key: "H".to_string(),
///     modifiers: input::Modifiers::default()
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
    pub fn text(&self) -> String {
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
    pub fn line(&self, index: usize) -> Option<String> {
        self.state.line(index)
    }

    /// Returns the keymap for querying available commands.
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Returns whether command info should be displayed.
    pub fn show_command_info(&self) -> bool {
        self.state.show_command_info
    }

    /// Returns all lines as an iterator.
    pub fn lines(&self) -> impl Iterator<Item = String> + use<'_> {
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
    use crate::{
        events::EditorEvent,
        input::{keys, Key, Modifiers, MouseButton},
    };

    /// Creates a key press event.
    pub fn key_press(key: Key, modifiers: Modifiers) -> EditorEvent {
        EditorEvent::KeyPress { key, modifiers }
    }

    /// Creates a key press event for a character with no modifiers.
    pub fn char_key(ch: char) -> EditorEvent {
        EditorEvent::KeyPress {
            key: ch.to_string(),
            modifiers: Modifiers::default(),
        }
    }

    /// Creates a key press event for a named key with no modifiers.
    pub fn named_key(key_name: &str) -> EditorEvent {
        EditorEvent::KeyPress {
            key: key_name.to_string(),
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

    /// Creates a text paste event.
    pub fn paste(content: &str) -> EditorEvent {
        EditorEvent::TextPasted {
            content: content.to_string(),
        }
    }

    /// Creates a mouse click event.
    pub fn mouse_click(x: f32, y: f32) -> EditorEvent {
        EditorEvent::MouseClick {
            position: gpui::point(x, y),
            button: MouseButton::Left,
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
    use crate::{actions::EditMode, Stoat};

    #[test]
    fn new_engine_starts_empty() {
        Stoat::test()
            .assert_text("")
            .assert_cursor(0, 0)
            .assert_mode(EditMode::Normal)
            .assert_dirty(false);
    }

    #[test]
    fn with_text_sets_initial_content() {
        Stoat::test()
            .with_text("Hello, world!")
            .assert_text("Hello, world!");
    }

    #[test]
    fn snapshot_and_restore() {
        let mut stoat = Stoat::with_text("Original");
        let snapshot = stoat.engine().snapshot();

        stoat.keyboard_input("iX");
        assert_ne!(stoat.buffer_contents(), "Original");

        stoat.engine_mut().set_state(snapshot);
        assert_eq!(stoat.buffer_contents(), "Original");
        assert_eq!(stoat.engine().mode(), EditMode::Normal);
    }
}
