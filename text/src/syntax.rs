//! AST and syntax tree types

pub mod compat;
pub mod flat_ast;
pub mod flat_builder;
pub mod incremental;
pub mod kind;
pub mod node;
pub mod simple;
pub mod tree;

pub use compat::{AstBridge, FlatSyntaxNode};
pub use flat_ast::{ElementId, FlatAst, NodeData, NodeId, SyntaxNodeRef, TokenData, TokenId};
pub use flat_builder::FlatTreeBuilder;
pub use incremental::{IncrementalParseError, IncrementalParser, InvalidationSet, TextChange};
pub use kind::{Syntax, SyntaxKind};
pub use node::{SyntaxElement, SyntaxNode, SyntaxToken};
pub use simple::{SimpleKind, SimpleText};
