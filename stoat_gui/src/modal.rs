//! Simple modal input system for Stoat editor.
//!
//! This module provides a minimal modal editing system with just two modes:
//! Normal mode for commands and Insert mode for text input.

use tracing::{debug, info};

/// The editor modes supported by the modal system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorMode {
    /// Normal mode - default command mode
    Normal,
    /// Insert mode - for text insertion
    Insert,
}

impl EditorMode {
    /// Returns the string representation of the mode for display
    pub fn as_str(&self) -> &'static str {
        match self {
            EditorMode::Normal => "NORMAL",
            EditorMode::Insert => "INSERT",
        }
    }
}

impl Default for EditorMode {
    fn default() -> Self {
        EditorMode::Normal
    }
}

/// Actions that can result from modal key handling
#[derive(Debug, PartialEq)]
pub enum ModalAction {
    /// No action needed
    None,
    /// Mode changed, UI should update
    ModeChanged,
    /// Insert the given text
    InsertText(String),
    /// Quit the application
    Quit,
}

/// Simple modal input handler that manages editor modes and key processing
pub struct ModalHandler {
    mode: EditorMode,
}

impl ModalHandler {
    /// Creates a new modal handler starting in Normal mode
    pub fn new() -> Self {
        Self {
            mode: EditorMode::Normal,
        }
    }

    /// Returns the current editor mode
    pub fn current_mode(&self) -> EditorMode {
        self.mode
    }

    /// Handles a key press and returns the appropriate action
    ///
    /// # Arguments
    /// * `key` - The key that was pressed as a string
    ///
    /// # Returns
    /// The action that should be taken in response to the key press
    pub fn handle_key(&mut self, key: &str) -> ModalAction {
        debug!("Processing key '{}' in mode {:?}", key, self.mode);

        let action = match self.mode {
            EditorMode::Normal => match key {
                "i" => {
                    info!("Switching from Normal to Insert mode");
                    self.mode = EditorMode::Insert;
                    ModalAction::ModeChanged
                },
                "escape" => {
                    info!("Quit requested from Normal mode");
                    ModalAction::Quit
                },
                _ => {
                    debug!("Ignoring key '{}' in Normal mode", key);
                    ModalAction::None
                },
            },
            EditorMode::Insert => match key {
                "escape" => {
                    info!("Switching from Insert to Normal mode");
                    self.mode = EditorMode::Normal;
                    ModalAction::ModeChanged
                },
                _ => {
                    debug!("Inserting text '{}' in Insert mode", key);
                    ModalAction::InsertText(key.to_string())
                },
            },
        };

        debug!("Action result: {:?}, new mode: {:?}", action, self.mode);
        action
    }

    /// Switches to Normal mode (for external use)
    pub fn switch_to_normal(&mut self) -> ModalAction {
        if self.mode != EditorMode::Normal {
            self.mode = EditorMode::Normal;
            ModalAction::ModeChanged
        } else {
            ModalAction::None
        }
    }

    /// Switches to Insert mode (for external use)
    pub fn switch_to_insert(&mut self) -> ModalAction {
        if self.mode != EditorMode::Insert {
            self.mode = EditorMode::Insert;
            ModalAction::ModeChanged
        } else {
            ModalAction::None
        }
    }
}

impl Default for ModalHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_mode() {
        let handler = ModalHandler::new();
        assert_eq!(handler.current_mode(), EditorMode::Normal);
    }

    #[test]
    fn test_normal_mode_keys() {
        let mut handler = ModalHandler::new();

        // Test 'i' switches to insert mode
        let action = handler.handle_key("i");
        assert_eq!(action, ModalAction::ModeChanged);
        assert_eq!(handler.current_mode(), EditorMode::Insert);

        // Reset to normal
        handler.switch_to_normal();

        // Test 'escape' quits
        let action = handler.handle_key("escape");
        assert_eq!(action, ModalAction::Quit);

        // Test other keys do nothing
        let action = handler.handle_key("j");
        assert_eq!(action, ModalAction::None);
    }

    #[test]
    fn test_insert_mode_keys() {
        let mut handler = ModalHandler::new();
        handler.switch_to_insert();

        // Test 'escape' returns to normal
        let action = handler.handle_key("escape");
        assert_eq!(action, ModalAction::ModeChanged);
        assert_eq!(handler.current_mode(), EditorMode::Normal);

        // Switch back to insert
        handler.switch_to_insert();

        // Test other keys insert text
        let action = handler.handle_key("a");
        assert_eq!(action, ModalAction::InsertText("a".to_string()));

        let action = handler.handle_key("hello");
        assert_eq!(action, ModalAction::InsertText("hello".to_string()));
    }

    #[test]
    fn test_mode_strings() {
        assert_eq!(EditorMode::Normal.as_str(), "NORMAL");
        assert_eq!(EditorMode::Insert.as_str(), "INSERT");
    }
}
