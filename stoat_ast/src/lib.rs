//! Arena-based AST implementation for efficient text editing
//!
//! This crate provides a fast, arena-allocated AST with DAG-based history
//! for efficient undo/redo operations and structural sharing.

pub mod arena;
pub mod kind;
pub mod node;

pub use arena::Arena;
pub use kind::SyntaxKind;
pub use node::Node;

#[cfg(test)]
mod test_utils;
