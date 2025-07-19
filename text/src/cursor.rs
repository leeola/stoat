//! Text cursor for navigation and editing within views

use crate::{
    TextSize,
    edit::{Edit, EditOperation},
    range::TextRange,
    syntax::SyntaxNode,
};
use snafu::Snafu;

#[derive(Debug, Snafu)]
pub enum CursorError {
    #[snafu(display("Invalid cursor position"))]
    InvalidPosition,

    #[snafu(display("No node at cursor"))]
    NoNodeAtCursor,
}

type Result<T> = std::result::Result<T, CursorError>;

/// Unique identifier for a cursor
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CursorId(u64);

impl CursorId {
    pub(crate) fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// A cursor for navigating and editing text within a view
pub struct TextCursor {
    /// Unique identifier for this cursor
    id: CursorId,
    /// Position in the buffer (byte offset)
    position: TextSize,
    /// Current node the cursor is at
    current_node: Option<SyntaxNode>,
    /// Selection range (if any)
    selection: Option<TextRange>,
}

impl TextCursor {
    /// Create a new cursor at the given position
    pub(crate) fn new(position: TextSize, root: SyntaxNode) -> Self {
        Self {
            id: CursorId::new(),
            position,
            current_node: Some(root),
            selection: None,
        }
    }

    /// Get the cursor's ID
    pub fn id(&self) -> CursorId {
        self.id
    }

    /// Get the cursor's position
    pub fn position(&self) -> TextSize {
        self.position
    }

    /// Set the cursor's position
    pub fn set_position(&mut self, position: TextSize) {
        self.position = position;
    }

    /// Get the cursor's selection
    pub fn selection(&self) -> Option<TextRange> {
        self.selection
    }

    /// Set the cursor's selection
    pub fn set_selection(&mut self, selection: Option<TextRange>) {
        self.selection = selection;
    }

    /// Clear the cursor's selection
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Set the current node
    pub fn set_current_node(&mut self, node: SyntaxNode) {
        self.current_node = Some(node);
    }

    /// Get the current node
    pub fn current(&self) -> Option<&SyntaxNode> {
        self.current_node.as_ref()
    }

    /// Move to the parent node
    pub fn move_to_parent(&mut self) -> bool {
        if let Some(current) = &self.current_node {
            if let Some(parent) = current.parent() {
                self.current_node = Some(parent);
                return true;
            }
        }
        false
    }

    /// Move to the first child
    pub fn move_to_first_child(&mut self) -> bool {
        if let Some(current) = &self.current_node {
            if let Some(child) = current.first_child() {
                self.current_node = Some(child);
                return true;
            }
        }
        false
    }

    /// Move to child at index
    pub fn move_to_child(&mut self, index: usize) -> bool {
        if let Some(current) = &self.current_node {
            if let Some(child) = current.child(index) {
                self.current_node = Some(child);
                return true;
            }
        }
        false
    }

    /// Find and move to the next node matching a predicate
    pub fn find_next(&mut self, _predicate: impl Fn(&SyntaxNode) -> bool) -> bool {
        // TODO: Implement proper tree traversal
        false
    }

    /// Get the text of the current node
    pub fn text(&self) -> Option<&str> {
        self.current_node.as_ref().map(|n| n.text())
    }

    /// Get the range of the current node
    pub fn range(&self) -> Option<TextRange> {
        self.current_node.as_ref().map(|n| n.text_range())
    }

    /// Insert text at the current position
    pub fn insert(&mut self, text: &str) -> Result<Edit> {
        let node = self
            .current_node
            .as_ref()
            .ok_or(CursorError::NoNodeAtCursor)?;

        Ok(Edit {
            target: node.clone(),
            operation: EditOperation::InsertAt {
                offset: self.position.into(),
                text: text.to_string(),
            },
        })
    }

    /// Replace the current node's text
    pub fn replace(&mut self, text: &str) -> Result<Edit> {
        let node = self
            .current_node
            .as_ref()
            .ok_or(CursorError::NoNodeAtCursor)?;

        Ok(Edit {
            target: node.clone(),
            operation: EditOperation::Replace(text.to_string()),
        })
    }

    /// Delete the current node
    pub fn delete(&mut self) -> Result<Edit> {
        let node = self
            .current_node
            .as_ref()
            .ok_or(CursorError::NoNodeAtCursor)?;

        Ok(Edit {
            target: node.clone(),
            operation: EditOperation::Delete,
        })
    }

    /// Insert text before the current node
    pub fn insert_before(&mut self, text: &str) -> Result<Edit> {
        let node = self
            .current_node
            .as_ref()
            .ok_or(CursorError::NoNodeAtCursor)?;

        Ok(Edit {
            target: node.clone(),
            operation: EditOperation::InsertBefore(text.to_string()),
        })
    }

    /// Insert text after the current node
    pub fn insert_after(&mut self, text: &str) -> Result<Edit> {
        let node = self
            .current_node
            .as_ref()
            .ok_or(CursorError::NoNodeAtCursor)?;

        Ok(Edit {
            target: node.clone(),
            operation: EditOperation::InsertAfter(text.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn test_cursor_creation() {
        let root = simple_root("hello world");
        let cursor = TextCursor::new(0.into(), root);
        assert!(cursor.current().is_some());
        assert_eq!(cursor.position(), 0.into());
    }

    #[test]
    fn test_cursor_edit_operations() {
        let root = simple_root("hello world");
        let mut cursor = TextCursor::new(0.into(), root);

        let edit = cursor
            .replace("goodbye")
            .expect("Failed to create replace edit");
        match edit.operation {
            EditOperation::Replace(text) => assert_eq!(text, "goodbye"),
            _ => panic!("Expected Replace operation"),
        }
    }
}
