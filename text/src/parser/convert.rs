//! Conversion from tree-sitter AST to rope AST

use crate::parser::{Language, ParseError};
use std::sync::Arc;
use stoat_rope::{
    ast::{AstNode, TextRange},
    builder::AstBuilder,
    kind::SyntaxKind,
};
use tree_sitter::{Node as TsNode, Tree};

/// Convert a tree-sitter tree to a rope AST
pub fn convert_tree(
    tree: &Tree,
    source: &str,
    language: Language,
) -> Result<Arc<AstNode>, ParseError> {
    let root = tree.root_node();

    // If root is not a document node, wrap it
    if root.kind() != "document" {
        let range = TextRange::new(root.start_byte(), root.end_byte());
        let mut builder = AstBuilder::start_node(SyntaxKind::Document, range);
        let child = convert_node(&root, source, language)?;
        builder = builder.add_child(child);
        Ok(builder.finish())
    } else {
        convert_node(&root, source, language)
    }
}

/// Convert a tree-sitter node to a rope AST node
fn convert_node(
    ts_node: &TsNode<'_>,
    source: &str,
    language: Language,
) -> Result<Arc<AstNode>, ParseError> {
    let kind = map_node_kind(ts_node.kind(), language);
    let range = TextRange::new(ts_node.start_byte(), ts_node.end_byte());

    // Special handling for inline nodes - they contain the actual text
    if ts_node.kind() == "inline" {
        // For inline nodes, extract the text directly
        let text =
            ts_node
                .utf8_text(source.as_bytes())
                .map_err(|_| ParseError::ConversionError {
                    message: "Invalid UTF-8".to_string(),
                })?;

        return Ok(AstBuilder::token(SyntaxKind::Text, text, range));
    }

    if ts_node.child_count() == 0 {
        // Leaf node - create token
        let text =
            ts_node
                .utf8_text(source.as_bytes())
                .map_err(|_| ParseError::ConversionError {
                    message: "Invalid UTF-8".to_string(),
                })?;

        Ok(AstBuilder::token(kind, text, range))
    } else {
        // Internal node - create syntax node with children
        let mut builder = AstBuilder::start_node(kind, range);

        // Convert all children
        let mut cursor = ts_node.walk();
        let mut last_end = range.start.0;

        for child in ts_node.children(&mut cursor) {
            // Skip certain node types that we don't want in the AST
            if should_skip_node(child.kind()) {
                continue;
            }

            // Check for gaps between nodes - preserve ALL text
            let child_start = child.start_byte();
            if child_start > last_end {
                // There's a gap - preserve it as text
                let gap_text = &source[last_end..child_start];
                if !gap_text.is_empty() {
                    let gap_range = TextRange::new(last_end, child_start);
                    let gap_node = AstBuilder::token(SyntaxKind::Text, gap_text, gap_range);
                    builder = builder.add_child(gap_node);
                }
            }

            let child_node = convert_node(&child, source, language)?;
            builder = builder.add_child(child_node);
            last_end = child.end_byte();
        }

        // Check for final gap after all children
        if last_end < range.end.0 {
            let final_gap_text = &source[last_end..range.end.0];
            if !final_gap_text.is_empty() {
                let gap_range = TextRange::new(last_end, range.end.0);
                let gap_node = AstBuilder::token(SyntaxKind::Text, final_gap_text, gap_range);
                builder = builder.add_child(gap_node);
            }
        }

        Ok(builder.finish())
    }
}

/// Map tree-sitter node kind to rope AST SyntaxKind
fn map_node_kind(ts_kind: &str, language: Language) -> SyntaxKind {
    match language {
        Language::Markdown => map_markdown_kind(ts_kind),
        Language::PlainText => {
            // PlainText should never use tree-sitter conversion
            unreachable!("PlainText uses manual parsing, not tree-sitter")
        },
    }
}

/// Map markdown-specific node kinds
fn map_markdown_kind(ts_kind: &str) -> SyntaxKind {
    match ts_kind {
        // Document structure
        "document" => SyntaxKind::Document,
        "section" => SyntaxKind::Block,
        "paragraph" => SyntaxKind::Paragraph,
        "heading" | "atx_heading" | "setext_heading" => SyntaxKind::Heading,

        // Handle ERROR nodes specially - they might be headings
        "ERROR" => SyntaxKind::Block,

        // Text content
        "text" => SyntaxKind::Text,
        "code_span" => SyntaxKind::CodeSpan,
        "emphasis" => SyntaxKind::Emphasis,
        "strong_emphasis" => SyntaxKind::Strong,

        // Whitespace and breaks
        "line_break" | "hard_line_break" | "soft_line_break" => SyntaxKind::Newline,

        // Markdown syntax elements - treat as text
        "atx_h1_marker" | "atx_h2_marker" | "atx_h3_marker" | "atx_h4_marker" | "atx_h5_marker"
        | "atx_h6_marker" => SyntaxKind::Text,
        "emphasis_delimiter" => SyntaxKind::Text,
        "code_span_delimiter" => SyntaxKind::Text,

        // Default fallback
        _ => SyntaxKind::Text,
    }
}

/// Check if a node type should be skipped during conversion
fn should_skip_node(_ts_kind: &str) -> bool {
    // Don't skip any nodes - we need all text content
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;

    #[test]
    fn test_simple_markdown_parsing() {
        let mut parser =
            Parser::from_language(Language::Markdown).expect("Failed to create parser");

        let text = "Hello world";
        let rope_ast = parser.parse_text(text).expect("Failed to parse");

        // Basic structure verification
        let root = rope_ast.root();
        assert_eq!(root.kind(), SyntaxKind::Document);

        // Verify content is preserved
        let output = rope_ast.to_string();
        assert_eq!(output, text);
    }

    #[test]
    fn test_markdown_with_heading() {
        let mut parser =
            Parser::from_language(Language::Markdown).expect("Failed to create parser");

        let text = "# Hello\nWorld";
        let rope_ast = parser.parse_text(text).expect("Failed to parse");

        let root = rope_ast.root();
        assert_eq!(root.kind(), SyntaxKind::Document);
    }

    #[test]
    fn test_markdown_structure() {
        let mut parser =
            Parser::from_language(Language::Markdown).expect("Failed to create parser");

        let text = "# Heading\n\nSimple paragraph.\n";
        let rope_ast = parser.parse_text(text).expect("Failed to parse");

        // Verify the AST structure
        let root = rope_ast.root();
        assert_eq!(root.kind(), SyntaxKind::Document);

        // The document should have content
        assert!(root.children().is_some());

        // Verify we can convert back to text (even if not perfect yet)
        let output = rope_ast.to_string();
        assert!(!output.is_empty());
    }
}
