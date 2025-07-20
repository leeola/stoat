//! Lossless parsing tests
//!
//! These tests verify that parsing preserves the exact input text.
//! They use simple inputs that work with our limited AST node types.

use crate::parser::{Language, Parser};

/// Test that parsing and converting back to string preserves the exact input
fn test_lossless(input: &str) {
    let mut parser =
        Parser::from_language(Language::Markdown).expect("Failed to create markdown parser");

    let rope_ast = parser.parse_text(input).expect("Failed to parse markdown");

    let output = rope_ast.to_string();
    assert_eq!(
        output, input,
        "Lossless parsing failed.\nInput:  {input:?}\nOutput: {output:?}"
    );
}

#[test]
fn test_lossless_empty() {
    test_lossless("");
}

#[test]
fn test_lossless_plain_text() {
    test_lossless("Hello world");
    test_lossless("Plain text");
    test_lossless("Multiple words here");
}

#[test]
fn test_lossless_whitespace() {
    test_lossless(" ");
    test_lossless("  ");
    test_lossless("\t");
    test_lossless("text with  spaces");
    test_lossless("  leading spaces");
    test_lossless("trailing spaces  ");
}

#[test]
fn test_lossless_newlines() {
    test_lossless("\n");
    test_lossless("line1\nline2");
    test_lossless("line1\n\nline2");
    test_lossless("trailing\n");
}

#[test]
fn test_lossless_markdown_heading() {
    // This is the critical test - preserving the space after #
    test_lossless("# Title");
    test_lossless("## Heading 2");
    test_lossless("#NoSpace");
    test_lossless("#  Extra spaces");
}

#[test]
fn test_lossless_markdown_formatting() {
    test_lossless("*text*");
    test_lossless("**text**");
    test_lossless("* text *"); // spaces inside
    test_lossless("text *with* formatting");
}

#[test]
fn test_lossless_special_chars() {
    test_lossless("@#$%^&*()");
    test_lossless("text: value");
    test_lossless("[brackets]");
    test_lossless("{braces}");
}

#[test]
fn test_lossless_mixed_content() {
    test_lossless("# Title\n\nParagraph text");
    test_lossless("Line 1\nLine 2\nLine 3");
    test_lossless("Text with *emphasis* and **strong**");
}
