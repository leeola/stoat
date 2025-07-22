//! Syntax kinds for the AST

/// Syntax kinds for the AST
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxKind {
    // === Document Structure ===
    /// Root document node
    Document,
    /// Module or namespace
    Module,
    /// Code block or scope
    Block,

    // === Text Structure ===
    /// Paragraph - a block of text
    Paragraph,
    /// Plain text content
    Text,
    /// Line of text
    Line,

    // === Programming Tokens ===
    /// Identifier (variable, function, type names)
    Identifier,
    /// Numeric literal
    Number,
    /// String literal
    String,
    /// Character literal
    Char,
    /// Boolean literal
    Boolean,
    /// Language keyword
    Keyword,
    /// Operator (+, -, *, /, etc.)
    Operator,

    // === Punctuation ===
    /// Whitespace
    Whitespace,
    /// Newline character
    Newline,
    /// Comment
    Comment,

    // === Special ===
    /// Unknown or error token
    Unknown,
}
