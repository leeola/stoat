//! Edit operations for AST-based text editing

use crate::{
    TextSize,
    range::TextRange,
    syntax::{Syntax, SyntaxNode},
};
use snafu::Snafu;

/// Errors that can occur during edit operations
#[derive(Debug, Snafu)]
pub enum EditError {
    #[snafu(display("Invalid range: start {:?} > end {:?}", start, end))]
    InvalidRange { start: TextSize, end: TextSize },

    #[snafu(display("Range out of bounds: {:?} > buffer length {:?}", position, length))]
    RangeOutOfBounds {
        position: TextSize,
        length: TextSize,
    },

    #[snafu(display("Node not found in buffer"))]
    NodeNotFound,

    #[snafu(display("Buffer not accessible"))]
    BufferNotAccessible,

    #[snafu(display("Conflicting edit at position {:?}", position))]
    ConflictingEdit { position: TextSize },
}

/// An edit operation on the AST
#[derive(Clone)]
pub struct Edit<S: Syntax> {
    /// The target node to edit
    pub target: SyntaxNode<S>,
    /// The operation to perform
    pub operation: EditOperation,
}

/// Types of edit operations
#[derive(Debug, Clone)]
pub enum EditOperation {
    /// Replace the entire node's text
    Replace(String),
    /// Insert text before the node
    InsertBefore(String),
    /// Insert text after the node
    InsertAfter(String),
    /// Insert at a specific offset within the node
    InsertAt { offset: usize, text: String },
    /// Delete the node
    Delete,
    /// Wrap the node with text before and after
    WrapWith { before: String, after: String },
    /// Unwrap the node (remove surrounding text)
    Unwrap,
    /// Delete a specific range within the node
    DeleteRange { start: usize, end: usize },
    /// Replace a specific range within the node
    ReplaceRange {
        start: usize,
        end: usize,
        text: String,
    },
}

impl<S: Syntax> Edit<S> {
    /// Create a replace edit
    pub fn replace(target: SyntaxNode<S>, text: String) -> Self {
        Self {
            target,
            operation: EditOperation::Replace(text),
        }
    }

    /// Create an insert before edit
    pub fn insert_before(target: SyntaxNode<S>, text: String) -> Self {
        Self {
            target,
            operation: EditOperation::InsertBefore(text),
        }
    }

    /// Create an insert after edit
    pub fn insert_after(target: SyntaxNode<S>, text: String) -> Self {
        Self {
            target,
            operation: EditOperation::InsertAfter(text),
        }
    }

    /// Create a delete edit
    pub fn delete(target: SyntaxNode<S>) -> Self {
        Self {
            target,
            operation: EditOperation::Delete,
        }
    }

    /// Delete a specific node from the AST
    pub fn delete_node(node: SyntaxNode<S>) -> Self {
        Self {
            target: node,
            operation: EditOperation::Delete,
        }
    }

    /// Replace a specific node's text
    pub fn replace_node(node: SyntaxNode<S>, text: String) -> Self {
        Self {
            target: node,
            operation: EditOperation::Replace(text),
        }
    }

    /// Insert text at a specific offset within a node
    pub fn insert_at_node(node: SyntaxNode<S>, offset: usize, text: String) -> Self {
        Self {
            target: node,
            operation: EditOperation::InsertAt { offset, text },
        }
    }

    /// Delete a specific range within a node
    pub fn delete_range(node: SyntaxNode<S>, start: usize, end: usize) -> Self {
        Self {
            target: node,
            operation: EditOperation::DeleteRange { start, end },
        }
    }

    /// Replace a specific range within a node
    pub fn replace_range(node: SyntaxNode<S>, start: usize, end: usize, text: String) -> Self {
        Self {
            target: node,
            operation: EditOperation::ReplaceRange { start, end, text },
        }
    }
}

/// A low-level edit operation on the rope
#[derive(Debug, Clone)]
pub struct RopeEdit {
    /// The range to replace (empty for pure insert)
    pub range: TextRange,
    /// The text to insert (empty for pure delete)
    pub text: String,
}

impl RopeEdit {
    /// Create a new rope edit
    pub fn new(range: TextRange, text: String) -> Self {
        Self { range, text }
    }

    /// Create an insert edit at a position
    pub fn insert(position: TextSize, text: String) -> Self {
        Self {
            range: TextRange::empty(position),
            text,
        }
    }

    /// Create a delete edit for a range
    pub fn delete(range: TextRange) -> Self {
        Self {
            range,
            text: String::new(),
        }
    }

    /// Create a replace edit
    pub fn replace(range: TextRange, text: String) -> Self {
        Self { range, text }
    }
}
