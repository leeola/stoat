//! Movement commands for cursor navigation.

use crate::{
    actions::{EditorAction, TextPosition},
    processor::{byte_to_char, byte_to_visual, char_to_byte, visual_to_byte},
    state::EditorState,
};

/// Movement-related commands
#[derive(Debug, Clone, PartialEq)]
pub enum MovementCommand {
    /// Move cursor left one character
    Left,
    /// Move cursor right one character
    Right,
    /// Move cursor up one line
    Up,
    /// Move cursor down one line
    Down,
}

impl MovementCommand {
    pub fn to_action(&self, state: &EditorState) -> EditorAction {
        let position = match self {
            MovementCommand::Left => move_cursor_left(state),
            MovementCommand::Right => move_cursor_right(state),
            MovementCommand::Up => move_cursor_up(state),
            MovementCommand::Down => move_cursor_down(state),
        };
        EditorAction::MoveCursor { position }
    }

    pub fn description(&self) -> &'static str {
        match self {
            MovementCommand::Left => "Move cursor left",
            MovementCommand::Right => "Move cursor right",
            MovementCommand::Up => "Move cursor up",
            MovementCommand::Down => "Move cursor down",
        }
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            MovementCommand::Left => "Left",
            MovementCommand::Right => "Right",
            MovementCommand::Up => "Up",
            MovementCommand::Down => "Down",
        }
    }
}

fn move_cursor_left(state: &EditorState) -> TextPosition {
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

fn move_cursor_right(state: &EditorState) -> TextPosition {
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

fn move_cursor_up(state: &EditorState) -> TextPosition {
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

fn move_cursor_down(state: &EditorState) -> TextPosition {
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

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn cursor_movement_in_normal_mode() {
        Stoat::test()
            .with_text("Hello World")
            .assert_cursor(0, 0)
            .type_keys("l") // Move right
            .assert_cursor(0, 1)
            .type_keys("l") // Move right again
            .assert_cursor(0, 2)
            .type_keys("h") // Move left
            .assert_cursor(0, 1)
            .type_keys("h") // Move left again
            .assert_cursor(0, 0);
    }

    #[test]
    fn keyboard_input_navigation() {
        Stoat::test()
            .with_text("Hello World")
            .type_keys("l") // Move right
            .assert_cursor(0, 1)
            .type_keys("l") // Move right again
            .assert_cursor(0, 2)
            .type_keys("h") // Move left
            .assert_cursor(0, 1)
            .type_keys("h") // Move left again
            .assert_cursor(0, 0);
    }

    #[test]
    fn vertical_movement() {
        Stoat::test()
            .with_text("Line 1\nLine 2\nLine 3")
            .assert_cursor(0, 0)
            .type_keys("j") // Move down
            .assert_cursor(1, 0)
            .type_keys("j") // Move down again
            .assert_cursor(2, 0)
            .type_keys("k") // Move up
            .assert_cursor(1, 0)
            .type_keys("k") // Move up again
            .assert_cursor(0, 0);
    }

    #[test]
    fn movement_at_boundaries() {
        Stoat::test()
            .with_text("ABC")
            .assert_cursor(0, 0)
            .type_keys("h") // Try to move left at start - should stay
            .assert_cursor(0, 0)
            .type_keys("lll") // Move to end
            .assert_cursor(0, 3)
            .type_keys("l") // Try to move right at end - should stay
            .assert_cursor(0, 3);
    }
}
