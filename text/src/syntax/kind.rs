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
