//! Text editing commands for inserting and deleting text.

use crate::{
    actions::{EditorAction, TextPosition, TextRange},
    processor::byte_to_visual,
    state::EditorState,
};
use smol_str::SmolStr;

/// Text editing commands
#[derive(Debug, Clone, PartialEq)]
pub enum EditCommand {
    /// Insert a string at cursor position
    InsertStr(SmolStr),
    /// Insert a newline at cursor position
    InsertNewline,
    /// Delete character before cursor (backspace)
    DeleteChar,
}

impl EditCommand {
    pub fn to_action(&self, state: &EditorState) -> Option<EditorAction> {
        match self {
            EditCommand::InsertStr(text) => Some(EditorAction::InsertText {
                position: state.cursor_position(),
                text: text.to_string(),
            }),
            EditCommand::InsertNewline => Some(EditorAction::InsertText {
                position: state.cursor_position(),
                text: "\n".to_string(),
            }),
            EditCommand::DeleteChar => delete_char_action(state),
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            EditCommand::InsertStr(_) => "Insert text",
            EditCommand::InsertNewline => "Insert newline",
            EditCommand::DeleteChar => "Delete character",
        }
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            EditCommand::InsertStr(_) => "Insert",
            EditCommand::InsertNewline => "Enter",
            EditCommand::DeleteChar => "Backsp",
        }
    }
}

fn delete_char_action(state: &EditorState) -> Option<EditorAction> {
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

                // Use helper function to calculate visual column for the delete position
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
}
