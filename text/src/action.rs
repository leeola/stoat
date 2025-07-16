//! Text editing actions
//!
//! This module defines all the actions that can be executed on text views.
//! Actions provide a high-level API for text manipulation that modal input
//! systems can target.

use crate::{TextSize, cursor::CursorId, edit::EditError, range::TextRange};
use snafu::Snafu;

#[derive(Debug, Snafu)]
pub enum ActionError {
    #[snafu(display("Invalid cursor position: {position:?}"))]
    InvalidPosition { position: TextSize },

    #[snafu(display("Invalid line number: {line}"))]
    InvalidLine { line: usize },

    #[snafu(display("No cursor with id: {id:?}"))]
    CursorNotFound { id: CursorId },

    #[snafu(display("Edit operation failed: {source}"))]
    EditFailed { source: EditError },

    #[snafu(display("Operation not supported without proper AST"))]
    AstNotAvailable,
}

pub type ActionResult<T> = Result<T, ActionError>;

/// Result of executing a action
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Whether the action succeeded
    pub success: bool,
    /// Ranges that were affected by the action
    pub affected_ranges: Vec<TextRange>,
    /// Optional message about the execution
    pub message: Option<String>,
}

/// Text editing actions
#[derive(Debug, Clone)]
pub enum TextAction {
    // Cursor Movement
    /// Move cursor left by count characters
    MoveLeft { count: usize },
    /// Move cursor right by count characters
    MoveRight { count: usize },
    /// Move cursor up by count lines
    MoveUp { count: usize },
    /// Move cursor down by count lines
    MoveDown { count: usize },
    /// Move cursor forward by one word
    MoveWordForward,
    /// Move cursor backward by one word
    MoveWordBackward,
    /// Move cursor to start of current line
    MoveToLineStart,
    /// Move cursor to end of current line
    MoveToLineEnd,
    /// Move cursor to start of document
    MoveToDocumentStart,
    /// Move cursor to end of document
    MoveToDocumentEnd,
    /// Move cursor to specific line (1-indexed)
    MoveToLine { line: usize },
    /// Move cursor to specific offset
    MoveToOffset { offset: TextSize },

    // Selection
    /// Extend selection left by count characters
    ExtendSelectionLeft { count: usize },
    /// Extend selection right by count characters
    ExtendSelectionRight { count: usize },
    /// Extend selection to word end
    ExtendSelectionToWordEnd,
    /// Extend selection to word start
    ExtendSelectionToWordStart,
    /// Select the current line
    SelectLine,
    /// Select the word at cursor
    SelectWord,
    /// Select all text
    SelectAll,
    /// Clear current selection
    ClearSelection,

    // Editing
    /// Insert text at cursor position
    InsertText { text: String },
    /// Delete character forward (Delete key)
    DeleteForward,
    /// Delete character backward (Backspace)
    DeleteBackward,
    /// Delete word forward
    DeleteWordForward,
    /// Delete word backward
    DeleteWordBackward,
    /// Delete current line
    DeleteLine,
    /// Replace current selection with text
    ReplaceSelection { text: String },

    // Multi-cursor
    /// Add a cursor above current position
    AddCursorAbove,
    /// Add a cursor below current position
    AddCursorBelow,
    /// Add a cursor at specific offset
    AddCursorAtOffset { offset: TextSize },
    /// Remove all secondary cursors
    RemoveSecondaryCursors,
}

impl TextAction {
    /// Check if this action modifies the buffer
    pub fn is_edit_action(&self) -> bool {
        matches!(
            self,
            TextAction::InsertText { .. }
                | TextAction::DeleteForward
                | TextAction::DeleteBackward
                | TextAction::DeleteWordForward
                | TextAction::DeleteWordBackward
                | TextAction::DeleteLine
                | TextAction::ReplaceSelection { .. }
        )
    }

    /// Check if this action is a movement action
    pub fn is_movement_action(&self) -> bool {
        matches!(
            self,
            TextAction::MoveLeft { .. }
                | TextAction::MoveRight { .. }
                | TextAction::MoveUp { .. }
                | TextAction::MoveDown { .. }
                | TextAction::MoveWordForward
                | TextAction::MoveWordBackward
                | TextAction::MoveToLineStart
                | TextAction::MoveToLineEnd
                | TextAction::MoveToDocumentStart
                | TextAction::MoveToDocumentEnd
                | TextAction::MoveToLine { .. }
                | TextAction::MoveToOffset { .. }
        )
    }

    /// Check if this action affects selection
    pub fn is_selection_action(&self) -> bool {
        matches!(
            self,
            TextAction::ExtendSelectionLeft { .. }
                | TextAction::ExtendSelectionRight { .. }
                | TextAction::ExtendSelectionToWordEnd
                | TextAction::ExtendSelectionToWordStart
                | TextAction::SelectLine
                | TextAction::SelectWord
                | TextAction::SelectAll
                | TextAction::ClearSelection
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{range::TextRange, test_helpers::*};

    #[test]
    fn test_move_actions() {
        TestScenario::new("hello world\ntest line")
            .exec(ActionBuilder::move_right(5))
            .expect_pos(5)
            .exec(ActionBuilder::move_left(2))
            .expect_pos(3)
            .exec(TextAction::MoveToLineStart)
            .expect_pos(0)
            .exec(TextAction::MoveToLineEnd)
            .expect_pos(12) // After newline
            .exec(TextAction::MoveToDocumentEnd)
            .expect_pos(21);
    }

    #[test]
    fn test_word_movement() {
        TestScenario::new("hello world test")
            .exec(TextAction::MoveWordForward)
            .expect_pos(6) // After "hello "
            .exec(TextAction::MoveWordForward)
            .expect_pos(12) // After "world "
            .exec(TextAction::MoveWordBackward)
            .expect_pos(6); // Back to start of "world"
    }

    #[test]
    fn test_move_to_line() {
        let view = simple_view("line 1\nline 2\nline 3");

        exec_expect_pos(&view, ActionBuilder::move_to_line(2), 7); // Start of "line 2"
        exec_expect_pos(&view, ActionBuilder::move_to_line(3), 14); // Start of "line 3"

        // Try invalid lines
        let result = view.execute_action(&ActionBuilder::move_to_line(0));
        assert!(matches!(result, Err(ActionError::InvalidLine { .. })));

        let result = view.execute_action(&ActionBuilder::move_to_line(10));
        assert!(matches!(result, Err(ActionError::InvalidLine { .. })));
    }

    #[test]
    fn test_insert_text() {
        TestScenario::at_position("hello world", 5)
            .exec(ActionBuilder::insert_text(" beautiful"))
            .expect_text("hello beautiful world");
    }

    #[test]
    fn test_multi_cursor_actions() {
        let view = simple_view("hello world");

        exec(&view, &ActionBuilder::add_cursor_at(6));
        assert_eq!(view.cursors().len(), 2);

        exec(&view, &ActionBuilder::move_right(2));
        let positions: Vec<u32> = view
            .cursors()
            .iter()
            .map(|c| u32::from(c.position()))
            .collect();
        assert_eq!(positions, vec![2, 8]);

        exec(&view, &TextAction::RemoveSecondaryCursors);
        assert_eq!(view.cursors().len(), 1);
    }

    #[test]
    fn test_clear_selection() {
        let view = simple_view("hello world");
        view.primary_cursor_mut()
            .set_selection(Some(TextRange::new(0.into(), 5.into())));

        exec(&view, &TextAction::ClearSelection);
        assert_no_selection(&view.primary_cursor());
    }

    #[test]
    fn test_action_classification() {
        // Test movement actions
        assert!(TextAction::MoveLeft { count: 1 }.is_movement_action());
        assert!(TextAction::MoveWordForward.is_movement_action());
        assert!(TextAction::MoveToLineStart.is_movement_action());

        // Test edit actions
        assert!(
            TextAction::InsertText {
                text: "test".to_string()
            }
            .is_edit_action()
        );
        assert!(TextAction::DeleteForward.is_edit_action());
        assert!(TextAction::DeleteLine.is_edit_action());

        // Test selection actions
        assert!(TextAction::SelectLine.is_selection_action());
        assert!(TextAction::ExtendSelectionLeft { count: 1 }.is_selection_action());
        assert!(TextAction::ClearSelection.is_selection_action());
    }
}
