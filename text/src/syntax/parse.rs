//! Unified parsing implementation for all syntax types

use crate::{
    TextSize,
    range::TextRange,
    syntax::{
        flat_ast::FlatAst,
        flat_builder::FlatTreeBuilder,
        kind::SyntaxKind,
        node::{SyntaxElement, SyntaxNode, SyntaxToken},
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
    use tree_sitter::Parser;

    // Create tree-sitter parser
    let mut parser = Parser::new();
    let language = tree_sitter_markdown::language();

    if let Err(e) = parser.set_language(language) {
        // If we can't set up tree-sitter, fall back to simple structure
        let root = SyntaxNode::new_with_children(
            SyntaxKind::Document,
            TextRange::new(0.into(), (text.len() as u32).into()),
            vec![SyntaxElement::Token(SyntaxToken::new(
                SyntaxKind::Text,
                TextRange::new(0.into(), (text.len() as u32).into()),
                Arc::from(text),
            ))],
        );

        return ParseResult {
            root,
            errors: vec![ParseError {
                range: TextRange::new(0.into(), 0.into()),
                message: format!("Failed to initialize markdown parser: {e}"),
            }],
        };
    }

    // Parse the text
    let tree = match parser.parse(text, None) {
        Some(tree) => tree,
        None => {
            // Parse failed, return simple structure
            let root = SyntaxNode::new_with_children(
                SyntaxKind::Document,
                TextRange::new(0.into(), (text.len() as u32).into()),
                vec![SyntaxElement::Token(SyntaxToken::new(
                    SyntaxKind::Text,
                    TextRange::new(0.into(), (text.len() as u32).into()),
                    Arc::from(text),
                ))],
            );

            return ParseResult {
                root,
                errors: vec![ParseError {
                    range: TextRange::new(0.into(), 0.into()),
                    message: "Failed to parse markdown".to_string(),
                }],
            };
        },
    };

    // Convert tree-sitter tree to our AST
    let root = convert_tree_to_syntax_node(&tree, text);

    // Check for errors in the tree
    let errors = collect_parse_errors(&tree, text);

    ParseResult { root, errors }
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
    parse_markdown_to_flat_ast(text)
}

/// Parse markdown text directly to flat AST
pub fn parse_markdown_to_flat_ast(text: &str) -> FlatAst {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    let language = tree_sitter_markdown::language();

    if parser.set_language(language).is_err() {
        // Fallback to simple structure
        let mut builder = FlatTreeBuilder::new();
        builder.start_node(SyntaxKind::Document);
        builder.add_token(SyntaxKind::Text, text.to_string());
        builder.finish_node();
        return builder.finish();
    }

    match parser.parse(text, None) {
        Some(tree) => {
            // Use the existing conversion from markdown.rs
            crate::syntax::markdown::convert_tree_to_flat_ast(&tree, text)
        },
        None => {
            // Fallback to simple structure
            let mut builder = FlatTreeBuilder::new();
            builder.start_node(SyntaxKind::Document);
            builder.add_token(SyntaxKind::Text, text.to_string());
            builder.finish_node();
            builder.finish()
        },
    }
}

/// Convert tree-sitter tree to our SyntaxNode structure
fn convert_tree_to_syntax_node(tree: &tree_sitter::Tree, text: &str) -> SyntaxNode {
    let root_node = tree.root_node();
    convert_ts_node_to_syntax_node(root_node, text.as_bytes(), TextSize::from(0))
}

/// Recursively convert a tree-sitter node to our SyntaxNode
fn convert_ts_node_to_syntax_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    _offset: TextSize,
) -> SyntaxNode {
    let kind = map_tree_sitter_kind(node.kind());
    let range = TextRange::new(
        (node.start_byte() as u32).into(),
        (node.end_byte() as u32).into(),
    );

    let mut children = Vec::new();

    if node.child_count() == 0 {
        // Leaf node - create a token
        let text = node.utf8_text(source).unwrap_or("<invalid>");
        children.push(SyntaxElement::Token(SyntaxToken::new(
            kind,
            range,
            Arc::from(text),
        )));

        // For leaf nodes, return a node containing the token
        SyntaxNode::new_with_children(kind, range, children)
    } else {
        // Internal node - convert children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let child_node = convert_ts_node_to_syntax_node(child, source, TextSize::from(0));
            children.push(SyntaxElement::Node(child_node));
        }

        SyntaxNode::new_with_children(kind, range, children)
    }
}

/// Map tree-sitter node kinds to our SyntaxKind enum
fn map_tree_sitter_kind(ts_kind: &str) -> SyntaxKind {
    // Reuse the mapping from markdown.rs
    crate::syntax::markdown::map_tree_sitter_kind(ts_kind)
}

/// Collect parse errors from tree-sitter tree
fn collect_parse_errors(tree: &tree_sitter::Tree, text: &str) -> Vec<ParseError> {
    let mut errors = Vec::new();
    let mut cursor = tree.walk();

    // Walk the tree looking for ERROR nodes
    collect_errors_recursive(&mut cursor, text.as_bytes(), &mut errors);

    errors
}

/// Recursively collect errors from tree
fn collect_errors_recursive(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    source: &[u8],
    errors: &mut Vec<ParseError>,
) {
    let node = cursor.node();

    if node.kind() == "ERROR" {
        let range = TextRange::new(
            (node.start_byte() as u32).into(),
            (node.end_byte() as u32).into(),
        );

        let text = node.utf8_text(source).unwrap_or("<invalid>");
        errors.push(ParseError {
            range,
            message: format!("Syntax error: unexpected '{text}'"),
        });
    }

    // Check children
    if cursor.goto_first_child() {
        loop {
            collect_errors_recursive(cursor, source, errors);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_markdown() {
        let text = "# Hello World\n\nThis is a paragraph.";
        let result = parse_markdown(text);

        assert_eq!(result.root.kind(), SyntaxKind::Document);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_parse_simple() {
        let text = "hello world\nfoo bar";
        let result = parse_simple(text);

        assert_eq!(result.root.kind(), SyntaxKind::Root);
        assert!(result.errors.is_empty());

        // Verify line structure
        let lines: Vec<_> = result
            .root
            .children()
            .iter()
            .filter_map(|child| match child {
                SyntaxElement::Node(n) if n.kind() == SyntaxKind::Line => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_parse_to_flat_ast() {
        let text = "**bold** text";
        let flat_ast = parse_to_flat_ast(text);

        let root_id = flat_ast.root();
        let root = flat_ast.get_node(root_id).expect("Root should exist");
        assert_eq!(root.kind, SyntaxKind::Document);
    }

    #[test]
    fn test_parse_markdown_error_handling() {
        // Even invalid markdown should parse without crashing
        let text = "**unclosed bold";
        let result = parse_markdown(text);

        // Should still have a document root
        assert_eq!(result.root.kind(), SyntaxKind::Document);
    }

    #[test]
    fn test_parse_empty_text() {
        let result = parse_markdown("");
        assert_eq!(result.root.kind(), SyntaxKind::Document);

        let result = parse_simple("");
        assert_eq!(result.root.kind(), SyntaxKind::Root);
    }
}
