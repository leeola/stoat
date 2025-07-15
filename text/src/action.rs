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
    use crate::{buffer::TextBuffer, range::TextRange, syntax::simple::SimpleText};

    #[test]
    fn test_move_actions() {
        let buffer = TextBuffer::<SimpleText>::new("hello world\ntest line");
        let view = buffer.create_view();

        // Test move right
        let act = TextAction::MoveRight { count: 5 };
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 5.into());

        // Test move left
        let act = TextAction::MoveLeft { count: 2 };
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 3.into());

        // Test move to line start
        let act = TextAction::MoveToLineStart;
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 0.into());

        // Test move to line end
        let act = TextAction::MoveToLineEnd;
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 12.into()); // After newline

        // Test move to document end
        let act = TextAction::MoveToDocumentEnd;
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 21.into());
    }

    #[test]
    fn test_word_movement() {
        let buffer = TextBuffer::<SimpleText>::new("hello world test");
        let view = buffer.create_view();

        // Move forward by word
        let act = TextAction::MoveWordForward;
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 6.into()); // After "hello "

        // Move forward again
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 12.into()); // After "world "

        // Move backward by word
        let act = TextAction::MoveWordBackward;
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 6.into()); // Back to start of "world"
    }

    #[test]
    fn test_move_to_line() {
        let buffer = TextBuffer::<SimpleText>::new("line 1\nline 2\nline 3");
        let view = buffer.create_view();

        // Move to line 2 (1-indexed)
        let act = TextAction::MoveToLine { line: 2 };
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 7.into()); // Start of "line 2"

        // Move to line 3
        let act = TextAction::MoveToLine { line: 3 };
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.primary_cursor().position(), 14.into()); // Start of "line 3"

        // Try invalid line
        let act = TextAction::MoveToLine { line: 0 };
        let result = view.execute_action(&act);
        assert!(matches!(result, Err(ActionError::InvalidLine { .. })));

        // Try line beyond buffer
        let act = TextAction::MoveToLine { line: 10 };
        let result = view.execute_action(&act);
        assert!(matches!(result, Err(ActionError::InvalidLine { .. })));
    }

    #[test]
    fn test_insert_text() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let view = buffer.create_view();

        // Move cursor to position 5
        let act = TextAction::MoveToOffset { offset: 5.into() };
        view.execute_action(&act).expect("Move should succeed");

        // Insert text
        let act = TextAction::InsertText {
            text: " beautiful".to_string(),
        };
        let result = view.execute_action(&act).expect("Insert should succeed");
        assert!(result.success);
        assert_eq!(buffer.text(), "hello beautiful world");
    }

    #[test]
    fn test_multi_cursor_actions() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let view = buffer.create_view();

        // Add cursor at position 6
        let act = TextAction::AddCursorAtOffset { offset: 6.into() };
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.cursors().len(), 2);

        // Move all cursors right
        let act = TextAction::MoveRight { count: 2 };
        view.execute_action(&act).expect("Action should succeed");

        // Check cursor positions
        let positions: Vec<u32> = view
            .cursors()
            .iter()
            .map(|c| u32::from(c.position()))
            .collect();
        assert_eq!(positions, vec![2, 8]);

        // Remove secondary cursors
        let act = TextAction::RemoveSecondaryCursors;
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert_eq!(view.cursors().len(), 1);
    }

    #[test]
    fn test_clear_selection() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let view = buffer.create_view();

        // Set a selection on the primary cursor
        view.primary_cursor_mut()
            .set_selection(Some(TextRange::new(0.into(), 5.into())));

        // Clear selection
        let act = TextAction::ClearSelection;
        let result = view.execute_action(&act).expect("Action should succeed");
        assert!(result.success);
        assert!(view.primary_cursor().selection().is_none());
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
