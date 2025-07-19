//! Test helpers to reduce boilerplate in tests

use crate::{
    action::{ExecutionResult, TextAction},
    buffer::TextBuffer,
    cursor::TextCursor,
    edit::Edit,
    range::TextRange,
    syntax::{SyntaxNode, unified_kind::SyntaxKind},
    view::TextView,
};

/// Create a TextBuffer with unified syntax
pub fn simple_buffer(text: &str) -> TextBuffer {
    TextBuffer::new(text)
}

/// Create a TextView with unified syntax
pub fn simple_view(text: &str) -> TextView {
    simple_buffer(text).create_view()
}

/// Create a TextView positioned at the given offset
pub fn simple_view_at(text: &str, pos: u32) -> TextView {
    let view = simple_view(text);
    let action = TextAction::MoveToOffset { offset: pos.into() };
    exec(&view, &action);
    view
}

/// Execute an action on a view, expecting success
pub fn exec(view: &TextView, action: &TextAction) -> ExecutionResult {
    view.execute_action(action).expect("Action should succeed")
}

/// Execute a movement action and return the new cursor position
pub fn exec_move(view: &TextView, action: TextAction) -> u32 {
    exec(view, &action);
    u32::from(view.primary_cursor().position())
}

/// Execute an action and assert the cursor ends up at expected position
pub fn exec_expect_pos(view: &TextView, action: TextAction, expected_pos: u32) {
    exec(view, &action);
    assert_cursor_at(view, expected_pos);
}

/// Apply a replace edit and return the new buffer text
pub fn apply_replace(buffer: &TextBuffer, text: &str) -> String {
    let root = buffer.syntax();
    let edit = Edit::replace(root, text.to_string());
    buffer.apply_edit(&edit).expect("Edit should succeed");
    buffer.text()
}

/// Apply an insert before edit and return the new buffer text
pub fn apply_insert_before(buffer: &TextBuffer, text: &str) -> String {
    let root = buffer.syntax();
    let edit = Edit::insert_before(root, text.to_string());
    buffer.apply_edit(&edit).expect("Edit should succeed");
    buffer.text()
}

/// Apply an insert after edit and return the new buffer text
pub fn apply_insert_after(buffer: &TextBuffer, text: &str) -> String {
    let root = buffer.syntax();
    let edit = Edit::insert_after(root, text.to_string());
    buffer.apply_edit(&edit).expect("Edit should succeed");
    buffer.text()
}

/// Apply a delete edit and return the new buffer text
pub fn apply_delete(buffer: &TextBuffer) -> String {
    let root = buffer.syntax();
    let edit = Edit::delete(root);
    buffer.apply_edit(&edit).expect("Edit should succeed");
    buffer.text()
}

/// Assert cursor is at the expected position
pub fn assert_cursor_at(view: &TextView, pos: u32) {
    assert_eq!(view.primary_cursor().position(), pos.into());
}

/// Assert buffer has the expected text
pub fn assert_buffer_text(buffer: &TextBuffer, expected: &str) {
    assert_eq!(buffer.text(), expected);
}

/// Assert cursor has the expected selection
pub fn assert_selection(cursor: &TextCursor, start: u32, end: u32) {
    let selection = cursor.selection().expect("Cursor should have selection");
    assert_eq!(selection, TextRange::new(start.into(), end.into()));
}

/// Assert cursor has no selection
pub fn assert_no_selection(cursor: &TextCursor) {
    assert!(
        cursor.selection().is_none(),
        "Cursor should have no selection"
    );
}

/// Create a simple syntax node for testing
pub fn simple_node(kind: SyntaxKind, start: u32, end: u32, text: &str) -> SyntaxNode {
    SyntaxNode::new_with_text(
        kind,
        TextRange::new(start.into(), end.into()),
        text.to_string(),
    )
}

/// Create a root node for testing
pub fn simple_root(text: &str) -> SyntaxNode {
    simple_node(SyntaxKind::Root, 0, text.len() as u32, text)
}

/// Builder for creating test actions
pub struct ActionBuilder;

impl ActionBuilder {
    pub fn move_left(count: usize) -> TextAction {
        TextAction::MoveLeft { count }
    }

    pub fn move_right(count: usize) -> TextAction {
        TextAction::MoveRight { count }
    }

    pub fn move_up(count: usize) -> TextAction {
        TextAction::MoveUp { count }
    }

    pub fn move_down(count: usize) -> TextAction {
        TextAction::MoveDown { count }
    }

    pub fn move_to_offset(offset: u32) -> TextAction {
        TextAction::MoveToOffset {
            offset: offset.into(),
        }
    }

    pub fn move_to_line(line: usize) -> TextAction {
        TextAction::MoveToLine { line }
    }

    pub fn insert_text(text: &str) -> TextAction {
        TextAction::InsertText {
            text: text.to_string(),
        }
    }

    pub fn add_cursor_at(offset: u32) -> TextAction {
        TextAction::AddCursorAtOffset {
            offset: offset.into(),
        }
    }

    pub fn extend_selection_left(count: usize) -> TextAction {
        TextAction::ExtendSelectionLeft { count }
    }

    pub fn extend_selection_right(count: usize) -> TextAction {
        TextAction::ExtendSelectionRight { count }
    }
}

/// Multi-step test builder for complex scenarios
pub struct TestScenario {
    view: TextView,
}

impl TestScenario {
    pub fn new(text: &str) -> Self {
        Self {
            view: simple_view(text),
        }
    }

    pub fn at_position(text: &str, pos: u32) -> Self {
        Self {
            view: simple_view_at(text, pos),
        }
    }

    pub fn exec(self, action: TextAction) -> Self {
        exec(&self.view, &action);
        self
    }

    pub fn expect_pos(self, expected: u32) -> Self {
        assert_cursor_at(&self.view, expected);
        self
    }

    pub fn expect_text(self, expected: &str) -> Self {
        assert_buffer_text(self.view.buffer(), expected);
        self
    }

    pub fn expect_selection(self, start: u32, end: u32) -> Self {
        assert_selection(&self.view.primary_cursor(), start, end);
        self
    }

    pub fn expect_no_selection(self) -> Self {
        assert_no_selection(&self.view.primary_cursor());
        self
    }

    pub fn expect_cursor_count(self, count: usize) -> Self {
        assert_eq!(self.view.cursors().len(), count);
        self
    }

    pub fn view(&self) -> &TextView {
        &self.view
    }

    pub fn buffer(&self) -> &TextBuffer {
        self.view.buffer()
    }
}
