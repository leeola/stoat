//! Simple word/whitespace syntax for testing

use crate::{
    range::TextRange,
    syntax::{
        kind::{ParseResult, Syntax, SyntaxKind},
        node::SyntaxNode,
    },
};

/// Simple text syntax
pub struct SimpleText;

/// Kinds of nodes in simple text
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimpleKind {
    /// Root node
    Root,
    /// A word
    Word,
    /// Whitespace
    Whitespace,
    /// A line
    Line,
}

impl Syntax for SimpleText {
    type Kind = SimpleKind;

    fn parse(text: &str) -> ParseResult<Self> {
        // For now, just create a root node with the full text
        let root = SyntaxNode::new_with_text(
            SimpleKind::Root,
            TextRange::new(0.into(), (text.len() as u32).into()),
            text.to_string(),
        );

        ParseResult {
            root,
            errors: Vec::new(),
        }
    }
}

impl SyntaxKind for SimpleKind {
    fn is_token(&self) -> bool {
        matches!(self, SimpleKind::Word | SimpleKind::Whitespace)
    }

    fn is_trivia(&self) -> bool {
        matches!(self, SimpleKind::Whitespace)
    }

    fn name(&self) -> &'static str {
        match self {
            SimpleKind::Root => "Root",
            SimpleKind::Word => "Word",
            SimpleKind::Whitespace => "Whitespace",
            SimpleKind::Line => "Line",
        }
    }
}
