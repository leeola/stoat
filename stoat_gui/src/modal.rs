//! Simple modal input system for Stoat editor.
//!
//! This module provides a minimal modal editing system with just two modes:
//! Normal mode for commands and Insert mode for text input.
//!
//! The modal system transforms key input into editor commands that can be
//! executed by the command system.

use gpui::Action;
use stoat::actions::*;
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

/// Result of modal key handling - either a command to execute or no action
#[derive(Debug)]
pub enum ModalResult {
    /// No command to execute
    None,
    /// Execute the given command
    Command(Box<dyn Action>),
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

    /// Handles a key press and returns the appropriate command
    ///
    /// # Arguments
    /// * `key` - The key that was pressed as a string
    ///
    /// # Returns
    /// The command that should be executed in response to the key press
    pub fn handle_key(&mut self, key: &str) -> ModalResult {
        debug!("Processing key '{}' in mode {:?}", key, self.mode);

        let result = match self.mode {
            EditorMode::Normal => match key {
                "i" => {
                    info!("Switching from Normal to Insert mode");
                    self.mode = EditorMode::Insert;
                    ModalResult::Command(Box::new(EnterInsertMode))
                },
                "escape" => {
                    info!("Exit app requested from Normal mode");
                    ModalResult::Command(Box::new(ExitApp))
                },
                "h" => {
                    debug!("Move left in Normal mode");
                    ModalResult::Command(Box::new(MoveLeft))
                },
                "j" => {
                    debug!("Move down in Normal mode");
                    ModalResult::Command(Box::new(MoveDown))
                },
                "k" => {
                    debug!("Move up in Normal mode");
                    ModalResult::Command(Box::new(MoveUp))
                },
                "l" => {
                    debug!("Move right in Normal mode");
                    ModalResult::Command(Box::new(MoveRight))
                },
                "ctrl-f" | "pagedown" => {
                    debug!("Page down in Normal mode");
                    ModalResult::Command(Box::new(PageDown))
                },
                "ctrl-b" | "pageup" => {
                    debug!("Page up in Normal mode");
                    ModalResult::Command(Box::new(PageUp))
                },
                "0" => {
                    debug!("Move to line start in Normal mode");
                    ModalResult::Command(Box::new(MoveToLineStart))
                },
                "$" => {
                    debug!("Move to line end in Normal mode");
                    ModalResult::Command(Box::new(MoveToLineEnd))
                },
                "g" => {
                    // FIXME: Should handle gg for file start
                    debug!("Move to file start in Normal mode");
                    ModalResult::Command(Box::new(MoveToFileStart))
                },
                "G" => {
                    debug!("Move to file end in Normal mode");
                    ModalResult::Command(Box::new(MoveToFileEnd))
                },
                "x" => {
                    debug!("Delete character in Normal mode");
                    ModalResult::Command(Box::new(DeleteRight))
                },
                "d" => {
                    // FIXME: Should handle dd for delete line
                    debug!("Delete line in Normal mode");
                    ModalResult::Command(Box::new(DeleteLine))
                },
                "D" => {
                    debug!("Delete to end of line in Normal mode");
                    ModalResult::Command(Box::new(DeleteToEndOfLine))
                },
                _ => {
                    debug!("Ignoring key '{}' in Normal mode", key);
                    ModalResult::None
                },
            },
            EditorMode::Insert => match key {
                "escape" => {
                    info!("Switching from Insert to Normal mode");
                    self.mode = EditorMode::Normal;
                    ModalResult::Command(Box::new(EnterNormalMode))
                },
                "enter" => {
                    debug!("Inserting newline in Insert mode");
                    ModalResult::Command(Box::new(InsertText("\n".to_string())))
                },
                "tab" => {
                    debug!("Inserting tab in Insert mode");
                    ModalResult::Command(Box::new(InsertText("\t".to_string())))
                },
                "space" => {
                    debug!("Inserting space in Insert mode");
                    ModalResult::Command(Box::new(InsertText(" ".to_string())))
                },
                "backspace" => {
                    debug!("Delete left in Insert mode");
                    ModalResult::Command(Box::new(DeleteLeft))
                },
                _ => {
                    debug!("Inserting text '{}' in Insert mode", key);
                    ModalResult::Command(Box::new(InsertText(key.to_string())))
                },
            },
        };

        debug!("Command result: {:?}, new mode: {:?}", result, self.mode);
        result
    }

    /// Switches to Normal mode (for external use)
    pub fn switch_to_normal(&mut self) -> ModalResult {
        if self.mode != EditorMode::Normal {
            self.mode = EditorMode::Normal;
            ModalResult::Command(Box::new(EnterNormalMode))
        } else {
            ModalResult::None
        }
    }

    /// Switches to Insert mode (for external use)
    pub fn switch_to_insert(&mut self) -> ModalResult {
        if self.mode != EditorMode::Insert {
            self.mode = EditorMode::Insert;
            ModalResult::Command(Box::new(EnterInsertMode))
        } else {
            ModalResult::None
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
    fn test_mode_strings() {
        assert_eq!(EditorMode::Normal.as_str(), "NORMAL");
        assert_eq!(EditorMode::Insert.as_str(), "INSERT");
    }

    // TODO: Update tests for command-based system
    // The tests need to be rewritten to check for command types instead of ModalAction
}
