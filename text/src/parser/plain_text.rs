//! Plain text parser for creating structured AST from unformatted text
//!
//! This module provides a manual tokenizer for plain text content that doesn't belong
//! to a specific programming language. It creates a structured AST with proper tokenization
//! while preserving all whitespace and content exactly.

use crate::parser::ParseError;
use std::sync::Arc;
use stoat_rope::{
    RopeAst,
    ast::{AstNode, TextRange},
    builder::AstBuilder,
    kind::SyntaxKind,
    word_parser::{IdentifierPart, parse_identifier_parts},
};

/// Parse plain text content into a structured rope AST
///
/// This function tokenizes plain text content using word boundaries and creates
/// a proper AST structure with Document to Paragraph to tokens hierarchy.
/// All content is preserved exactly for lossless round-trip conversion.
///
/// The parser uses these generic token types:
/// - [`SyntaxKind::Text`] - Regular words and content
/// - [`SyntaxKind::Number`] - Numeric sequences
/// - [`SyntaxKind::Whitespace`] - Spaces and tabs
/// - [`SyntaxKind::Newline`] - Line breaks
///
/// Paragraphs are separated by double newlines (blank lines).
pub fn parse_plain_text(content: &str) -> Result<Arc<RopeAst>, ParseError> {
    if content.is_empty() {
        return Ok(Arc::new(create_empty_document()));
    }

    let tokens = tokenize_text(content);
    let paragraphs = group_into_paragraphs(tokens);
    let document = build_document(paragraphs, content.len());

    Ok(Arc::new(RopeAst::from_root(document)))
}

/// Token representation during parsing
#[derive(Debug, Clone)]
struct Token {
    kind: SyntaxKind,
    text: String,
    range: TextRange,
}

/// Tokenize text content into a sequence of tokens
fn tokenize_text(content: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = content.char_indices().peekable();

    while let Some((start_idx, ch)) = chars.next() {
        match ch {
            // Handle newlines
            '\n' => {
                let end_idx = start_idx + ch.len_utf8();
                tokens.push(Token {
                    kind: SyntaxKind::Newline,
                    text: ch.to_string(),
                    range: TextRange::new(start_idx, end_idx),
                });
            },

            // Handle whitespace (spaces, tabs)
            ch if ch.is_whitespace() => {
                let mut text = String::new();
                text.push(ch);
                let mut end_idx = start_idx + ch.len_utf8();

                // Collect consecutive whitespace (but not newlines)
                while let Some((_idx, next_ch)) = chars.peek() {
                    if next_ch.is_whitespace() && *next_ch != '\n' {
                        let (new_idx, next_ch) =
                            chars.next().expect("Iterator peeked successfully");
                        text.push(next_ch);
                        end_idx = new_idx + next_ch.len_utf8();
                    } else {
                        break;
                    }
                }

                tokens.push(Token {
                    kind: SyntaxKind::Whitespace,
                    text,
                    range: TextRange::new(start_idx, end_idx),
                });
            },

            // Handle numeric content
            ch if ch.is_ascii_digit() => {
                let mut text = String::new();
                text.push(ch);
                let mut end_idx = start_idx + ch.len_utf8();

                // Collect consecutive digits and decimal points
                while let Some((_idx, next_ch)) = chars.peek() {
                    if next_ch.is_ascii_digit() || *next_ch == '.' {
                        let (new_idx, next_ch) =
                            chars.next().expect("Iterator peeked successfully");
                        text.push(next_ch);
                        end_idx = new_idx + next_ch.len_utf8();
                    } else {
                        break;
                    }
                }

                tokens.push(Token {
                    kind: SyntaxKind::Number,
                    text,
                    range: TextRange::new(start_idx, end_idx),
                });
            },

            // Handle text content (words, punctuation, everything else)
            _ => {
                let mut text = String::new();
                text.push(ch);
                let mut end_idx = start_idx + ch.len_utf8();

                // Collect consecutive non-whitespace, non-digit characters
                while let Some((_idx, next_ch)) = chars.peek() {
                    if !next_ch.is_whitespace() && !next_ch.is_ascii_digit() {
                        let (new_idx, next_ch) =
                            chars.next().expect("Iterator peeked successfully");
                        text.push(next_ch);
                        end_idx = new_idx + next_ch.len_utf8();
                    } else {
                        break;
                    }
                }

                tokens.push(Token {
                    kind: SyntaxKind::Text,
                    text,
                    range: TextRange::new(start_idx, end_idx),
                });
            },
        }
    }

    tokens
}

/// Group tokens into paragraphs based on double newlines
fn group_into_paragraphs(tokens: Vec<Token>) -> Vec<Vec<Token>> {
    let mut paragraphs = Vec::new();
    let mut current_paragraph = Vec::new();
    let mut consecutive_newlines = 0;

    for token in tokens {
        match token.kind {
            SyntaxKind::Newline => {
                consecutive_newlines += 1;
                current_paragraph.push(token);

                // Double newline indicates paragraph break
                if consecutive_newlines >= 2 {
                    if !current_paragraph.is_empty() {
                        paragraphs.push(current_paragraph);
                        current_paragraph = Vec::new();
                    }
                    consecutive_newlines = 0;
                }
            },
            _ => {
                consecutive_newlines = 0;
                current_paragraph.push(token);
            },
        }
    }

    // Add final paragraph if it has content
    if !current_paragraph.is_empty() {
        paragraphs.push(current_paragraph);
    }

    // The tokens were already consumed by the for loop above
    // If no paragraphs were created but we had tokens, they would have been
    // added to current_paragraph, which was already pushed

    paragraphs
}

/// Build a token node, potentially parsing identifiers into Word sub-nodes
fn build_token_node(token: Token) -> Arc<AstNode> {
    // Check if this looks like an identifier (alphanumeric with possible underscores/dashes)
    if token.kind == SyntaxKind::Text && looks_like_identifier(&token.text) {
        let parts = parse_identifier_parts(&token.text);

        // If we found multiple parts (words and separators), create a Syntax node
        if parts.len() > 1 {
            let part_nodes: Vec<Arc<AstNode>> = parts
                .into_iter()
                .map(|part| {
                    match part {
                        IdentifierPart::Word(text, range) => {
                            // Adjust range to be absolute (relative to document start)
                            let abs_range = TextRange::new(
                                token.range.start.0 + range.start,
                                token.range.start.0 + range.end,
                            );
                            AstBuilder::token(SyntaxKind::Word, &text, abs_range)
                        },
                        IdentifierPart::Separator(text, range) => {
                            // Adjust range to be absolute (relative to document start)
                            let abs_range = TextRange::new(
                                token.range.start.0 + range.start,
                                token.range.start.0 + range.end,
                            );
                            AstBuilder::token(SyntaxKind::Separator, &text, abs_range)
                        },
                    }
                })
                .collect();

            // Create an Identifier syntax node containing Word and Separator tokens
            return AstBuilder::start_node(SyntaxKind::Identifier, token.range)
                .add_children(part_nodes)
                .finish();
        }

        // Single word identifier stays as a simple token
        if parts.len() == 1 {
            return AstBuilder::token(SyntaxKind::Identifier, &token.text, token.range);
        }
    }

    // Default: create token as-is
    AstBuilder::token(token.kind, &token.text, token.range)
}

/// Check if text looks like an identifier (variable name, function name, etc.)
fn looks_like_identifier(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    // Check if it starts with a letter or underscore
    let first_char = text.chars().next().unwrap();
    if !first_char.is_ascii_alphabetic() && first_char != '_' {
        return false;
    }

    // Check if all characters are alphanumeric, underscore, or dash
    text.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Build the document AST from paragraphs
fn build_document(paragraphs: Vec<Vec<Token>>, total_len: usize) -> Arc<AstNode> {
    let document_range = TextRange::new(0, total_len);

    if paragraphs.is_empty() {
        return AstBuilder::start_node(SyntaxKind::Document, document_range).finish();
    }

    let mut paragraph_nodes: Vec<Arc<AstNode>> = Vec::new();

    for paragraph_tokens in paragraphs {
        if paragraph_tokens.is_empty() {
            continue;
        }

        // Calculate paragraph range
        let start = paragraph_tokens
            .first()
            .expect("Paragraph tokens are not empty")
            .range
            .start
            .0;
        let end = paragraph_tokens
            .last()
            .expect("Paragraph tokens are not empty")
            .range
            .end
            .0;
        let paragraph_range = TextRange::new(start, end);

        // Build tokens for this paragraph, parsing identifiers into words
        let token_nodes: Vec<Arc<AstNode>> = paragraph_tokens
            .into_iter()
            .map(|token| build_token_node(token))
            .collect();

        // Create paragraph node
        let paragraph = AstBuilder::start_node(SyntaxKind::Paragraph, paragraph_range)
            .add_children(token_nodes)
            .finish();

        paragraph_nodes.push(paragraph);
    }

    // Create document with paragraphs
    AstBuilder::start_node(SyntaxKind::Document, document_range)
        .add_children(paragraph_nodes)
        .finish()
}

/// Create an empty document structure for empty content
fn create_empty_document() -> RopeAst {
    let document = AstBuilder::start_node(SyntaxKind::Document, TextRange::new(0, 0)).finish();
    RopeAst::from_root(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_round_trip(content: &str) {
        let ast = parse_plain_text(content).expect("Parse should succeed");
        assert_eq!(
            ast.to_string(),
            content,
            "Round-trip failed for: {content:?}"
        );
    }

    #[test]
    fn empty_content() {
        let ast = parse_plain_text("").expect("Parse should succeed");
        assert_eq!(ast.to_string(), "");
    }

    #[test]
    fn simple_text() {
        assert_round_trip("hello world");
        assert_round_trip("hello");
        assert_round_trip("a");
    }

    #[test]
    fn identifier_word_parsing() {
        // Test camelCase identifier parsing
        let ast = parse_plain_text("fooBarBaz").expect("Parse should succeed");
        let root = ast.root();

        // Navigate to the identifier
        let doc_children = root.children().expect("Document should have children");
        let para = &doc_children[0].0;
        let para_children = para.children().expect("Paragraph should have children");
        let identifier = &para_children[0].0;

        // Verify it's an Identifier syntax node with Word children
        assert_eq!(identifier.kind(), SyntaxKind::Identifier);
        let word_children = identifier
            .children()
            .expect("Identifier should have word children");
        assert_eq!(word_children.len(), 3);

        // Check each word
        assert_eq!(word_children[0].0.kind(), SyntaxKind::Word);
        assert_eq!(word_children[0].0.token_text(), Some("foo"));

        assert_eq!(word_children[1].0.kind(), SyntaxKind::Word);
        assert_eq!(word_children[1].0.token_text(), Some("Bar"));

        assert_eq!(word_children[2].0.kind(), SyntaxKind::Word);
        assert_eq!(word_children[2].0.token_text(), Some("Baz"));

        // Verify round-trip still works
        assert_eq!(ast.to_string(), "fooBarBaz");
    }

    #[test]
    fn snake_case_identifier() {
        let ast = parse_plain_text("foo_bar_baz").expect("Parse should succeed");
        let root = ast.root();

        // Navigate to the identifier
        let doc_children = root.children().expect("Document should have children");
        let para = &doc_children[0].0;
        let para_children = para.children().expect("Paragraph should have children");
        let identifier = &para_children[0].0;

        // Verify it's an Identifier with Word and Separator children
        assert_eq!(identifier.kind(), SyntaxKind::Identifier);
        let children = identifier
            .children()
            .expect("Identifier should have children");
        assert_eq!(children.len(), 5); // "foo", "_", "bar", "_", "baz"

        // Check the parts
        assert_eq!(children[0].0.kind(), SyntaxKind::Word);
        assert_eq!(children[0].0.token_text(), Some("foo"));

        assert_eq!(children[1].0.kind(), SyntaxKind::Separator);
        assert_eq!(children[1].0.token_text(), Some("_"));

        assert_eq!(children[2].0.kind(), SyntaxKind::Word);
        assert_eq!(children[2].0.token_text(), Some("bar"));

        assert_eq!(children[3].0.kind(), SyntaxKind::Separator);
        assert_eq!(children[3].0.token_text(), Some("_"));

        assert_eq!(children[4].0.kind(), SyntaxKind::Word);
        assert_eq!(children[4].0.token_text(), Some("baz"));

        // Verify round-trip works
        assert_eq!(ast.to_string(), "foo_bar_baz");
    }

    #[test]
    fn single_word_identifier() {
        let ast = parse_plain_text("hello").expect("Parse should succeed");
        let root = ast.root();

        // Navigate to the token
        let doc_children = root.children().expect("Document should have children");
        let para = &doc_children[0].0;
        let para_children = para.children().expect("Paragraph should have children");
        let identifier = &para_children[0].0;

        // Single word should be a Token, not a Syntax node
        assert_eq!(identifier.kind(), SyntaxKind::Identifier);
        assert!(identifier.is_token());
        assert_eq!(identifier.token_text(), Some("hello"));
    }

    #[test]
    fn mixed_content_with_identifiers() {
        let content = "The fooBarBaz function calls XMLHttpRequest";
        let ast = parse_plain_text(content).expect("Parse should succeed");

        // Verify round-trip
        assert_eq!(ast.to_string(), content);

        // Check that we have multiple tokens including identifiers
        let root = ast.root();
        let doc_children = root.children().expect("Document should have children");
        let para = &doc_children[0].0;
        let para_children = para.children().expect("Paragraph should have children");

        // Should have: "The", " ", "fooBarBaz", " ", "function", " ", "calls", " ",
        // "XMLHttpRequest"
        assert!(para_children.len() >= 9);

        // Find and check fooBarBaz
        let foo_bar_baz = para_children
            .iter()
            .find(|(node, _)| node.kind() == SyntaxKind::Identifier && node.children().is_some())
            .expect("Should find multi-word identifier");

        let words = foo_bar_baz.0.children().expect("Should have word children");
        assert_eq!(words.len(), 3);
    }

    #[test]
    fn whitespace_preservation() {
        assert_round_trip("hello  world");
        assert_round_trip("  hello world  ");
        assert_round_trip("\thello\tworld\t");
    }

    #[test]
    fn newlines() {
        assert_round_trip("hello\nworld");
        assert_round_trip("line1\nline2\nline3");
        assert_round_trip("\nhello\n");
    }

    #[test]
    fn paragraphs() {
        assert_round_trip("paragraph1\n\nparagraph2");
        assert_round_trip("para1\n\n\npara2");
        assert_round_trip("first\n\nsecond\n\nthird");
    }

    #[test]
    fn numbers() {
        assert_round_trip("hello 123 world");
        assert_round_trip("3.14159");
        assert_round_trip("0");
        assert_round_trip("version 1.2.3");
    }

    #[test]
    fn mixed_content() {
        assert_round_trip("The answer is 42, not 24.");
        assert_round_trip("Hello, world!\nHow are you?");
        assert_round_trip("Line 1\n\nParagraph 2 has 100% content.");
    }

    #[test]
    fn unicode() {
        assert_round_trip("Hello world");
        assert_round_trip("unicode test");
        assert_round_trip("cafe naive resume");
    }
}
