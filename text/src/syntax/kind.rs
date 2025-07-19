//! Core parsing types and unified syntax kind for the syntax system

use std::fmt;

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

/// Unified syntax kind that supports multiple languages
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxKind {
    // Simple text kinds
    Root,
    Word,
    Whitespace,
    Line,

    // Markdown kinds
    Document,
    Section,
    Paragraph,
    Heading,
    CodeBlock,
    FencedCodeBlock,
    BlockQuote,
    List,
    ListItem,
    Text,
    Strong,
    Emphasis,
    Code,
    Link,
    Image,
    HeadingMarker,
    CodeFence,
    LinkText,
    LinkDestination,
    Newline,
    Error,
}

impl SyntaxKind {
    /// Check if this is a token (leaf node)
    pub fn is_token(&self) -> bool {
        matches!(
            self,
            SyntaxKind::Word
                | SyntaxKind::Whitespace
                | SyntaxKind::Text
                | SyntaxKind::HeadingMarker
                | SyntaxKind::CodeFence
                | SyntaxKind::LinkText
                | SyntaxKind::LinkDestination
                | SyntaxKind::Newline
                | SyntaxKind::Error
        )
    }

    /// Check if this is trivia (whitespace, comments)
    pub fn is_trivia(&self) -> bool {
        matches!(self, SyntaxKind::Whitespace | SyntaxKind::Newline)
    }

    /// Get a human-readable name for this kind
    pub fn name(&self) -> &'static str {
        match self {
            // Simple text
            SyntaxKind::Root => "root",
            SyntaxKind::Word => "word",
            SyntaxKind::Whitespace => "whitespace",
            SyntaxKind::Line => "line",

            // Markdown
            SyntaxKind::Document => "document",
            SyntaxKind::Section => "section",
            SyntaxKind::Paragraph => "paragraph",
            SyntaxKind::Heading => "heading",
            SyntaxKind::CodeBlock => "code_block",
            SyntaxKind::FencedCodeBlock => "fenced_code_block",
            SyntaxKind::BlockQuote => "block_quote",
            SyntaxKind::List => "list",
            SyntaxKind::ListItem => "list_item",
            SyntaxKind::Text => "text",
            SyntaxKind::Strong => "strong",
            SyntaxKind::Emphasis => "emphasis",
            SyntaxKind::Code => "code",
            SyntaxKind::Link => "link",
            SyntaxKind::Image => "image",
            SyntaxKind::HeadingMarker => "heading_marker",
            SyntaxKind::CodeFence => "code_fence",
            SyntaxKind::LinkText => "link_text",
            SyntaxKind::LinkDestination => "link_destination",
            SyntaxKind::Newline => "newline",
            SyntaxKind::Error => "error",
        }
    }
}

impl fmt::Display for SyntaxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

// Conversion implementations for migration
impl From<crate::syntax::simple::SimpleKind> for SyntaxKind {
    fn from(kind: crate::syntax::simple::SimpleKind) -> Self {
        match kind {
            crate::syntax::simple::SimpleKind::Root => SyntaxKind::Root,
            crate::syntax::simple::SimpleKind::Word => SyntaxKind::Word,
            crate::syntax::simple::SimpleKind::Whitespace => SyntaxKind::Whitespace,
            crate::syntax::simple::SimpleKind::Line => SyntaxKind::Line,
        }
    }
}

impl From<crate::syntax::markdown::MarkdownKind> for SyntaxKind {
    fn from(kind: crate::syntax::markdown::MarkdownKind) -> Self {
        match kind {
            crate::syntax::markdown::MarkdownKind::Document => SyntaxKind::Document,
            crate::syntax::markdown::MarkdownKind::Section => SyntaxKind::Section,
            crate::syntax::markdown::MarkdownKind::Paragraph => SyntaxKind::Paragraph,
            crate::syntax::markdown::MarkdownKind::Heading => SyntaxKind::Heading,
            crate::syntax::markdown::MarkdownKind::CodeBlock => SyntaxKind::CodeBlock,
            crate::syntax::markdown::MarkdownKind::FencedCodeBlock => SyntaxKind::FencedCodeBlock,
            crate::syntax::markdown::MarkdownKind::BlockQuote => SyntaxKind::BlockQuote,
            crate::syntax::markdown::MarkdownKind::List => SyntaxKind::List,
            crate::syntax::markdown::MarkdownKind::ListItem => SyntaxKind::ListItem,
            crate::syntax::markdown::MarkdownKind::Text => SyntaxKind::Text,
            crate::syntax::markdown::MarkdownKind::Strong => SyntaxKind::Strong,
            crate::syntax::markdown::MarkdownKind::Emphasis => SyntaxKind::Emphasis,
            crate::syntax::markdown::MarkdownKind::Code => SyntaxKind::Code,
            crate::syntax::markdown::MarkdownKind::Link => SyntaxKind::Link,
            crate::syntax::markdown::MarkdownKind::Image => SyntaxKind::Image,
            crate::syntax::markdown::MarkdownKind::HeadingMarker => SyntaxKind::HeadingMarker,
            crate::syntax::markdown::MarkdownKind::CodeFence => SyntaxKind::CodeFence,
            crate::syntax::markdown::MarkdownKind::LinkText => SyntaxKind::LinkText,
            crate::syntax::markdown::MarkdownKind::LinkDestination => SyntaxKind::LinkDestination,
            crate::syntax::markdown::MarkdownKind::Whitespace => SyntaxKind::Whitespace,
            crate::syntax::markdown::MarkdownKind::Newline => SyntaxKind::Newline,
            crate::syntax::markdown::MarkdownKind::Error => SyntaxKind::Error,
        }
    }
}
