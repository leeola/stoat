//! Markdown syntax implementation using tree-sitter

use crate::{
    TextSize,
    range::TextRange,
    syntax::{
        flat_ast::FlatAst, flat_builder::FlatTreeBuilder, kind::ParseResult,
        unified_kind::SyntaxKind,
    },
};
use tree_sitter::{Parser, Tree};

/// Markdown syntax implementation
#[derive(Clone)]
pub struct MarkdownSyntax;

/// Markdown node and token kinds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownKind {
    // Document structure
    Document,
    Section,

    // Block elements
    Paragraph,
    Heading,
    CodeBlock,
    FencedCodeBlock,
    BlockQuote,
    List,
    ListItem,

    // Inline elements
    Text,
    Strong,
    Emphasis,
    Code,
    Link,
    Image,

    // Special tokens
    HeadingMarker,
    CodeFence,
    LinkText,
    LinkDestination,

    // Whitespace and punctuation
    Whitespace,
    Newline,

    // Error recovery
    Error,
}

#[allow(deprecated)]
impl crate::syntax::kind::Syntax for MarkdownSyntax {
    type Kind = MarkdownKind;

    fn parse(text: &str) -> ParseResult {
        let mut parser = Parser::new();
        let language = tree_sitter_markdown::language();
        parser
            .set_language(language)
            .expect("Error loading Markdown grammar");

        // Parse the text
        let tree = parser.parse(text, None).expect("Failed to parse markdown");

        // Convert tree-sitter tree to flat AST
        let _flat_ast = convert_tree_to_flat_ast(&tree, text);

        ParseResult {
            root: create_legacy_root(text), // FIXME: Remove this when legacy support is removed
            errors: Vec::new(),             // FIXME: Extract errors from tree-sitter
        }
    }
}

#[allow(deprecated)]
impl crate::syntax::kind::SyntaxKind for MarkdownKind {
    fn is_token(&self) -> bool {
        matches!(
            self,
            MarkdownKind::Text
                | MarkdownKind::HeadingMarker
                | MarkdownKind::CodeFence
                | MarkdownKind::LinkText
                | MarkdownKind::LinkDestination
                | MarkdownKind::Whitespace
                | MarkdownKind::Newline
        )
    }

    fn is_trivia(&self) -> bool {
        matches!(self, MarkdownKind::Whitespace | MarkdownKind::Newline)
    }

    fn name(&self) -> &'static str {
        match self {
            MarkdownKind::Document => "document",
            MarkdownKind::Section => "section",
            MarkdownKind::Paragraph => "paragraph",
            MarkdownKind::Heading => "heading",
            MarkdownKind::CodeBlock => "code_block",
            MarkdownKind::FencedCodeBlock => "fenced_code_block",
            MarkdownKind::BlockQuote => "block_quote",
            MarkdownKind::List => "list",
            MarkdownKind::ListItem => "list_item",
            MarkdownKind::Text => "text",
            MarkdownKind::Strong => "strong",
            MarkdownKind::Emphasis => "emphasis",
            MarkdownKind::Code => "code",
            MarkdownKind::Link => "link",
            MarkdownKind::Image => "image",
            MarkdownKind::HeadingMarker => "heading_marker",
            MarkdownKind::CodeFence => "code_fence",
            MarkdownKind::LinkText => "link_text",
            MarkdownKind::LinkDestination => "link_destination",
            MarkdownKind::Whitespace => "whitespace",
            MarkdownKind::Newline => "newline",
            MarkdownKind::Error => "error",
        }
    }
}

/// Convert tree-sitter tree to flat AST
fn convert_tree_to_flat_ast(tree: &Tree, text: &str) -> FlatAst {
    let mut builder = FlatTreeBuilder::new();
    let root_node = tree.root_node();

    // Start with document root
    builder.start_node(SyntaxKind::Document);

    // Convert tree-sitter nodes recursively
    convert_node_recursive(&mut builder, root_node, text.as_bytes());

    builder.finish_node();
    builder.finish()
}

/// Recursively convert tree-sitter nodes to flat AST
fn convert_node_recursive(
    builder: &mut FlatTreeBuilder,
    node: tree_sitter::Node<'_>,
    source: &[u8],
) {
    let kind = map_tree_sitter_kind(node.kind());

    if node.child_count() == 0 {
        // Leaf node - add as token
        let text = node.utf8_text(source).unwrap_or("<invalid>");
        builder.add_token(kind, text.to_string());
    } else {
        // Internal node - recurse into children
        builder.start_node(kind);

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            convert_node_recursive(builder, child, source);
        }

        builder.finish_node();
    }
}

/// Map tree-sitter node kinds to our SyntaxKind enum
fn map_tree_sitter_kind(ts_kind: &str) -> SyntaxKind {
    match ts_kind {
        "document" => SyntaxKind::Document,
        "section" => SyntaxKind::Section,
        "paragraph" => SyntaxKind::Paragraph,
        "atx_heading" => SyntaxKind::Heading,
        "setext_heading" => SyntaxKind::Heading,
        "code_block" => SyntaxKind::CodeBlock,
        "fenced_code_block" => SyntaxKind::FencedCodeBlock,
        "block_quote" => SyntaxKind::BlockQuote,
        "list" => SyntaxKind::List,
        "list_item" => SyntaxKind::ListItem,
        "strong_emphasis" => SyntaxKind::Strong,
        "emphasis" => SyntaxKind::Emphasis,
        "code_span" => SyntaxKind::Code,
        "link" => SyntaxKind::Link,
        "image" => SyntaxKind::Image,
        "atx_h1_marker" | "atx_h2_marker" | "atx_h3_marker" | "atx_h4_marker" | "atx_h5_marker"
        | "atx_h6_marker" => SyntaxKind::HeadingMarker,
        "code_fence_start" | "code_fence_end" => SyntaxKind::CodeFence,
        "link_text" => SyntaxKind::LinkText,
        "link_destination" => SyntaxKind::LinkDestination,
        "\n" => SyntaxKind::Newline,
        " " | "\t" => SyntaxKind::Whitespace,
        "ERROR" => SyntaxKind::Error,
        _ => SyntaxKind::Text, // Default to text for unknown kinds
    }
}

/// Create a legacy root node for backward compatibility
/// FIXME: Remove this when legacy support is removed
fn create_legacy_root(text: &str) -> crate::syntax::SyntaxNode {
    use crate::syntax::{SyntaxElement, SyntaxNode, SyntaxToken};
    use std::sync::Arc;

    // Create a simple root with the entire text as a single token
    let range = TextRange::new(TextSize::from(0), TextSize::from(text.len() as u32));
    let token = SyntaxToken::new(SyntaxKind::Text, range, Arc::from(text));
    let children = vec![SyntaxElement::Token(token)];

    SyntaxNode::new_with_children(SyntaxKind::Document, range, children)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(deprecated)]
    use crate::syntax::kind::{Syntax, SyntaxKind as DeprecatedSyntaxKind};

    #[test]
    fn test_markdown_kind_properties() {
        assert!(MarkdownKind::Text.is_token());
        assert!(!MarkdownKind::Paragraph.is_token());
        assert!(MarkdownKind::Whitespace.is_trivia());
        assert!(!MarkdownKind::Text.is_trivia());
    }

    #[test]
    fn test_markdown_parsing() {
        let text = "# Hello World\n\nThis is a paragraph.";
        let result = MarkdownSyntax::parse(text);

        assert_eq!(result.root.kind(), SyntaxKind::Document);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_tree_sitter_kind_mapping() {
        assert_eq!(map_tree_sitter_kind("document"), SyntaxKind::Document);
        assert_eq!(map_tree_sitter_kind("paragraph"), SyntaxKind::Paragraph);
        assert_eq!(map_tree_sitter_kind("atx_heading"), SyntaxKind::Heading);
        assert_eq!(map_tree_sitter_kind("unknown"), SyntaxKind::Text);
    }

    #[test]
    fn test_markdown_with_flat_buffer() {
        use crate::FlatTextBuffer;

        let markdown_text = "# Hello World\n\nThis is a **bold** paragraph with `code`.";
        let buffer = FlatTextBuffer::new(markdown_text);

        assert_eq!(buffer.text(), markdown_text);
        assert!(!buffer.is_empty());

        // Get the flat syntax node
        let root = buffer.flat_syntax();
        assert_eq!(root.kind(), Some(SyntaxKind::Document));
    }
}
