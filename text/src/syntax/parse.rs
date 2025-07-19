//! Unified parsing implementation for all syntax types

use crate::{
    TextSize,
    range::TextRange,
    syntax::{
        flat_ast::FlatAst,
        flat_builder::FlatTreeBuilder,
        node::{SyntaxElement, SyntaxNode, SyntaxToken},
        unified_kind::SyntaxKind,
    },
};
use std::sync::Arc;

/// Parse result for the unified syntax system
pub struct ParseResult {
    /// The parsed syntax tree
    pub root: SyntaxNode,
    /// Any parsing errors
    pub errors: Vec<ParseError>,
}

/// A parsing error
#[derive(Debug, Clone)]
pub struct ParseError {
    /// The range where the error occurred
    pub range: TextRange,
    /// Error message
    pub message: String,
}

/// Parse text as markdown (default syntax)
pub fn parse_markdown(text: &str) -> ParseResult {
    // For now, create a simple AST structure
    // FIXME: Implement proper markdown parsing with tree-sitter
    let root = SyntaxNode::new_with_children(
        SyntaxKind::Document,
        TextRange::new(0.into(), (text.len() as u32).into()),
        vec![SyntaxElement::Token(SyntaxToken::new(
            SyntaxKind::Text,
            TextRange::new(0.into(), (text.len() as u32).into()),
            Arc::from(text),
        ))],
    );

    ParseResult {
        root,
        errors: Vec::new(),
    }
}

/// Parse text as simple word/whitespace syntax
pub fn parse_simple(text: &str) -> ParseResult {
    let mut builder = SimpleAstBuilder::new(text);
    let root = builder.build();

    ParseResult {
        root,
        errors: Vec::new(),
    }
}

/// Parse text using the default syntax (markdown)
pub fn parse(text: &str) -> ParseResult {
    parse_markdown(text)
}

/// Helper to build simple AST
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
                let mut ws_end_idx = start_idx + ch.len_utf8();

                while let Some(&(idx, next_ch)) = chars.peek() {
                    if next_ch.is_whitespace() {
                        ws_end_idx = idx + next_ch.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }

                let ws_text = &line_text[start_idx..ws_end_idx];
                let ws_end = self.pos + TextSize::from(ws_end_idx as u32);

                children.push(SyntaxElement::Token(SyntaxToken::new(
                    SyntaxKind::Whitespace,
                    TextRange::new(ws_start, ws_end),
                    Arc::from(ws_text),
                )));
            } else {
                // Collect consecutive non-whitespace as a word
                let word_start = self.pos + TextSize::from(start_idx as u32);
                let mut word_end_idx = start_idx + ch.len_utf8();

                while let Some(&(idx, next_ch)) = chars.peek() {
                    if !next_ch.is_whitespace() {
                        word_end_idx = idx + next_ch.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }

                let word_text = &line_text[start_idx..word_end_idx];
                let word_end = self.pos + TextSize::from(word_end_idx as u32);

                children.push(SyntaxElement::Token(SyntaxToken::new(
                    SyntaxKind::Word,
                    TextRange::new(word_start, word_end),
                    Arc::from(word_text),
                )));
            }
        }

        // Update position for the line
        self.pos += TextSize::from(line_text.len() as u32);

        let line_end = self.pos;
        SyntaxNode::new_with_children(
            SyntaxKind::Line,
            TextRange::new(line_start, line_end),
            children,
        )
    }
}

/// Build a flat AST from text
pub fn parse_to_flat_ast(text: &str) -> FlatAst {
    let mut builder = FlatTreeBuilder::new();

    // For now, create a simple flat AST
    builder.start_node(SyntaxKind::Root);
    builder.add_token(SyntaxKind::Text, text.to_string());
    builder.finish_node();

    builder.finish()
}
