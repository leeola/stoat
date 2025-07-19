//! Collection for managing multiple cursors

use crate::{
    TextSize,
    cursor::{CursorId, TextCursor},
    syntax::SyntaxNode,
};
use indexmap::IndexMap;

/// A collection of cursors for multi-cursor editing
pub struct CursorCollection {
    /// The primary cursor ID
    primary: CursorId,
    /// All cursors indexed by ID
    cursors: IndexMap<CursorId, TextCursor>,
}

impl CursorCollection {
    /// Create a new cursor collection with a single cursor
    pub fn new(position: TextSize, root: SyntaxNode) -> Self {
        let cursor = TextCursor::new(position, root);
        let primary = cursor.id();
        let mut cursors = IndexMap::new();
        cursors.insert(primary, cursor);

        Self { primary, cursors }
    }

    /// Get the primary cursor
    pub fn primary(&self) -> &TextCursor {
        self.cursors
            .get(&self.primary)
            .expect("Primary cursor should always exist")
    }

    /// Get the primary cursor mutably
    pub fn primary_mut(&mut self) -> &mut TextCursor {
        self.cursors
            .get_mut(&self.primary)
            .expect("Primary cursor should always exist")
    }

    /// Add a new cursor at the given position
    pub fn add_cursor(&mut self, position: TextSize, root: SyntaxNode) -> CursorId {
        let cursor = TextCursor::new(position, root);
        let id = cursor.id();
        self.cursors.insert(id, cursor);
        id
    }

    /// Remove a cursor by ID
    pub fn remove_cursor(&mut self, id: CursorId) -> bool {
        if id == self.primary {
            // Can't remove the primary cursor
            return false;
        }
        self.cursors.shift_remove(&id).is_some()
    }

    /// Get a cursor by ID
    pub fn get(&self, id: CursorId) -> Option<&TextCursor> {
        self.cursors.get(&id)
    }

    /// Get a cursor by ID mutably
    pub fn get_mut(&mut self, id: CursorId) -> Option<&mut TextCursor> {
        self.cursors.get_mut(&id)
    }

    /// Iterate over all cursors
    pub fn iter(&self) -> impl Iterator<Item = &TextCursor> {
        self.cursors.values()
    }

    /// Iterate over all cursors mutably
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut TextCursor> {
        self.cursors.values_mut()
    }

    /// Get the number of cursors
    pub fn len(&self) -> usize {
        self.cursors.len()
    }

    /// Check if the collection is empty (always false since we always have primary)
    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }

    /// Merge cursors that are at the same position
    pub fn merge_overlapping(&mut self) {
        let mut positions: IndexMap<TextSize, CursorId> = IndexMap::new();
        let mut to_remove = Vec::new();

        // Find duplicates
        for (id, cursor) in &self.cursors {
            let pos = cursor.position();
            if let Some(&existing_id) = positions.get(&pos) {
                // Keep the primary cursor if it's one of the duplicates
                if *id == self.primary {
                    to_remove.push(existing_id);
                    positions.insert(pos, *id);
                } else {
                    to_remove.push(*id);
                }
            } else {
                positions.insert(pos, *id);
            }
        }

        // Remove duplicates
        for id in to_remove {
            self.cursors.shift_remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn test_cursor_collection_creation() {
        let root = simple_root("test");
        let collection = CursorCollection::new(0.into(), root);
        assert_eq!(collection.len(), 1);
        assert!(!collection.is_empty());
    }

    #[test]
    fn test_add_remove_cursors() {
        let root = simple_root("test");
        let mut collection = CursorCollection::new(0.into(), root.clone());
        let id = collection.add_cursor(5.into(), root.clone());
        assert_eq!(collection.len(), 2);

        assert!(collection.remove_cursor(id));
        assert_eq!(collection.len(), 1);

        // Can't remove primary
        let primary_id = collection.primary().id();
        assert!(!collection.remove_cursor(primary_id));
    }

    #[test]
    fn test_merge_overlapping() {
        let root = simple_root("test");
        let mut collection = CursorCollection::new(0.into(), root.clone());
        collection.add_cursor(5.into(), root.clone());
        collection.add_cursor(5.into(), root.clone()); // Same position
        collection.add_cursor(10.into(), root.clone());

        assert_eq!(collection.len(), 4);
        collection.merge_overlapping();
        assert_eq!(collection.len(), 3); // One duplicate removed
    }
}
