//! Views into text buffers

use crate::{
    TextSize,
    buffer::{ChangeEvent, TextBuffer},
    cursor::TextCursor,
    cursor_collection::CursorCollection,
    range::TextRange,
    syntax::{Syntax, SyntaxNode},
};
use parking_lot::RwLock;
use std::sync::Arc;

/// A view into a text buffer, showing a specific portion
pub struct TextView<S: Syntax> {
    inner: Arc<TextViewInner<S>>,
}

pub(crate) struct TextViewInner<S: Syntax> {
    /// Reference to the buffer
    buffer: TextBuffer<S>,
    /// Root node of this view
    view_root: RwLock<SyntaxNode<S>>,
    /// Collection of cursors for this view
    cursors: RwLock<CursorCollection<S>>,
}

impl<S: Syntax> TextView<S> {
    /// Create a new view
    pub(crate) fn new(buffer: TextBuffer<S>, root: SyntaxNode<S>) -> Self {
        let cursors = CursorCollection::new(0.into(), root.clone());
        let inner = Arc::new(TextViewInner {
            buffer: buffer.clone(),
            view_root: RwLock::new(root),
            cursors: RwLock::new(cursors),
        });

        // Register this view with the buffer
        buffer.register_view(Arc::downgrade(&inner));

        Self { inner }
    }

    /// Get the buffer this view is attached to
    pub fn buffer(&self) -> &TextBuffer<S> {
        &self.inner.buffer
    }

    /// Get a reference to the primary cursor
    pub fn primary_cursor(&self) -> impl std::ops::Deref<Target = TextCursor<S>> + '_ {
        parking_lot::RwLockReadGuard::map(self.inner.cursors.read(), |c| c.primary())
    }

    /// Get a mutable reference to the primary cursor
    pub fn primary_cursor_mut(&self) -> impl std::ops::DerefMut<Target = TextCursor<S>> + '_ {
        parking_lot::RwLockWriteGuard::map(self.inner.cursors.write(), |c| c.primary_mut())
    }

    /// Access the cursor collection
    pub fn cursors(&self) -> parking_lot::RwLockReadGuard<'_, CursorCollection<S>> {
        self.inner.cursors.read()
    }

    /// Access the cursor collection mutably
    pub fn cursors_mut(&self) -> parking_lot::RwLockWriteGuard<'_, CursorCollection<S>> {
        self.inner.cursors.write()
    }

    /// Get the root node of this view
    pub fn root(&self) -> SyntaxNode<S> {
        self.inner.view_root.read().clone()
    }

    /// Set the root node of this view
    pub fn set_root(&mut self, node: SyntaxNode<S>) {
        *self.inner.view_root.write() = node;
    }

    /// Expand the view to show the parent of the current root
    pub fn expand_to_parent(&mut self) -> bool {
        let current_root = self.inner.view_root.read().clone();
        if let Some(parent) = current_root.parent() {
            self.set_root(parent);
            true
        } else {
            false
        }
    }

    /// Narrow the view to show a specific child
    pub fn narrow_to_child(&mut self, index: usize) -> bool {
        let current_root = self.inner.view_root.read().clone();
        if let Some(child) = current_root.child(index) {
            self.set_root(child);
            true
        } else {
            false
        }
    }

    /// Get the visible text in this view
    pub fn text(&self) -> String {
        // TODO: Extract text from rope using view range
        self.inner.view_root.read().text().to_string()
    }

    /// Get the text range of this view
    pub fn text_range(&self) -> TextRange {
        self.inner.view_root.read().text_range()
    }

    /// Check if a buffer offset is visible in this view
    pub fn contains_offset(&self, offset: usize) -> bool {
        let range = self.text_range();
        range.contains((offset as u32).into())
    }
}

impl<S: Syntax> Clone for TextView<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: Syntax> TextViewInner<S> {
    /// Handle a change event from the buffer
    pub(crate) fn on_buffer_change(&self, event: &ChangeEvent) {
        // Update the view root to point to the new AST
        let new_root = self.buffer.syntax();
        *self.view_root.write() = new_root.clone();

        // Adjust cursor positions based on the change
        let mut cursors = self.cursors.write();

        // Calculate the adjustment needed
        let change_start = event.range.start();
        let change_end = event.range.end();
        let inserted_len = u32::from(event.inserted_len);
        let deleted_len = u32::from(event.deleted_len);

        // Update each cursor
        for cursor in cursors.iter_mut() {
            let current_pos = cursor.position();

            // If cursor is before the change, no adjustment needed
            if current_pos < change_start {
                // Position is unchanged
            }
            // If cursor is at the exact start of an insertion (no deletion), move after the
            // insertion
            else if current_pos == change_start && deleted_len == 0 {
                cursor.set_position(change_start + event.inserted_len);
            }
            // If cursor is within the deleted range, move to the start of the change
            else if current_pos < change_end {
                cursor.set_position(change_start);
            }
            // If cursor is after the change, adjust by the size delta
            else {
                let current_pos_u32 = u32::from(current_pos);
                let new_pos_u32 = if inserted_len >= deleted_len {
                    // Net insertion
                    current_pos_u32 + (inserted_len - deleted_len)
                } else {
                    // Net deletion
                    current_pos_u32.saturating_sub(deleted_len - inserted_len)
                };
                cursor.set_position(TextSize::from(new_pos_u32));
            }

            // Update the cursor's current node to match the new AST
            // For now, just reset to the root - proper node tracking will come later
            cursor.set_current_node(new_root.clone());

            // Clear selection if it was affected by the change
            if let Some(selection) = cursor.selection() {
                // If selection overlaps with the change, clear it
                if selection.start() < event.range.end() && selection.end() > event.range.start() {
                    cursor.clear_selection();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::simple::SimpleText;

    #[test]
    fn test_view_creation() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let view = buffer.create_view();
        assert_eq!(view.text(), "hello world");
    }

    #[test]
    fn test_view_cursor_position() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let view = buffer.create_view();
        assert_eq!(view.primary_cursor().position(), 0.into());

        view.primary_cursor_mut().set_position(5.into());
        assert_eq!(view.primary_cursor().position(), 5.into());
    }

    #[test]
    fn test_view_multiple_cursors() {
        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let view = buffer.create_view();
        let root = view.root();

        assert_eq!(view.cursors().len(), 1);

        view.cursors_mut().add_cursor(5.into(), root.clone());
        view.cursors_mut().add_cursor(10.into(), root);
        assert_eq!(view.cursors().len(), 3);
    }

    #[test]
    fn test_view_cursor_adjustment_on_edit() {
        use crate::edit::Edit;

        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let view = buffer.create_view();
        let root = view.root();

        // Place cursors at different positions
        view.cursors_mut().add_cursor(5.into(), root.clone()); // After "hello"
        view.cursors_mut().add_cursor(11.into(), root.clone()); // At end

        // Apply an edit that inserts text at position 0
        let edit = Edit::insert_before(root.clone(), "Hi! ".to_string());
        buffer.apply_edit(&edit).expect("Edit should succeed");

        // Check that cursors were adjusted
        let cursors = view.cursors();
        let positions: Vec<u32> = cursors.iter().map(|c| u32::from(c.position())).collect();

        // Original positions: 0, 5, 11
        // After inserting "Hi! " (4 chars) at start: 4, 9, 15
        assert_eq!(positions[0], 4); // Primary cursor moved
        assert_eq!(positions[1], 9); // Cursor at 5 moved to 9
        assert_eq!(positions[2], 15); // Cursor at 11 moved to 15
    }

    #[test]
    fn test_view_cursor_in_deleted_range() {
        use crate::edit::Edit;

        let buffer = TextBuffer::<SimpleText>::new("hello world");
        let view = buffer.create_view();
        let root = view.root();

        // Place a cursor in the middle of "hello"
        view.cursors_mut().add_cursor(3.into(), root.clone());

        // Delete the entire content
        let edit = Edit::delete(root);
        buffer.apply_edit(&edit).expect("Edit should succeed");

        // Check that cursor was moved to position 0
        let cursors = view.cursors();
        for cursor in cursors.iter() {
            assert_eq!(u32::from(cursor.position()), 0);
        }
    }
}
