//! AST-based text editing primitives for Stoat
//!
//! This crate provides efficient text editing with a focus on AST-based operations.
//! Text is stored in a rope data structure for efficiency, while all navigation and
//! editing operations work through an AST interface.
//!
//! # Core Concepts
//!
//! - **TextBuffer**: The source of truth containing text and lazily-parsed AST
//! - **TextView**: A window into a specific part of a buffer (e.g., just one function)
//! - **SyntaxNode**: AST nodes that reference positions in the underlying rope
//! - **TextCursor**: Navigation and editing within a view

pub mod action;
pub mod buffer;
pub mod cursor;
pub mod cursor_collection;
pub mod edit;
pub mod flat_buffer;
pub mod query;
pub mod range;
pub mod syntax;
pub mod view;

#[cfg(test)]
pub mod test_helpers;

// Re-export core types
pub use action::{ActionError, ActionResult, ExecutionResult, TextAction};
pub use buffer::{BufferId, TextBuffer};
pub use cursor::TextCursor;
pub use edit::{Edit, EditError, EditOperation, FlatEdit, RopeEdit};
pub use flat_buffer::FlatTextBuffer;
pub use range::TextRange;
pub use syntax::{
    FlatAst, FlatSyntaxNode, IncrementalParser, MarkdownKind, MarkdownSyntax, Syntax, SyntaxKind,
    SyntaxNode, TextChange,
};
// Re-export text-size for convenience
pub use text_size::{TextLen, TextSize};
pub use view::TextView;
