//! stoat_rope_v3 - Token metadata tracking for AST using Zed's infrastructure
//!
//! This crate provides a TokenMap that stores AST token metadata synchronized
//! with a text buffer, following Zed's architecture where text (Rope) and
//! syntax information are stored separately but kept in sync.
//!
//! ## Key Features
//! - O(log n) operations via SumTree
//! - Rich semantic metadata tracking
//! - Synchronization with text buffer edits
//! - Stable positions via anchors
//!
//! ## Example
//! ```ignore
//! use stoat_rope_v3::{TokenMap, SyntaxKind};
//! use text::BufferSnapshot;
//!
//! let mut token_map = TokenMap::new();
//!
//! // Sync with buffer after edits
//! token_map.sync(&buffer_snapshot, &edits);
//!
//! // Query tokens
//! let snapshot = token_map.snapshot();
//! let identifiers = snapshot.tokens_of_kind(SyntaxKind::Identifier, &buffer_snapshot);
//! ```

// Internal modules
mod kinds;
mod language;
mod semantic;
mod token;
mod token_map;

// Public re-exports
pub use kinds::SyntaxKind;
pub use language::Language;
pub use semantic::{SemanticId, SemanticInfo, SemanticKind};
pub use token::{TokenEntry, TokenSummary};
pub use token_map::{TokenMap, TokenSnapshot};

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId, BufferSnapshot};

    fn create_test_buffer(text: &str) -> BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    #[test]
    fn test_token_map_creation() {
        let buffer = create_test_buffer("hello world");
        let token_map = TokenMap::new(&buffer);
        let snapshot = token_map.snapshot();

        // Initially empty
        assert_eq!(snapshot.token_count(&buffer), 0);
    }

    #[test]
    fn test_token_queries() {
        let buffer = create_test_buffer("let x = 42;");
        let mut token_map = TokenMap::new(&buffer);

        // Simulate tokenization by syncing (simplified for test)
        // In real usage, sync() would handle the tokenization
        token_map.sync(&buffer, &[]);

        let snapshot = token_map.snapshot();

        // Note: Without actual edits triggering tokenization,
        // this will be empty. This is expected behavior.
        assert_eq!(snapshot.token_count(&buffer), 0);
    }

    #[test]
    fn test_token_at_offset() {
        let buffer = create_test_buffer("hello world");
        let token_map = TokenMap::new(&buffer);
        let snapshot = token_map.snapshot();

        // Should return None for empty map
        assert!(snapshot.token_at_offset(0, &buffer).is_none());
    }

    #[test]
    fn test_error_tokens() {
        let buffer = create_test_buffer("valid @ invalid");
        let token_map = TokenMap::new(&buffer);
        let snapshot = token_map.snapshot();

        // Initially no errors
        let errors = snapshot.error_tokens(&buffer);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_summary() {
        let buffer = create_test_buffer("hello world");
        let token_map = TokenMap::new(&buffer);
        let snapshot = token_map.snapshot();
        let summary = snapshot.summary(&buffer);

        assert_eq!(summary.token_count, 0);
        assert!(summary.kinds.is_empty());
    }
}
