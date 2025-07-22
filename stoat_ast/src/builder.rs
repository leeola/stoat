//! Builder for constructing AST nodes with automatic position tracking

use crate::{Arena, Node, SyntaxKind, position::TextPos};
use compact_str::CompactString;

/// Builder for constructing an AST with automatic position tracking
pub struct Builder<'arena> {
    arena: &'arena Arena<'arena>,
    current_pos: TextPos,
}

impl<'arena> Builder<'arena> {
    /// Create a new builder starting at position 0
    pub fn new(arena: &'arena Arena<'arena>) -> Self {
        Self {
            arena,
            current_pos: TextPos::new(0),
        }
    }

    /// Create a new builder starting at a specific position
    pub fn new_at(arena: &'arena Arena<'arena>, start_pos: TextPos) -> Self {
        Self {
            arena,
            current_pos: start_pos,
        }
    }

    /// Get the current position
    pub fn current_pos(&self) -> TextPos {
        self.current_pos
    }

    /// Build a leaf node and advance the position
    pub fn leaf(
        &mut self,
        kind: SyntaxKind,
        text: impl Into<CompactString>,
    ) -> &'arena Node<'arena> {
        let text = text.into();
        let start = self.current_pos;
        let len = text.len();
        self.current_pos.advance(len);
        let range = start..self.current_pos;

        self.arena.alloc(Node::leaf(kind, text, range))
    }

    /// Build an internal node from children
    ///
    /// The range is automatically derived from the first and last child.
    /// This assumes children are already positioned correctly.
    pub fn internal(
        &self,
        kind: SyntaxKind,
        children: Vec<&'arena Node<'arena>>,
    ) -> &'arena Node<'arena> {
        self.arena.alloc(Node::internal(kind, children))
    }

    /// Build a tree bottom-up, manually managing positions
    pub fn build_tree<F>(&mut self, f: F) -> &'arena Node<'arena>
    where
        F: FnOnce(&mut Self) -> &'arena Node<'arena>,
    {
        f(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_leaf_nodes() {
        let arena = Arena::new();
        let mut builder = Builder::new(&arena);

        let hello = builder.leaf(SyntaxKind::Text, "hello");
        assert_eq!(hello.text(), "hello");
        assert_eq!(hello.range().start.0, 0);
        assert_eq!(hello.range().end.0, 5);
        assert_eq!(hello.len_bytes(), 5);

        let space = builder.leaf(SyntaxKind::Whitespace, " ");
        assert_eq!(space.range().start.0, 5);
        assert_eq!(space.range().end.0, 6);

        let world = builder.leaf(SyntaxKind::Text, "world");
        assert_eq!(world.range().start.0, 6);
        assert_eq!(world.range().end.0, 11);
    }

    #[test]
    fn test_builder_internal_nodes() {
        let arena = Arena::new();
        let mut builder = Builder::new(&arena);

        let hello = builder.leaf(SyntaxKind::Text, "hello");
        let space = builder.leaf(SyntaxKind::Whitespace, " ");
        let world = builder.leaf(SyntaxKind::Text, "world");

        let sentence = builder.internal(SyntaxKind::Line, vec![hello, space, world]);

        assert_eq!(sentence.range().start.0, 0);
        assert_eq!(sentence.range().end.0, 11);
        assert_eq!(sentence.len_bytes(), 11);
        assert_eq!(sentence.len_tokens(), 3);
    }

    #[test]
    fn test_builder_with_newlines() {
        let arena = Arena::new();
        let mut builder = Builder::new(&arena);

        let line1 = builder.leaf(SyntaxKind::Text, "line1\n");
        let line2 = builder.leaf(SyntaxKind::Text, "line2");

        let doc = builder.internal(SyntaxKind::Document, vec![line1, line2]);

        assert_eq!(doc.len_bytes(), 11);
        assert_eq!(doc.len_newlines(), 1);
        assert_eq!(doc.len_tokens(), 2);
    }

    #[test]
    fn test_builder_unicode() {
        let arena = Arena::new();
        let mut builder = Builder::new(&arena);

        let text1 = builder.leaf(SyntaxKind::Text, "crab");
        let text2 = builder.leaf(SyntaxKind::Text, "rust");

        assert_eq!(text1.range().start.0, 0);
        assert_eq!(text1.range().end.0, 4);
        assert_eq!(text2.range().start.0, 4);
        assert_eq!(text2.range().end.0, 8);

        let phrase = builder.internal(SyntaxKind::Line, vec![text1, text2]);
        assert_eq!(phrase.len_bytes(), 8);
        assert_eq!(phrase.len_chars(), 8);
    }
}
