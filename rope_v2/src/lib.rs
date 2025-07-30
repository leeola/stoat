#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_mut)]
#![allow(clippy::len_without_is_empty)]
#![allow(clippy::new_without_default)]

pub mod builder;
pub mod node;
pub mod query;
pub mod syntax;
pub mod tree;

// Re-export commonly used types
pub use builder::TreeBuilder;
pub use node::{NodeId, SSTNode};
pub use query::{NodeQuery, TreeCursor};
pub use syntax::{LanguageId, SemanticKind};
pub use tree::SyntaxTree;
