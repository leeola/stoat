//! Syntax kinds for the rope AST - copied from stoat_rope

use std::fmt;

/// Syntax kinds for the rope AST
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

    // === Formatting ===
    /// Emphasized text (*text*)
    Emphasis,
    /// Strong/bold text (**text**)
    Strong,
    /// Code span (`code`)
    CodeSpan,
    /// Code block (```code```)
    CodeBlock,
    /// Heading
    Heading,

    // === Programming Tokens ===
    /// Identifier (variable, function, type names)
    Identifier,
    /// Word part within an identifier or text
    Word,
    /// Separator within identifiers (underscore, dash)
    Separator,
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
    /// Left parenthesis (
    OpenParen,
    /// Right parenthesis )
    CloseParen,
    /// Left square bracket [
    OpenBracket,
    /// Right square bracket ]
    CloseBracket,
    /// Left curly brace {
    OpenBrace,
    /// Right curly brace }
    CloseBrace,
    /// Comma ,
    Comma,
    /// Semicolon ;
    Semicolon,
    /// Colon :
    Colon,
    /// Period/dot .
    Dot,
    /// Arrow ->
    Arrow,

    // === Comments ===
    /// Single-line comment
    LineComment,
    /// Multi-line comment
    BlockComment,
    /// Documentation comment
    DocComment,

    // === Whitespace ===
    /// Whitespace (spaces, tabs)
    Whitespace,
    /// Line break
    Newline,

    // === Special ===
    /// Unknown or error token
    Unknown,
    /// End of file
    Eof,
}

impl SyntaxKind {
    /// Returns true if this kind represents a token (leaf node with text)
    pub fn is_token(&self) -> bool {
        matches!(
            self,
            SyntaxKind::Text
                | SyntaxKind::Identifier
                | SyntaxKind::Word
                | SyntaxKind::Separator
                | SyntaxKind::Number
                | SyntaxKind::String
                | SyntaxKind::Char
                | SyntaxKind::Boolean
                | SyntaxKind::Keyword
                | SyntaxKind::Operator
                | SyntaxKind::OpenParen
                | SyntaxKind::CloseParen
                | SyntaxKind::OpenBracket
                | SyntaxKind::CloseBracket
                | SyntaxKind::OpenBrace
                | SyntaxKind::CloseBrace
                | SyntaxKind::Comma
                | SyntaxKind::Semicolon
                | SyntaxKind::Colon
                | SyntaxKind::Dot
                | SyntaxKind::Arrow
                | SyntaxKind::LineComment
                | SyntaxKind::BlockComment
                | SyntaxKind::DocComment
                | SyntaxKind::Whitespace
                | SyntaxKind::Newline
                | SyntaxKind::Unknown
                | SyntaxKind::Eof
        )
    }

    /// Returns true if this kind represents trivia (whitespace, comments, etc.)
    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            SyntaxKind::Whitespace
                | SyntaxKind::Newline
                | SyntaxKind::LineComment
                | SyntaxKind::BlockComment
                | SyntaxKind::DocComment
        )
    }

    /// Get a human-readable name for this syntax kind
    pub fn as_str(&self) -> &'static str {
        match self {
            // Document Structure
            SyntaxKind::Document => "document",
            SyntaxKind::Module => "module",
            SyntaxKind::Block => "block",

            // Text Structure
            SyntaxKind::Paragraph => "paragraph",
            SyntaxKind::Text => "text",
            SyntaxKind::Line => "line",

            // Formatting
            SyntaxKind::Emphasis => "emphasis",
            SyntaxKind::Strong => "strong",
            SyntaxKind::CodeSpan => "code_span",
            SyntaxKind::CodeBlock => "code_block",
            SyntaxKind::Heading => "heading",

            // Programming Tokens
            SyntaxKind::Identifier => "identifier",
            SyntaxKind::Word => "word",
            SyntaxKind::Separator => "separator",
            SyntaxKind::Number => "number",
            SyntaxKind::String => "string",
            SyntaxKind::Char => "char",
            SyntaxKind::Boolean => "boolean",
            SyntaxKind::Keyword => "keyword",
            SyntaxKind::Operator => "operator",

            // Punctuation
            SyntaxKind::OpenParen => "open_paren",
            SyntaxKind::CloseParen => "close_paren",
            SyntaxKind::OpenBracket => "open_bracket",
            SyntaxKind::CloseBracket => "close_bracket",
            SyntaxKind::OpenBrace => "open_brace",
            SyntaxKind::CloseBrace => "close_brace",
            SyntaxKind::Comma => "comma",
            SyntaxKind::Semicolon => "semicolon",
            SyntaxKind::Colon => "colon",
            SyntaxKind::Dot => "dot",
            SyntaxKind::Arrow => "arrow",

            // Comments
            SyntaxKind::LineComment => "line_comment",
            SyntaxKind::BlockComment => "block_comment",
            SyntaxKind::DocComment => "doc_comment",

            // Whitespace
            SyntaxKind::Whitespace => "whitespace",
            SyntaxKind::Newline => "newline",

            // Special
            SyntaxKind::Unknown => "unknown",
            SyntaxKind::Eof => "eof",
        }
    }
}

impl fmt::Display for SyntaxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
