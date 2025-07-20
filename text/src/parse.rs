//! Language parsing for rope AST construction
//!
//! Provides the [`Parse`] type that defines how to parse different programming languages
//! into the rope AST structure. This enables syntax-aware editing and allows the editor
//! to understand code structure across different languages.

use std::sync::Arc;
use stoat_rope::{ast::AstNode, kind::SyntaxKind};

/// Token type representing a syntax kind and its text
pub type Token = (SyntaxKind, String);

/// Tokenizer function type
pub type Tokenizer = Arc<dyn Fn(&str) -> Vec<Token> + Send + Sync>;

/// Parser function type
pub type Parser = Arc<dyn Fn(Vec<Token>) -> Result<Arc<AstNode>, String> + Send + Sync>;

/// Language parser configuration
///
/// Parse defines how to convert raw text into a structured rope AST for a specific
/// language. Different languages will have different parsing rules, token types,
/// and syntax node structures.
#[derive(Clone)]
#[allow(dead_code)]
pub struct Parse {
    /// Language identifier (e.g., "rust", "python", "markdown")
    language: String,

    /// File extensions associated with this language
    extensions: Vec<String>,

    /// Function to tokenize text
    tokenizer: Tokenizer,

    /// Function to build AST from tokens
    parser: Parser,
}

impl Parse {
    // Implementation will follow in later phases
}
