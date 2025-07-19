//! Simple word/whitespace syntax for testing

use crate::{
    TextSize,
    range::TextRange,
    syntax::{
        kind::ParseResult,
        node::{SyntaxElement, SyntaxNode, SyntaxToken},
        unified_kind::SyntaxKind,
    },
};
use std::sync::Arc;

/// Simple text syntax
#[derive(Clone)]
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

#[allow(deprecated)]
impl crate::syntax::kind::Syntax for SimpleText {
    type Kind = SimpleKind;

    fn parse(text: &str) -> ParseResult {
        let mut builder = SimpleAstBuilder::new(text);
        let root = builder.build();

        ParseResult {
            root,
            errors: Vec::new(),
        }
    }
}

/// Helper to build the AST
struct SimpleAstBuilder<'a> {
    text: &'a str,
    pos: TextSize,
}

impl<'a> SimpleAstBuilder<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            pos: TextSize::from(0),
        }
    }

    fn build(&mut self) -> SyntaxNode {
        let root_start = self.pos;
        let mut children = Vec::new();

        // Split into lines and parse each
        let lines: Vec<&str> = self.text.split('\n').collect();
        let line_count = lines.len();

        for (i, line_text) in lines.into_iter().enumerate() {
            // Always create a line node for each line
            let line_node = self.parse_line(line_text);
            children.push(SyntaxElement::Node(line_node));

            // Add newline whitespace (except after the last line)
            if i < line_count - 1 {
                let newline_start = self.pos;
                self.pos += TextSize::from(1); // '\n' is 1 byte

                let newline_token = SyntaxToken::new(
                    SyntaxKind::Whitespace,
                    TextRange::new(newline_start, self.pos),
                    Arc::from("\n"),
                );
                children.push(SyntaxElement::Token(newline_token));
            }
        }

        let root_end = self.pos;
        SyntaxNode::new_with_children(
            SyntaxKind::Root,
            TextRange::new(root_start, root_end),
            children,
        )
    }

    fn parse_line(&mut self, line_text: &str) -> SyntaxNode {
        let line_start = self.pos;
        let mut children = Vec::new();
        let mut chars = line_text.char_indices().peekable();

        while let Some((start_idx, ch)) = chars.next() {
            if ch.is_whitespace() {
                // Collect consecutive whitespace
                let ws_start = self.pos + TextSize::from(start_idx as u32);
                let mut end_idx = start_idx + ch.len_utf8();

                while let Some(&(idx, next_ch)) = chars.peek() {
                    if !next_ch.is_whitespace() {
                        break;
                    }
                    end_idx = idx + next_ch.len_utf8();
                    chars.next();
                }

                let ws_text = &line_text[start_idx..end_idx];
                let ws_end = ws_start + TextSize::from(ws_text.len() as u32);

                let ws_token = SyntaxToken::new(
                    SyntaxKind::Whitespace,
                    TextRange::new(ws_start, ws_end),
                    Arc::from(ws_text),
                );
                children.push(SyntaxElement::Token(ws_token));
            } else {
                // Collect consecutive non-whitespace as a word
                let word_start = self.pos + TextSize::from(start_idx as u32);
                let mut end_idx = start_idx + ch.len_utf8();

                while let Some(&(idx, next_ch)) = chars.peek() {
                    if next_ch.is_whitespace() {
                        break;
                    }
                    end_idx = idx + next_ch.len_utf8();
                    chars.next();
                }

                let word_text = &line_text[start_idx..end_idx];
                let word_end = word_start + TextSize::from(word_text.len() as u32);

                let word_token = SyntaxToken::new(
                    SyntaxKind::Word,
                    TextRange::new(word_start, word_end),
                    Arc::from(word_text),
                );
                children.push(SyntaxElement::Token(word_token));
            }
        }

        self.pos += TextSize::from(line_text.len() as u32);
        let line_end = self.pos;

        SyntaxNode::new_with_children(
            SyntaxKind::Line,
            TextRange::new(line_start, line_end),
            children,
        )
    }
}

#[allow(deprecated)]
impl crate::syntax::kind::SyntaxKind for SimpleKind {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(deprecated)]
    use crate::syntax::kind::Syntax;

    #[test]
    fn test_parse_single_line() {
        let text = "hello world";
        let result = SimpleText::parse(text);
        let root = result.root;

        assert_eq!(root.kind(), SyntaxKind::Root);
        assert_eq!(root.text_range(), TextRange::new(0.into(), 11.into()));

        // Check children: should have one Line node
        let children = root.children();
        assert_eq!(children.len(), 1);

        // Check the line node
        let line = match &children[0] {
            SyntaxElement::Node(n) => n,
            _ => panic!("Expected line node"),
        };
        assert_eq!(line.kind(), SyntaxKind::Line);
        assert_eq!(line.text_range(), TextRange::new(0.into(), 11.into()));

        // Check line's children: Word "hello", Whitespace " ", Word "world"
        let line_children = line.children();
        assert_eq!(line_children.len(), 3);

        // First word "hello"
        match &line_children[0] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Word);
                assert_eq!(t.text(), "hello");
                assert_eq!(t.text_range(), TextRange::new(0.into(), 5.into()));
            },
            _ => panic!("Expected word token"),
        }

        // Whitespace
        match &line_children[1] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Whitespace);
                assert_eq!(t.text(), " ");
                assert_eq!(t.text_range(), TextRange::new(5.into(), 6.into()));
            },
            _ => panic!("Expected whitespace token"),
        }

        // Second word "world"
        match &line_children[2] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Word);
                assert_eq!(t.text(), "world");
                assert_eq!(t.text_range(), TextRange::new(6.into(), 11.into()));
            },
            _ => panic!("Expected word token"),
        }
    }

    #[test]
    fn test_parse_multiple_lines() {
        let text = "hello world\nsecond line";
        let result = SimpleText::parse(text);
        let root = result.root;

        assert_eq!(root.kind(), SyntaxKind::Root);
        assert_eq!(root.text_range(), TextRange::new(0.into(), 23.into()));

        // Check children: Line, Whitespace (newline), Line
        let children = root.children();
        assert_eq!(children.len(), 3);

        // First line
        match &children[0] {
            SyntaxElement::Node(n) => {
                assert_eq!(n.kind(), SyntaxKind::Line);
                assert_eq!(n.text_range(), TextRange::new(0.into(), 11.into()));
            },
            _ => panic!("Expected line node"),
        }

        // Newline
        match &children[1] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Whitespace);
                assert_eq!(t.text(), "\n");
                assert_eq!(t.text_range(), TextRange::new(11.into(), 12.into()));
            },
            _ => panic!("Expected newline token"),
        }

        // Second line
        match &children[2] {
            SyntaxElement::Node(n) => {
                assert_eq!(n.kind(), SyntaxKind::Line);
                assert_eq!(n.text_range(), TextRange::new(12.into(), 23.into()));

                // Check it has the right content
                let line_children = n.children();
                assert_eq!(line_children.len(), 3); // "second" " " "line"
            },
            _ => panic!("Expected line node"),
        }
    }

    #[test]
    fn test_parse_empty_lines() {
        let text = "hello\n\nworld";
        let result = SimpleText::parse(text);
        let root = result.root;

        // Should have: Line("hello"), Newline, Line(""), Newline, Line("world")
        let children = root.children();
        assert_eq!(children.len(), 5);

        // Check empty line
        match &children[2] {
            SyntaxElement::Node(n) => {
                assert_eq!(n.kind(), SyntaxKind::Line);
                assert_eq!(n.children().len(), 0); // Empty line has no children
            },
            _ => panic!("Expected empty line node"),
        }
    }

    #[test]
    fn test_navigation() {
        let text = "hello world\ntest";
        let result = SimpleText::parse(text);
        let root = result.root;

        // Test first_child
        let first_line = root.first_child().expect("Should have first child");
        assert_eq!(first_line.kind(), SyntaxKind::Line);

        // Test parent navigation
        assert!(first_line.parent().is_some());
        let parent = first_line.parent().expect("Line should have parent");
        assert_eq!(parent.kind(), SyntaxKind::Root);

        // Test finding words
        let words: Vec<_> = root
            .tokens()
            .into_iter()
            .filter(|t| t.kind() == SyntaxKind::Word)
            .collect();
        assert_eq!(words.len(), 3); // "hello", "world", "test"
        assert_eq!(words[0].text(), "hello");
        assert_eq!(words[1].text(), "world");
        assert_eq!(words[2].text(), "test");
    }

    #[test]
    fn test_ast_enables_word_navigation() {
        let text = "fn hello_world() { println!(\"hello world!\"); }";
        let result = SimpleText::parse(text);
        let root = result.root;

        // The AST should parse this as words and whitespace
        let words: Vec<_> = root
            .tokens()
            .into_iter()
            .filter(|t| t.kind() == SyntaxKind::Word)
            .collect();

        // Check we can navigate through the words
        assert!(!words.is_empty());
        assert_eq!(words[0].text(), "fn");
        assert_eq!(words[1].text(), "hello_world()");
        assert_eq!(words[2].text(), "{");
        assert_eq!(words[3].text(), "println!(\"hello");
        assert_eq!(words[4].text(), "world!\");");
        assert_eq!(words[5].text(), "}");
    }
}
