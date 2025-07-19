//! Core parsing types for the unified syntax system

/// Result of parsing
pub struct ParseResult {
    /// The parsed syntax tree
    pub root: crate::syntax::SyntaxNode,
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

// TODO: Remove these traits once migration is complete
/// Trait for syntax types (e.g., different languages)
#[deprecated(note = "Use unified syntax system instead")]
pub trait Syntax: 'static + Sized + Clone {
    /// The kind of syntax nodes
    type Kind: SyntaxKind;

    /// Parse text into an AST
    fn parse(text: &str) -> ParseResult;
}

/// Trait for syntax node kinds
#[deprecated(note = "Use unified_kind::SyntaxKind instead")]
pub trait SyntaxKind: Copy + Clone + Eq + PartialEq + std::fmt::Debug + 'static {
    /// Check if this is a token (leaf node)
    fn is_token(&self) -> bool;

    /// Check if this is trivia (whitespace, comments)
    fn is_trivia(&self) -> bool;

    /// Get a human-readable name for this kind
    fn name(&self) -> &'static str;
}
