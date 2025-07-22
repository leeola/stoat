//! Arena-based AST implementation for efficient text editing
//!
//! This crate provides a fast, arena-allocated AST with DAG-based history
//! for efficient undo/redo operations and structural sharing.

pub mod arena;
pub mod builder;
pub mod kind;
pub mod node;
pub mod position;

pub use arena::Arena;
pub use builder::Builder;
pub use kind::SyntaxKind;
pub use node::Node;
pub use position::{TextInfo, TextPos, TextRangeExt};

#[cfg(test)]
mod test_utils;
