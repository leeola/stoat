//! Command types representing high-level editor operations.
//!
//! Commands are the intermediate layer between user input (keys, mouse, etc.)
//! and the low-level actions that transform editor state. They represent
//! semantic operations that users want to perform.

/// High-level editor commands.
///
/// Commands represent the user's intent - what operation they want to perform.
/// These are mapped from keys via the keymap system and then converted to
/// [`EditorAction`]s that actually transform the state.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    // Movement commands
    /// Move cursor left one character
    MoveCursorLeft,
    /// Move cursor right one character
    MoveCursorRight,
    /// Move cursor up one line
    MoveCursorUp,
    /// Move cursor down one line
    MoveCursorDown,

    // Mode change commands
    /// Enter Insert mode
    EnterInsertMode,
    /// Enter Normal mode
    EnterNormalMode,
    /// Enter Visual mode
    EnterVisualMode,
    /// Enter Command mode
    EnterCommandMode,

    // Text manipulation commands
    /// Insert a character at cursor position
    InsertChar(char),
    /// Insert a newline at cursor position
    InsertNewline,
    /// Delete character before cursor (backspace)
    DeleteChar,

    // Application commands
    /// Exit the application
    Exit,
}

impl Command {
    /// Returns a human-readable description of the command.
    pub fn description(&self) -> &'static str {
        match self {
            Command::MoveCursorLeft => "Move cursor left",
            Command::MoveCursorRight => "Move cursor right",
            Command::MoveCursorUp => "Move cursor up",
            Command::MoveCursorDown => "Move cursor down",
            Command::EnterInsertMode => "Enter Insert mode",
            Command::EnterNormalMode => "Enter Normal mode",
            Command::EnterVisualMode => "Enter Visual mode",
            Command::EnterCommandMode => "Enter Command mode",
            Command::InsertChar(_) => "Insert character",
            Command::InsertNewline => "Insert newline",
            Command::DeleteChar => "Delete character",
            Command::Exit => "Exit application",
        }
    }

    /// Converts a command to the corresponding editor action(s).
    ///
    /// Some commands map directly to actions, while others might require
    /// context from the editor state to determine the appropriate action.
    pub fn to_action(
        &self,
        state: &crate::state::EditorState,
    ) -> Option<crate::actions::EditorAction> {
        use crate::actions::{EditMode, EditorAction, TextPosition, TextRange};

        match self {
            Command::MoveCursorLeft => {
                let pos = move_cursor_left(state);
                Some(EditorAction::MoveCursor { position: pos })
            },
            Command::MoveCursorRight => {
                let pos = move_cursor_right(state);
                Some(EditorAction::MoveCursor { position: pos })
            },
            Command::MoveCursorUp => {
                let pos = move_cursor_up(state);
                Some(EditorAction::MoveCursor { position: pos })
            },
            Command::MoveCursorDown => {
                let pos = move_cursor_down(state);
                Some(EditorAction::MoveCursor { position: pos })
            },
            Command::EnterInsertMode => Some(EditorAction::SetMode {
                mode: EditMode::Insert,
            }),
            Command::EnterNormalMode => Some(EditorAction::SetMode {
                mode: EditMode::Normal,
            }),
            Command::EnterVisualMode => Some(EditorAction::SetMode {
                mode: EditMode::Visual { line_mode: false },
            }),
            Command::EnterCommandMode => Some(EditorAction::SetMode {
                mode: EditMode::Command,
            }),
            Command::InsertChar(ch) => Some(EditorAction::InsertText {
                position: state.cursor_position(),
                text: ch.to_string(),
            }),
            Command::InsertNewline => Some(EditorAction::InsertText {
                position: state.cursor_position(),
                text: "\n".to_string(),
            }),
            Command::DeleteChar => {
                let cursor_pos = state.cursor_position();
                if cursor_pos.line > 0 || cursor_pos.column > 0 {
                    let delete_pos = if cursor_pos.column > 0 {
                        TextPosition::new(cursor_pos.line, cursor_pos.column - 1)
                    } else {
                        // Delete line break - move to end of previous line
                        let prev_line_len = state
                            .line(cursor_pos.line - 1)
                            .map(|l| l.len())
                            .unwrap_or(0);
                        TextPosition::new(cursor_pos.line - 1, prev_line_len)
                    };

                    Some(EditorAction::DeleteText {
                        range: TextRange::new(delete_pos, cursor_pos),
                    })
                } else {
                    None // Can't delete at start of buffer
                }
            },
            Command::Exit => None, // Exit is handled as an effect, not an action
        }
    }
}

// Helper functions moved from processor.rs
fn move_cursor_left(state: &crate::state::EditorState) -> crate::actions::TextPosition {
    use crate::actions::TextPosition;

    let pos = state.cursor_position();
    if pos.column > 0 {
        TextPosition::new(pos.line, pos.column - 1)
    } else if pos.line > 0 {
        let prev_line_len = state.line(pos.line - 1).map(|l| l.len()).unwrap_or(0);
        TextPosition::new(pos.line - 1, prev_line_len)
    } else {
        pos
    }
}

fn move_cursor_right(state: &crate::state::EditorState) -> crate::actions::TextPosition {
    use crate::actions::TextPosition;

    let pos = state.cursor_position();
    let current_line_len = state.line(pos.line).map(|l| l.len()).unwrap_or(0);

    if pos.column < current_line_len {
        TextPosition::new(pos.line, pos.column + 1)
    } else if pos.line < state.line_count().saturating_sub(1) {
        TextPosition::new(pos.line + 1, 0)
    } else {
        pos
    }
}

fn move_cursor_up(state: &crate::state::EditorState) -> crate::actions::TextPosition {
    use crate::actions::TextPosition;

    let pos = state.cursor_position();
    if pos.line > 0 {
        let target_column = state.cursor.desired_column;
        let prev_line_len = state.line(pos.line - 1).map(|l| l.len()).unwrap_or(0);
        let new_column = target_column.min(prev_line_len);
        TextPosition::new(pos.line - 1, new_column)
    } else {
        pos
    }
}

fn move_cursor_down(state: &crate::state::EditorState) -> crate::actions::TextPosition {
    use crate::actions::TextPosition;

    let pos = state.cursor_position();
    if pos.line < state.line_count().saturating_sub(1) {
        let target_column = state.cursor.desired_column;
        let next_line_len = state.line(pos.line + 1).map(|l| l.len()).unwrap_or(0);
        let new_column = target_column.min(next_line_len);
        TextPosition::new(pos.line + 1, new_column)
    } else {
        pos
    }
}
