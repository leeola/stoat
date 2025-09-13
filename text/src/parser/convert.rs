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
        // Use the language-aware builder for the document root
        let rope_language = match language {
            Language::Markdown => stoat_rope::Language::Markdown,
            Language::PlainText => stoat_rope::Language::PlainText,
        };
        let mut builder =
            AstBuilder::start_node_with_language(SyntaxKind::Document, range, rope_language);
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

    // Special handling for fenced code blocks to detect language
    if ts_node.kind() == "fenced_code_block" {
        return convert_code_block(ts_node, source, language);
    }

    // Special handling for inline nodes - they contain the actual text
    if ts_node.kind() == "inline" {
        // For inline nodes, extract the text directly
        let text =
            ts_node
                .utf8_text(source.as_bytes())
                .map_err(|_| ParseError::ConversionError {
                    message: "Invalid UTF-8".to_string(),
                })?;

        // Convert Language enum to rope Language
        let rope_language = match language {
            Language::Markdown => stoat_rope::Language::Markdown,
            Language::PlainText => stoat_rope::Language::PlainText,
        };
        return Ok(AstBuilder::token_with_language(
            SyntaxKind::Text,
            text,
            range,
            rope_language,
        ));
    }

    if ts_node.child_count() == 0 {
        // Leaf node - create token with language
        let text =
            ts_node
                .utf8_text(source.as_bytes())
                .map_err(|_| ParseError::ConversionError {
                    message: "Invalid UTF-8".to_string(),
                })?;

        // Convert Language enum to rope Language
        let rope_language = match language {
            Language::Markdown => stoat_rope::Language::Markdown,
            Language::PlainText => stoat_rope::Language::PlainText,
        };
        Ok(AstBuilder::token_with_language(
            kind,
            text,
            range,
            rope_language,
        ))
    } else {
        // Internal node - create syntax node with children and language
        let rope_language = match language {
            Language::Markdown => stoat_rope::Language::Markdown,
            Language::PlainText => stoat_rope::Language::PlainText,
        };
        let mut builder = AstBuilder::start_node_with_language(kind, range, rope_language);

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
                // There's a gap - preserve it as text with language
                let gap_text = &source[last_end..child_start];
                if !gap_text.is_empty() {
                    let gap_range = TextRange::new(last_end, child_start);
                    let gap_node = AstBuilder::token_with_language(
                        SyntaxKind::Text,
                        gap_text,
                        gap_range,
                        rope_language,
                    );
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
                let gap_node = AstBuilder::token_with_language(
                    SyntaxKind::Text,
                    final_gap_text,
                    gap_range,
                    rope_language,
                );
                builder = builder.add_child(gap_node);
            }
        }

        Ok(builder.finish())
    }
}

/// Convert a fenced code block with language detection
fn convert_code_block(
    ts_node: &TsNode<'_>,
    source: &str,
    parent_language: Language,
) -> Result<Arc<AstNode>, ParseError> {
    let range = TextRange::new(ts_node.start_byte(), ts_node.end_byte());
    // Code blocks themselves should have the parent language (markdown)
    let rope_language = match parent_language {
        Language::Markdown => stoat_rope::Language::Markdown,
        Language::PlainText => stoat_rope::Language::PlainText,
    };
    let mut builder =
        AstBuilder::start_node_with_language(SyntaxKind::CodeBlock, range, rope_language);

    // Look for info_string to determine the language
    let mut code_language = None;
    let mut cursor = ts_node.walk();

    for child in ts_node.children(&mut cursor) {
        if child.kind() == "info_string" {
            // Extract the language identifier
            let _lang_text =
                child
                    .utf8_text(source.as_bytes())
                    .map_err(|_| ParseError::ConversionError {
                        message: "Invalid UTF-8 in info_string".to_string(),
                    })?;

            // Look for the actual language node inside info_string
            let mut lang_cursor = child.walk();
            for lang_child in child.children(&mut lang_cursor) {
                if lang_child.kind() == "language" {
                    let lang_name = lang_child.utf8_text(source.as_bytes()).map_err(|_| {
                        ParseError::ConversionError {
                            message: "Invalid UTF-8 in language".to_string(),
                        }
                    })?;

                    // Map language string to our Language enum
                    code_language = map_language_string(lang_name);
                    break;
                }
            }
        }
    }

    // Now convert all children with the detected language
    cursor = ts_node.walk();
    let mut last_end = range.start.0;

    for child in ts_node.children(&mut cursor) {
        // Skip certain node types that we don't want in the AST
        if should_skip_node(child.kind()) {
            continue;
        }

        // Check for gaps between nodes - preserve ALL text
        let child_start = child.start_byte();
        if child_start > last_end {
            // There's a gap - preserve it as text with parent language
            let gap_text = &source[last_end..child_start];
            if !gap_text.is_empty() {
                let gap_range = TextRange::new(last_end, child_start);
                let gap_node = AstBuilder::token_with_language(
                    SyntaxKind::Text,
                    gap_text,
                    gap_range,
                    rope_language,
                );
                builder = builder.add_child(gap_node);
            }
        }

        // For code_fence_content, apply the detected language
        let child_node = if child.kind() == "code_fence_content" && code_language.is_some() {
            // Create the content with the detected language
            let content_range = TextRange::new(child.start_byte(), child.end_byte());
            let text =
                child
                    .utf8_text(source.as_bytes())
                    .map_err(|_| ParseError::ConversionError {
                        message: "Invalid UTF-8 in code content".to_string(),
                    })?;
            AstBuilder::token_with_language(
                SyntaxKind::Text,
                text,
                content_range,
                code_language.unwrap(),
            )
        } else {
            convert_node(&child, source, parent_language)?
        };

        builder = builder.add_child(child_node);
        last_end = child.end_byte();
    }

    // Check for final gap after all children
    if last_end < range.end.0 {
        let final_gap_text = &source[last_end..range.end.0];
        if !final_gap_text.is_empty() {
            let gap_range = TextRange::new(last_end, range.end.0);
            let gap_node = AstBuilder::token_with_language(
                SyntaxKind::Text,
                final_gap_text,
                gap_range,
                rope_language,
            );
            builder = builder.add_child(gap_node);
        }
    }

    Ok(builder.finish())
}

/// Map a language string from markdown to our Language enum
fn map_language_string(lang: &str) -> Option<stoat_rope::Language> {
    match lang.trim().to_lowercase().as_str() {
        "text" | "txt" | "plain" | "plaintext" => Some(stoat_rope::Language::PlainText),
        "markdown" | "md" => Some(stoat_rope::Language::Markdown),
        "rust" | "rs" => Some(stoat_rope::Language::Rust),
        _ => None, // Unknown language
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

        // Code blocks
        "fenced_code_block" => SyntaxKind::CodeBlock,
        "code_fence_content" => SyntaxKind::Text, // The actual code content
        "info_string" => SyntaxKind::Text,        // Language identifier
        "fenced_code_block_delimiter" => SyntaxKind::Text, // The ``` markers

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
