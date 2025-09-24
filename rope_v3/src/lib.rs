//! stoat_rope_v3 - A high-performance rope for AST token tracking
//!
//! This crate implements the same functionality as stoat_rope but uses
//! a Zed-inspired SumTree architecture for O(log n) operations and
//! efficient aggregation of metadata.
//!
//! ## Key Features
//! - O(log n) operations for all queries and edits
//! - Rich semantic metadata tracking
//! - Multi-dimensional navigation (by byte, token, line, syntax kind, etc.)
//! - Stable positions via anchors (edits don't invalidate positions)
//! - CRDT-ready for collaborative editing
//!
//! ## Example
//! ```ignore
//! use stoat_rope_v3::{SyntaxTree, SyntaxKind};
//!
//! let mut tree = SyntaxTree::from_text("let x = 42;");
//!
//! // Get token count
//! println!("Tokens: {}", tree.token_count());
//!
//! // Find all identifiers
//! let identifiers = tree.tokens_of_kind(SyntaxKind::Identifier);
//! for token in identifiers {
//!     println!("Found identifier: {}", tree.token_text(&token));
//! }
//! ```

// Internal modules
mod anchor;
mod dimensions;
mod kinds;
mod language;
mod semantic;
mod token;
mod tree;

// Public re-exports
pub use anchor::{Anchor, AnchorRangeExt};
pub use dimensions::{
    ByteOffset, ErrorOffset, LineNumber, SemanticOffset, SyntaxKindOffset, TokenIndex,
};
pub use kinds::SyntaxKind;
pub use language::Language;
pub use rope::Point;
pub use semantic::{SemanticId, SemanticInfo, SemanticKind};
pub use sum_tree::{Bias, Cursor, SumTree};
pub use token::{Token, TokenSummary};
pub use tree::{SyntaxTree, TransactionId};

// For compatibility with original stoat_rope
pub type RopeAst = SyntaxTree;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_tokenization() {
        let tree = SyntaxTree::from_text("let x = 42;");

        // Check token count
        assert!(tree.token_count() > 0);

        // Check that we have some identifiers
        let identifiers = tree.tokens_of_kind(SyntaxKind::Identifier);
        assert!(!identifiers.is_empty());

        // Check that we can find a number
        let numbers = tree.tokens_of_kind(SyntaxKind::Number);
        assert!(!numbers.is_empty());
    }

    #[test]
    fn test_line_counting() {
        let tree = SyntaxTree::from_text("line1\nline2\nline3");
        assert_eq!(tree.line_count(), 3);
    }

    #[test]
    fn test_token_at_index() {
        let tree = SyntaxTree::from_text("a b c");

        // Should find tokens at various indices
        assert!(tree.token_at_index(0).is_some());
        assert!(tree.token_at_index(1).is_some()); // whitespace
        assert!(tree.token_at_index(2).is_some());
    }

    #[test]
    fn test_error_tokens() {
        let tree = SyntaxTree::from_text("valid @ invalid");

        // @ should be marked as Unknown/error
        let errors = tree.error_tokens();
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_summary() {
        let tree = SyntaxTree::from_text("hello world");
        let summary = tree.summary();

        assert!(summary.token_count > 0);
        assert!(summary.byte_count > 0);
        assert!(!summary.kinds.is_empty());
    }

    #[test]
    fn test_edit_operations() {
        let mut tree = SyntaxTree::from_text("hello world");

        // Test single edit returns transaction ID
        let tx_id = tree.edit(vec![(0..5, "goodbye")]);
        assert!(tx_id.is_some());
        assert_eq!(tree.text(), "goodbye world");

        // Test multiple edits (applied in reverse order)
        tree.edit(vec![(0..7, "hi"), (8..13, "there")]);
        assert_eq!(tree.text(), "hi there");

        // Verify tokens are updated
        assert!(tree.token_count() > 0);
    }

    #[test]
    fn test_coordinate_conversions() {
        let tree = SyntaxTree::from_text("line1\nline2\nline3");

        // Test offset to point
        let point = tree.offset_to_point(6); // Start of line2
        assert_eq!(point.row, 1);
        assert_eq!(point.column, 0);

        // Test point to offset
        let offset = tree.point_to_offset(Point::new(2, 0)); // Start of line3
        assert_eq!(offset, 12);

        // Test max point
        let max = tree.max_point();
        assert_eq!(max.row, 2);
    }

    #[test]
    fn test_efficient_text_access() {
        let tree = SyntaxTree::from_text("hello world");

        // Test length methods
        assert_eq!(tree.len(), 11);
        assert!(!tree.is_empty());

        // Test chunks iterator (zero-copy)
        let chunks: Vec<_> = tree.chunks().collect();
        assert!(!chunks.is_empty());

        // Test text slice (returns rope, not string)
        let slice = tree.text_slice(0..5);
        assert_eq!(slice.to_string(), "hello");
    }

    #[test]
    fn test_range_queries() {
        let tree = SyntaxTree::from_text("let x = 42;");

        // Test tokens in range
        let tokens = tree.tokens_in_range(4..10);
        assert!(!tokens.is_empty());

        // Test cursor at point
        let cursor = tree.cursor_at_point(Point::new(0, 4));
        assert!(cursor.item().is_some());
    }

    #[test]
    fn test_cursor_navigation() {
        let tree = SyntaxTree::from_text("let x = 42;");

        // Test basic cursor creation
        let mut cursor = tree.cursor::<ByteOffset>();
        cursor.next(); // Position cursor at first item
        assert!(cursor.item().is_some());

        let first_token = cursor.item().unwrap();
        cursor.next();
        // After next, should have moved to next token
        if let Some(second_token) = cursor.item() {
            assert!(first_token.range.start != second_token.range.start);
        }

        // Test cursor at offset
        let cursor_at_5 = tree.cursor_at_offset(5);
        // Should be positioned near the middle of the text
        assert!(cursor_at_5.item().is_some());
    }

    #[test]
    fn test_undo_redo() {
        let mut tree = SyntaxTree::from_text("hello world");

        // Make an edit
        tree.edit(vec![(0..5, "goodbye")]);
        assert_eq!(tree.text(), "goodbye world");
        assert!(tree.can_undo());

        // Undo (should restore "hello")
        assert!(tree.undo());
        assert_eq!(tree.text(), "hello world");
        assert!(tree.can_redo());

        // Redo
        assert!(tree.redo());
        assert_eq!(tree.text(), "goodbye world");

        // Make another edit (should clear redo stack)
        tree.edit(vec![(8..13, "there")]);
        assert_eq!(tree.text(), "goodbye there");
        assert!(!tree.can_redo());

        // Undo twice
        assert!(tree.undo());
        assert_eq!(tree.text(), "goodbye world");
        assert!(tree.undo());
        assert_eq!(tree.text(), "hello world");
    }
}
