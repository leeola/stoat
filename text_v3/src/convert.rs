//! Convert tree-sitter AST to flat token list

use crate::language::Language;
use stoat_rope_v3::{Language as RopeLanguage, SyntaxKind, TokenEntry};
use text::{Anchor, BufferSnapshot};
use tree_sitter::{Node, Tree};

/// Convert tree-sitter tree to flat token list
pub fn tree_to_tokens(
    tree: &Tree,
    source: &str,
    buffer: &BufferSnapshot,
    language: Language,
) -> Vec<TokenEntry> {
    let mut tokens = Vec::new();
    walk_tree(tree.root_node(), source, buffer, language, &mut tokens);
    tokens
}

/// Walk tree-sitter tree and extract leaf tokens
fn walk_tree(
    node: Node,
    source: &str,
    buffer: &BufferSnapshot,
    language: Language,
    tokens: &mut Vec<TokenEntry>,
) {
    // Skip whitespace-only nodes
    if node.kind() == "whitespace" {
        return;
    }

    if node.child_count() == 0 {
        // Leaf node - create token
        create_token(node, source, buffer, language, tokens);
    } else {
        // Internal node - recurse to children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_tree(child, source, buffer, language, tokens);
        }
    }
}

fn create_token(
    node: Node,
    _source: &str,
    buffer: &BufferSnapshot,
    language: Language,
    tokens: &mut Vec<TokenEntry>,
) {
    let kind = map_node_kind(node.kind(), language);
    let start = buffer.anchor_before(node.start_byte());
    let end = buffer.anchor_after(node.end_byte());
    let rope_lang = map_language(language);

    tokens.push(TokenEntry {
        range: start..end,
        kind,
        language: Some(rope_lang),
        semantic: None,
        highlight_id: None,
    });
}

fn map_node_kind(ts_kind: &str, language: Language) -> SyntaxKind {
    match language {
        Language::Rust => map_rust_kind(ts_kind),
        Language::Markdown => map_markdown_kind(ts_kind),
        Language::PlainText => SyntaxKind::Text,
    }
}

fn map_rust_kind(ts_kind: &str) -> SyntaxKind {
    match ts_kind {
        // Keywords
        "as" | "async" | "await" | "break" | "const" | "continue" | "crate" | "dyn" | "else"
        | "enum" | "extern" | "fn" | "for" | "if" | "impl" | "in" | "let" | "loop" | "match"
        | "mod" | "move" | "mut" | "pub" | "ref" | "return" | "self" | "Self" | "static"
        | "struct" | "super" | "trait" | "type" | "union" | "unsafe" | "use" | "where"
        | "while" => SyntaxKind::Keyword,

        // Literals
        "identifier" => SyntaxKind::Identifier,
        "integer_literal" | "float_literal" => SyntaxKind::Number,
        "string_literal" | "raw_string_literal" => SyntaxKind::String,
        "char_literal" => SyntaxKind::Char,
        "boolean_literal" | "true" | "false" => SyntaxKind::Boolean,

        // Comments
        "line_comment" => SyntaxKind::LineComment,
        "block_comment" => SyntaxKind::BlockComment,

        // Punctuation
        "(" => SyntaxKind::OpenParen,
        ")" => SyntaxKind::CloseParen,
        "[" => SyntaxKind::OpenBracket,
        "]" => SyntaxKind::CloseBracket,
        "{" => SyntaxKind::OpenBrace,
        "}" => SyntaxKind::CloseBrace,
        "," => SyntaxKind::Comma,
        ";" => SyntaxKind::Semicolon,
        ":" => SyntaxKind::Colon,
        "." => SyntaxKind::Dot,
        "->" => SyntaxKind::Arrow,

        // Operators
        "+" | "-" | "*" | "/" | "%" | "=" | "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||"
        | "!" | "&" | "|" | "^" | "<<" | ">>" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|="
        | "^=" | "<<=" | ">>=" => SyntaxKind::Operator,

        _ => SyntaxKind::Unknown,
    }
}

fn map_markdown_kind(ts_kind: &str) -> SyntaxKind {
    match ts_kind {
        "atx_heading" | "setext_heading" => SyntaxKind::Heading,
        "paragraph" => SyntaxKind::Paragraph,
        "code_span" => SyntaxKind::CodeSpan,
        "fenced_code_block" => SyntaxKind::CodeBlock,
        "emphasis" => SyntaxKind::Emphasis,
        "strong_emphasis" => SyntaxKind::Strong,
        "line_break" | "soft_line_break" | "hard_line_break" => SyntaxKind::Newline,
        _ => SyntaxKind::Text,
    }
}

fn map_language(language: Language) -> RopeLanguage {
    match language {
        Language::Rust => RopeLanguage::Rust,
        Language::Markdown => RopeLanguage::Markdown,
        Language::PlainText => RopeLanguage::Unknown,
    }
}

/// Simple plain text tokenization (no tree-sitter)
pub fn tokenize_plain_text(text: &str, buffer: &BufferSnapshot) -> Vec<TokenEntry> {
    if text.is_empty() {
        return Vec::new();
    }

    vec![TokenEntry {
        range: buffer.anchor_before(0)..buffer.anchor_after(text.len()),
        kind: SyntaxKind::Text,
        language: Some(RopeLanguage::Unknown),
        semantic: None,
        highlight_id: None,
    }]
}
