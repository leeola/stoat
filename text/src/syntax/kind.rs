//! Syntax kind traits for extensible language support

use std::fmt::Debug;

/// Trait for syntax types (e.g., different languages)
pub trait Syntax: 'static + Sized + Clone {
    /// The kind of syntax nodes
    type Kind: SyntaxKind;

    /// Parse text into an AST
    fn parse(text: &str) -> ParseResult<Self>;
}

/// Trait for syntax node kinds
pub trait SyntaxKind: Copy + Clone + Eq + PartialEq + Debug + 'static {
    /// Check if this is a token (leaf node)
    fn is_token(&self) -> bool;

    /// Check if this is trivia (whitespace, comments)
    fn is_trivia(&self) -> bool;

    /// Get a human-readable name for this kind
    fn name(&self) -> &'static str;
}

/// Result of parsing
pub struct ParseResult<S: Syntax> {
    /// The parsed syntax tree
    pub root: crate::syntax::SyntaxNode<S>,
    /// Any parsing errors
    pub errors: Vec<ParseError>,
}

/// A parsing error
#[derive(Debug, Clone)]
pub struct ParseError {
    /// The range where the error occurred
    pub range: crate::range::TextRange,
    /// Error message
    pub message: String,
}
