//! Markdown structure tests
//!
//! Tests that verify markdown parsing creates correct AST structure.
//! These tests focus on semantic correctness rather than exact text preservation.

use crate::parser::{Language, Parser};
use stoat_rope::kind::SyntaxKind;

/// Helper to parse markdown and return the rope AST
fn parse_markdown(input: &str) -> std::sync::Arc<stoat_rope::RopeAst> {
    let mut parser =
        Parser::from_language(Language::Markdown).expect("Failed to create markdown parser");

    parser.parse_text(input).expect("Failed to parse markdown")
}

#[test]
fn test_parse_empty() {
    let ast = parse_markdown("");
    assert_eq!(ast.root().kind(), SyntaxKind::Document);
    assert_eq!(ast.len_bytes(), 0);
}

#[test]
fn test_parse_simple_text() {
    let ast = parse_markdown("Hello, world!");
    assert_eq!(ast.root().kind(), SyntaxKind::Document);
    assert!(ast.len_bytes() > 0);

    // Should contain the text
    let text = ast.to_string();
    assert!(text.contains("Hello, world!"));
}

#[test]
fn test_parse_heading() {
    // Note: tree-sitter-md requires newlines after headings to parse them correctly
    let ast = parse_markdown("# Title\n");
    assert_eq!(ast.root().kind(), SyntaxKind::Document);

    // Should contain a heading node
    let has_heading = ast
        .iter_nodes()
        .any(|(node, _)| node.kind() == SyntaxKind::Heading);
    assert!(has_heading, "AST should contain a heading node");

    // Should contain the title text
    let text = ast.to_string();
    assert!(text.contains("Title"));
}

#[test]
fn test_parse_multiple_headings() {
    let ast = parse_markdown("# H1\n## H2\n### H3");

    // Count heading nodes
    let heading_count = ast
        .iter_nodes()
        .filter(|(node, _)| node.kind() == SyntaxKind::Heading)
        .count();
    assert_eq!(heading_count, 3, "Should have 3 heading nodes");
}

#[test]
fn test_parse_paragraph() {
    let ast = parse_markdown("This is a paragraph.");

    // Should contain a paragraph node
    let has_paragraph = ast
        .iter_nodes()
        .any(|(node, _)| node.kind() == SyntaxKind::Paragraph);
    assert!(has_paragraph, "AST should contain a paragraph node");
}

#[test]
fn test_parse_formatting() {
    // Note: tree-sitter-md doesn't create emphasis/strong nodes,
    // it just returns individual asterisk tokens within inline nodes.
    // This test now checks that the content is preserved rather than
    // checking for specific formatting nodes.

    // Test italic
    let ast = parse_markdown("*italic*");
    let text = ast.to_string();
    assert!(text.contains("*italic*"), "Should preserve italic markers");

    // Test bold
    let ast = parse_markdown("**bold**");
    let text = ast.to_string();
    assert!(text.contains("**bold**"), "Should preserve bold markers");

    // Test code
    let ast = parse_markdown("`code`");
    let text = ast.to_string();
    assert!(text.contains("`code`"), "Should preserve code markers");
}

#[test]
fn test_document_structure() {
    let input = r#"# Title

First paragraph.

## Section

Second paragraph."#;

    let ast = parse_markdown(input);

    // Verify document structure
    assert_eq!(ast.root().kind(), SyntaxKind::Document);

    // Count different node types
    let mut headings = 0;
    let mut paragraphs = 0;
    let mut blocks = 0;

    for (node, _) in ast.iter_nodes() {
        match node.kind() {
            SyntaxKind::Heading => headings += 1,
            SyntaxKind::Paragraph => paragraphs += 1,
            SyntaxKind::Block => blocks += 1,
            _ => {},
        }
    }

    assert_eq!(headings, 2, "Should have 2 headings");
    assert_eq!(paragraphs, 2, "Should have 2 paragraphs");
    assert!(blocks > 0, "Should have block nodes");
}

#[test]
fn test_preserves_content() {
    // Test that all text content is preserved, even if formatting isn't exact
    let inputs = vec![
        "Simple text",
        "Text with *formatting*",
        "# Heading\nParagraph",
        "Multiple\n\nParagraphs",
    ];

    for input in inputs {
        let ast = parse_markdown(input);
        let output = ast.to_string();

        // Check that key content is preserved (not necessarily exact format)
        // Remove markdown syntax chars for comparison
        let clean_input = input.replace("#", "").replace("*", "");
        let input_words: Vec<&str> = clean_input.split_whitespace().collect();

        // All content words should appear somewhere in output
        for word in &input_words {
            assert!(
                output.contains(word),
                "Content word '{word}' missing from output for input: {input}"
            );
        }
    }
}

#[test]
fn test_line_preservation() {
    // Verify that line structure is preserved in the output
    let ast = parse_markdown("Line 1\nLine 2\nLine 3");
    let output = ast.to_string();

    // Check that all lines are present
    assert!(output.contains("Line 1"), "Should contain Line 1");
    assert!(output.contains("Line 2"), "Should contain Line 2");
    assert!(output.contains("Line 3"), "Should contain Line 3");

    // Check that newlines are preserved (text includes them)
    assert!(output.contains("\n"), "Should preserve newline characters");
}
