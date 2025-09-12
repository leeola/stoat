//! Movement commands for cursor navigation.

use crate::{
    actions::{EditorAction, TextPosition},
    processor::{byte_to_char, byte_to_visual, char_to_byte, visual_to_byte},
    state::EditorState,
};
use stoat_rope::{kind::SyntaxKind, query::Query};

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
    /// Move to next paragraph
    NextParagraph,
    /// Move to previous paragraph  
    PreviousParagraph,
}

impl MovementCommand {
    pub fn to_action(&self, state: &EditorState) -> EditorAction {
        let position = match self {
            MovementCommand::Left => move_cursor_left(state),
            MovementCommand::Right => move_cursor_right(state),
            MovementCommand::Up => move_cursor_up(state),
            MovementCommand::Down => move_cursor_down(state),
            MovementCommand::NextParagraph => move_to_next_paragraph(state),
            MovementCommand::PreviousParagraph => move_to_previous_paragraph(state),
        };
        EditorAction::MoveCursor { position }
    }

    pub fn description(&self) -> &'static str {
        match self {
            MovementCommand::Left => "Move cursor left",
            MovementCommand::Right => "Move cursor right",
            MovementCommand::Up => "Move cursor up",
            MovementCommand::Down => "Move cursor down",
            MovementCommand::NextParagraph => "Move to next paragraph",
            MovementCommand::PreviousParagraph => "Move to previous paragraph",
        }
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            MovementCommand::Left => "Left",
            MovementCommand::Right => "Right",
            MovementCommand::Up => "Up",
            MovementCommand::Down => "Down",
            MovementCommand::NextParagraph => "NextPara",
            MovementCommand::PreviousParagraph => "PrevPara",
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

fn move_to_next_paragraph(state: &EditorState) -> TextPosition {
    let pos = state.cursor_position();
    let rope = state.buffer.rope();

    // Convert current line/column position to byte offset
    let current_text = state.text();
    let lines: Vec<&str> = current_text.lines().collect();
    let mut byte_offset = 0;

    // Calculate byte offset of current position
    for (i, line) in lines.iter().enumerate() {
        if i < pos.line {
            byte_offset += line.len() + 1; // +1 for newline
        } else if i == pos.line {
            byte_offset += char_to_byte(line, pos.column);
            break;
        }
    }

    // Find all paragraph nodes
    let paragraphs = Query::new(rope.root())
        .kind(SyntaxKind::Paragraph)
        .find_all();

    // Find the next paragraph after current position
    for para in paragraphs {
        let para_range = para.range();
        if para_range.start.0 > byte_offset {
            // Found next paragraph - move to its start
            // Convert byte offset back to line/column
            let mut line = 0;
            let mut remaining = para_range.start.0;

            for (i, line_text) in lines.iter().enumerate() {
                let line_bytes = line_text.len() + 1;
                if remaining < line_bytes {
                    line = i;
                    break;
                }
                remaining -= line_bytes;
            }

            let column = if line < lines.len() {
                byte_to_char(lines[line], remaining.min(lines[line].len()))
            } else {
                0
            };

            return TextPosition::new(line, column);
        }
    }

    // No next paragraph found, stay at current position
    pos
}

fn move_to_previous_paragraph(state: &EditorState) -> TextPosition {
    let pos = state.cursor_position();
    let rope = state.buffer.rope();

    // Convert current line/column position to byte offset
    let current_text = state.text();
    let lines: Vec<&str> = current_text.lines().collect();
    let mut byte_offset = 0;

    // Calculate byte offset of current position
    for (i, line) in lines.iter().enumerate() {
        if i < pos.line {
            byte_offset += line.len() + 1; // +1 for newline
        } else if i == pos.line {
            byte_offset += char_to_byte(line, pos.column);
            break;
        }
    }

    // Find all paragraph nodes
    let mut paragraphs = Query::new(rope.root())
        .kind(SyntaxKind::Paragraph)
        .find_all();

    // Reverse to find previous paragraph
    paragraphs.reverse();

    // Find the previous paragraph before current position
    for para in paragraphs {
        let para_range = para.range();
        if para_range.start.0 < byte_offset {
            // Found previous paragraph - move to its start
            // Convert byte offset back to line/column
            let mut line = 0;
            let mut remaining = para_range.start.0;

            for (i, line_text) in lines.iter().enumerate() {
                let line_bytes = line_text.len() + 1;
                if remaining < line_bytes {
                    line = i;
                    break;
                }
                remaining -= line_bytes;
            }

            let column = if line < lines.len() {
                byte_to_char(lines[line], remaining.min(lines[line].len()))
            } else {
                0
            };

            return TextPosition::new(line, column);
        }
    }

    // No previous paragraph found, move to document start
    TextPosition::new(0, 0)
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

    #[test]
    fn paragraph_navigation_basic() {
        // Test with simple paragraphs separated by blank lines
        let text = "First paragraph\nwith two lines\n\nSecond paragraph\n\nThird paragraph";

        Stoat::test()
            .with_text(text)
            .assert_cursor(0, 0)
            .type_keys("}") // Move to next paragraph
            .assert_cursor(3, 0) // Should be at "Second paragraph"
            .type_keys("}") // Move to next paragraph
            .assert_cursor(5, 0) // Should be at "Third paragraph"
            .type_keys("}") // Try to move past last paragraph
            .assert_cursor(5, 0) // Should stay at last paragraph
            .type_keys("{") // Move to previous paragraph
            .assert_cursor(3, 0) // Back to "Second paragraph"
            .type_keys("{") // Move to previous paragraph
            .assert_cursor(0, 0) // Back to "First paragraph"
            .type_keys("{") // Try to move before first paragraph
            .assert_cursor(0, 0); // Should stay at first paragraph
    }

    #[test]
    fn paragraph_navigation_single_paragraph() {
        // Test with text that has no paragraph breaks
        let text = "This is a single paragraph\nwith multiple lines\nbut no blank lines";

        Stoat::test()
            .with_text(text)
            .assert_cursor(0, 0)
            .type_keys("}") // Try to move to next paragraph
            .assert_cursor(0, 0) // Should stay at current position
            .type_keys("{") // Try to move to previous paragraph
            .assert_cursor(0, 0); // Should stay at current position
    }

    #[test]
    fn paragraph_navigation_empty_lines() {
        // Test with multiple consecutive empty lines
        let text = "First paragraph\n\n\n\nSecond paragraph";

        Stoat::test()
            .with_text(text)
            .assert_cursor(0, 0)
            .type_keys("}") // Move to next paragraph
            .assert_cursor(2, 0); // Parser groups empty lines as one paragraph break
    }

    #[test]
    fn paragraph_navigation_from_middle() {
        // Test navigation from middle of a paragraph
        let text = "First paragraph\nwith two lines\n\nSecond paragraph\n\nThird paragraph";

        Stoat::test()
            .with_text(text)
            .cursor(1, 5) // Start in middle of first paragraph
            .assert_cursor(1, 5)
            .type_keys("}") // Move to next paragraph
            .assert_cursor(3, 0) // Should be at "Second paragraph"
            .type_keys("{") // Move back
            .assert_cursor(0, 0); // Should be at start of "First paragraph"
    }
}
