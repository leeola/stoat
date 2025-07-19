//! AST and syntax tree types

pub mod compat;
pub mod flat_ast;
pub mod flat_builder;
pub mod incremental;
pub mod kind;
pub mod markdown;
pub mod node;
pub mod parse;
pub mod simple;
pub mod tree;
// FIXME: Rename module to `kind`.
pub mod unified_kind;

pub use compat::{AstBridge, FlatSyntaxNode};
pub use flat_ast::{ElementId, FlatAst, NodeData, NodeId, SyntaxNodeRef, TokenData, TokenId};
pub use flat_builder::FlatTreeBuilder;
pub use incremental::{IncrementalParseError, IncrementalParser, InvalidationSet, TextChange};
pub use kind::{ParseError, ParseResult};
pub use markdown::MarkdownKind;
pub use node::{SyntaxElement, SyntaxNode, SyntaxToken};
pub use parse::{parse, parse_markdown, parse_simple, parse_to_flat_ast};
pub use simple::SimpleKind;
pub use unified_kind::SyntaxKind as UnifiedSyntaxKind;
