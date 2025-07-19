//! Markdown syntax implementation using tree-sitter

use crate::syntax::{flat_ast::FlatAst, flat_builder::FlatTreeBuilder, kind::SyntaxKind};
use tree_sitter::Tree;

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

/// Convert tree-sitter tree to flat AST
pub fn convert_tree_to_flat_ast(tree: &Tree, text: &str) -> FlatAst {
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
pub fn map_tree_sitter_kind(ts_kind: &str) -> SyntaxKind {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_kind_properties() {
        // Test the unified SyntaxKind instead of deprecated MarkdownKind
        assert!(SyntaxKind::Text.is_token());
        assert!(!SyntaxKind::Paragraph.is_token());
        assert!(SyntaxKind::Whitespace.is_trivia());
        assert!(!SyntaxKind::Text.is_trivia());
    }

    #[test]
    fn test_markdown_parsing() {
        // Test using the unified parse function instead of deprecated Syntax::parse
        let text = "# Hello World\n\nThis is a paragraph.";
        let result = crate::syntax::parse::parse_markdown(text);

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
