//! Command types representing high-level editor operations.
//!
//! Commands are the intermediate layer between user input (keys, mouse, etc.)
//! and the low-level actions that transform editor state. They represent
//! semantic operations that users want to perform.

// Use the SmolStr type from iced to match keyboard input
use iced::advanced::graphics::core::SmolStr;

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
    /// Insert a string at cursor position
    InsertStr(SmolStr),
    /// Insert a newline at cursor position
    InsertNewline,
    /// Delete character before cursor (backspace)
    DeleteChar,

    // Application commands
    /// Exit the application
    Exit,

    /// Toggle command info display
    ToggleCommandInfo,
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
            Command::InsertStr(_) => "Insert text",
            Command::InsertNewline => "Insert newline",
            Command::DeleteChar => "Delete character",
            Command::Exit => "Exit application",
            Command::ToggleCommandInfo => "Toggle command help",
        }
    }

    /// Returns a short, concise name for display in UI.
    pub fn short_name(&self) -> &'static str {
        match self {
            Command::MoveCursorLeft => "Left",
            Command::MoveCursorRight => "Right",
            Command::MoveCursorUp => "Up",
            Command::MoveCursorDown => "Down",
            Command::EnterInsertMode => "Insert",
            Command::EnterNormalMode => "Normal",
            Command::EnterVisualMode => "Visual",
            Command::EnterCommandMode => "Command",
            Command::InsertStr(_) => "Insert",
            Command::InsertNewline => "Enter",
            Command::DeleteChar => "Backsp",
            Command::Exit => "Exit",
            Command::ToggleCommandInfo => "Help",
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
            Command::InsertStr(text) => Some(EditorAction::InsertText {
                position: state.cursor_position(),
                text: text.to_string(),
            }),
            Command::InsertNewline => Some(EditorAction::InsertText {
                position: state.cursor_position(),
                text: "\n".to_string(),
            }),
            Command::DeleteChar => {
                let cursor_pos = state.cursor_position();
                if cursor_pos.line > 0 || cursor_pos.column > 0 {
                    let delete_pos = if cursor_pos.column > 0 {
                        // We need to find the position of the previous character
                        // This requires proper handling of byte positions for tabs
                        if let Some(line_text) = state.line(cursor_pos.line) {
                            // Find the byte position of the previous character
                            let mut prev_byte_pos = 0;
                            let mut byte_pos = 0;

                            for (char_count, ch) in line_text.chars().enumerate() {
                                if char_count == cursor_pos.column - 1 {
                                    prev_byte_pos = byte_pos;
                                }
                                if char_count == cursor_pos.column {
                                    break;
                                }
                                byte_pos += ch.len_utf8();
                            }

                            // Use helper function to calculate visual column for the delete
                            // position
                            use crate::processor::byte_to_visual;
                            let visual_col = byte_to_visual(&line_text, prev_byte_pos);

                            TextPosition::new_with_byte_offset(
                                cursor_pos.line,
                                cursor_pos.column - 1,
                                prev_byte_pos,
                                visual_col,
                            )
                        } else {
                            // Fallback if line not found
                            TextPosition::new(cursor_pos.line, cursor_pos.column - 1)
                        }
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
            Command::ToggleCommandInfo => Some(EditorAction::ToggleCommandInfo),
        }
    }
}

// Helper functions moved from processor.rs
fn move_cursor_left(state: &crate::state::EditorState) -> crate::actions::TextPosition {
    use crate::{
        actions::TextPosition,
        processor::{byte_to_visual, char_to_byte},
    };

    let pos = state.cursor_position();
    if pos.column > 0 {
        let line_text = state.line(pos.line).unwrap_or_default();
        let new_column = pos.column - 1;
        let new_byte_offset = char_to_byte(&line_text, new_column);
        let new_visual_column = byte_to_visual(&line_text, new_byte_offset);

        TextPosition::new_with_byte_offset(pos.line, new_column, new_byte_offset, new_visual_column)
    } else if pos.line > 0 {
        let prev_line = state.line(pos.line - 1).unwrap_or_default();
        let prev_line_len = prev_line.chars().count();
        let byte_offset = prev_line.len();
        let visual_column = byte_to_visual(&prev_line, byte_offset);

        TextPosition::new_with_byte_offset(pos.line - 1, prev_line_len, byte_offset, visual_column)
    } else {
        pos
    }
}

fn move_cursor_right(state: &crate::state::EditorState) -> crate::actions::TextPosition {
    use crate::{
        actions::TextPosition,
        processor::{byte_to_visual, char_to_byte},
    };

    let pos = state.cursor_position();
    let line_text = state.line(pos.line).unwrap_or_default();
    let current_line_len = line_text.chars().count();

    if pos.column < current_line_len {
        let new_column = pos.column + 1;
        let new_byte_offset = char_to_byte(&line_text, new_column);
        let new_visual_column = byte_to_visual(&line_text, new_byte_offset);

        TextPosition::new_with_byte_offset(pos.line, new_column, new_byte_offset, new_visual_column)
    } else if pos.line < state.line_count().saturating_sub(1) {
        TextPosition::new_with_byte_offset(pos.line + 1, 0, 0, 0)
    } else {
        pos
    }
}

fn move_cursor_up(state: &crate::state::EditorState) -> crate::actions::TextPosition {
    use crate::{
        actions::TextPosition,
        processor::{byte_to_char, visual_to_byte},
    };

    let pos = state.cursor_position();
    if pos.line > 0 {
        // Use visual column for consistent tab-aware vertical movement
        let target_visual_column = state.cursor.desired_column;

        // Get the target line text and convert visual to char column
        if let Some(target_line) = state.line(pos.line - 1) {
            let (byte_offset, actual_visual) = visual_to_byte(&target_line, target_visual_column);
            let char_col = byte_to_char(&target_line, byte_offset);
            let line_char_len = target_line.chars().count();
            let new_column = char_col.min(line_char_len);
            let final_byte_offset = if new_column < char_col {
                target_line.len()
            } else {
                byte_offset
            };

            TextPosition::new_with_byte_offset(
                pos.line - 1,
                new_column,
                final_byte_offset,
                actual_visual,
            )
        } else {
            TextPosition::new_with_byte_offset(pos.line - 1, 0, 0, 0)
        }
    } else {
        pos
    }
}

fn move_cursor_down(state: &crate::state::EditorState) -> crate::actions::TextPosition {
    use crate::{
        actions::TextPosition,
        processor::{byte_to_char, visual_to_byte},
    };

    let pos = state.cursor_position();
    if pos.line < state.line_count().saturating_sub(1) {
        // Use visual column for consistent tab-aware vertical movement
        let target_visual_column = state.cursor.desired_column;

        // Get the target line text and convert visual to char column
        if let Some(target_line) = state.line(pos.line + 1) {
            let (byte_offset, actual_visual) = visual_to_byte(&target_line, target_visual_column);
            let char_col = byte_to_char(&target_line, byte_offset);
            let line_char_len = target_line.chars().count();
            let new_column = char_col.min(line_char_len);
            let final_byte_offset = if new_column < char_col {
                target_line.len()
            } else {
                byte_offset
            };

            TextPosition::new_with_byte_offset(
                pos.line + 1,
                new_column,
                final_byte_offset,
                actual_visual,
            )
        } else {
            TextPosition::new_with_byte_offset(pos.line + 1, 0, 0, 0)
        }
    } else {
        pos
    }
}
