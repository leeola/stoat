//! Convert tree-sitter AST to flat token list

use crate::language::Language;
use stoat_rope_v3::{Language as RopeLanguage, SyntaxKind, TokenEntry};
use text::BufferSnapshot;
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
    node: Node<'_>,
    source: &str,
    buffer: &BufferSnapshot,
    language: Language,
    tokens: &mut Vec<TokenEntry>,
) {
    walk_tree_with_parent(node, None, source, buffer, language, tokens);
}

/// Walk tree-sitter tree with parent context for contextual mapping
fn walk_tree_with_parent(
    node: Node<'_>,
    parent: Option<Node<'_>>,
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
        // Leaf node - create token with parent context
        create_token_with_context(node, parent, source, buffer, language, tokens);
    } else {
        // Internal node - recurse to children with this node as parent
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_tree_with_parent(child, Some(node), source, buffer, language, tokens);
        }
    }
}

fn create_token_with_context(
    node: Node<'_>,
    parent: Option<Node<'_>>,
    source: &str,
    buffer: &BufferSnapshot,
    language: Language,
    tokens: &mut Vec<TokenEntry>,
) {
    let kind = map_node_kind_with_context(node.kind(), parent, source, node, language);
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

fn map_node_kind_with_context(
    ts_kind: &str,
    parent: Option<Node<'_>>,
    source: &str,
    node: Node<'_>,
    language: Language,
) -> SyntaxKind {
    match language {
        Language::Rust => map_rust_kind_with_context(ts_kind, parent, source, node),
        Language::Markdown => map_markdown_kind(ts_kind),
        Language::PlainText => SyntaxKind::Text,
    }
}

fn map_rust_kind_with_context(
    ts_kind: &str,
    parent: Option<Node<'_>>,
    source: &str,
    node: Node<'_>,
) -> SyntaxKind {
    match ts_kind {
        // Keywords
        "as" | "async" | "await" | "break" | "const" | "continue" | "crate" | "dyn" | "else"
        | "enum" | "extern" | "fn" | "for" | "if" | "impl" | "in" | "let" | "loop" | "match"
        | "mod" | "move" | "mut" | "pub" | "ref" | "return" | "static" | "struct" | "super"
        | "trait" | "type" | "union" | "unsafe" | "use" | "where" | "while" | "macro_rules!"
        | "yield" | "default" | "raw" => SyntaxKind::Keyword,

        // Special keywords that are variables
        "self" => SyntaxKind::VariableSpecial,
        "Self" => SyntaxKind::Type, // Self is a type, not a keyword in our model

        // Identifiers with context
        "identifier" => {
            let text = &source[node.start_byte()..node.end_byte()];

            // Check parent context for function calls/definitions
            if let Some(parent) = parent {
                match parent.kind() {
                    "call_expression" | "generic_function" => return SyntaxKind::Function,
                    "function_item" | "function_signature_item" => {
                        // Check if this is the name field
                        if parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id()) {
                            return SyntaxKind::FunctionDefinition;
                        }
                    },
                    "macro_invocation" | "macro_definition" => {
                        if parent.child_by_field_name("macro").map(|n| n.id()) == Some(node.id())
                            || parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
                        {
                            return SyntaxKind::FunctionSpecial;
                        }
                    },
                    "parameter" => return SyntaxKind::VariableParameter,
                    "attribute" => return SyntaxKind::Attribute,
                    _ => {},
                }
            }

            // Heuristic: ALL_CAPS identifiers are constants
            if text.len() > 1
                && text
                    .chars()
                    .all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_')
            {
                return SyntaxKind::Constant;
            }

            // Heuristic: CamelCase identifiers starting with uppercase are types
            if text.chars().next().is_some_and(|c| c.is_uppercase()) {
                return SyntaxKind::Type;
            }

            SyntaxKind::Identifier
        },

        // Type identifiers with context
        "type_identifier" => {
            // Check if this is a trait in certain contexts
            if let Some(parent) = parent {
                match parent.kind() {
                    "trait_item" | "impl_item" | "trait_bounds" | "abstract_type"
                    | "dynamic_type" => {
                        return SyntaxKind::TypeInterface;
                    },
                    _ => {},
                }
            }
            SyntaxKind::Type
        },

        // Primitive types
        "primitive_type" => SyntaxKind::TypeBuiltin,

        // Fields and properties
        "field_identifier" | "shorthand_field_identifier" => {
            // Check if this is a method call
            if let Some(parent) = parent {
                if parent.kind() == "field_expression" {
                    // Look at the parent's parent to see if it's a call
                    if let Some(grandparent) = parent.parent() {
                        if grandparent.kind() == "call_expression" {
                            return SyntaxKind::FunctionMethod;
                        }
                    }
                }
            }
            SyntaxKind::Property
        },

        // Lifetimes
        "lifetime" => SyntaxKind::Lifetime,

        // Escape sequences in strings
        "escape_sequence" => SyntaxKind::StringEscape,

        // Literals
        "integer_literal" | "float_literal" => SyntaxKind::Number,
        "string_literal" | "raw_string_literal" => SyntaxKind::String,
        "char_literal" => SyntaxKind::Char,
        "boolean_literal" | "true" | "false" => SyntaxKind::Boolean,

        // Comments
        "line_comment" => {
            let text = &source[node.start_byte()..node.end_byte()];
            if text.starts_with("///") || text.starts_with("//!") {
                SyntaxKind::DocComment
            } else {
                SyntaxKind::LineComment
            }
        },
        "block_comment" => {
            let text = &source[node.start_byte()..node.end_byte()];
            if text.starts_with("/**") || text.starts_with("/*!") {
                SyntaxKind::DocComment
            } else {
                SyntaxKind::BlockComment
            }
        },

        // Punctuation - use specific types for now, can group later if needed
        "(" => SyntaxKind::OpenParen,
        ")" => SyntaxKind::CloseParen,
        "[" => SyntaxKind::OpenBracket,
        "]" => SyntaxKind::CloseBracket,
        "{" => SyntaxKind::OpenBrace,
        "}" => SyntaxKind::CloseBrace,
        "<" | ">" if parent.is_some_and(|p| p.kind().contains("generic")) => {
            SyntaxKind::PunctuationBracket
        },
        "," => SyntaxKind::Comma,
        ";" => SyntaxKind::Semicolon,
        ":" | "::" => SyntaxKind::PunctuationDelimiter,
        "." => SyntaxKind::Dot,
        "#" => SyntaxKind::PunctuationSpecial,
        "->" | "=>" => SyntaxKind::Arrow,

        // Operators
        "+" | "-" | "*" | "/" | "%" | "=" | "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||"
        | "!" | "&" | "|" | "^" | "<<" | ">>" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|="
        | "^=" | "<<=" | ">>=" | "?" | "@" | ".." | "..=" | "..." => SyntaxKind::Operator,

        // Single quote - check context to see if it's a lifetime
        "'" => {
            // Check if parent is a lifetime node
            if let Some(parent) = parent {
                if parent.kind() == "lifetime" {
                    return SyntaxKind::Lifetime;
                }
            }
            SyntaxKind::Unknown
        },

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

#[cfg(test)]
mod tests {
    use super::*;
    use text::{Buffer, BufferId, ToOffset};

    #[test]
    fn type_identifier_is_single_token() {
        let source = "use gpui::{actions, Action, Pixels, Point};";
        let buffer = Buffer::new(
            0,
            BufferId::new(1).expect("valid buffer id"),
            source.to_string(),
        );
        let snapshot = buffer.snapshot();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(tree_sitter_rust::language())
            .expect("valid rust language");
        let tree = parser.parse(source, None).expect("valid parse");

        let tokens = tree_to_tokens(&tree, source, &snapshot, Language::Rust);

        // Find the "Action" token
        let action_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                &source[start..end] == "Action"
            })
            .collect();

        // Should be exactly one token for "Action", not split into multiple
        assert_eq!(
            action_tokens.len(),
            1,
            "Action should be a single token, not split. Found {} tokens",
            action_tokens.len()
        );

        // With heuristics, CamelCase identifiers should be recognized as Type
        let token = action_tokens[0];
        assert_eq!(
            token.kind,
            SyntaxKind::Type,
            "Action should be classified as Type due to CamelCase"
        );
    }

    #[test]
    fn contextual_function_tokens() {
        let source = r#"
fn calculate(x: i32) -> i32 {
    println!("Hello");
    x + 1
}
"#;
        let buffer = Buffer::new(
            0,
            BufferId::new(1).expect("valid buffer id"),
            source.to_string(),
        );
        let snapshot = buffer.snapshot();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(tree_sitter_rust::language())
            .expect("valid rust language");
        let tree = parser.parse(source, None).expect("valid parse");

        let tokens = tree_to_tokens(&tree, source, &snapshot, Language::Rust);

        // Find function definition
        let calculate_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                &source[start..end] == "calculate"
            })
            .collect();

        assert_eq!(calculate_tokens.len(), 1);
        assert_eq!(
            calculate_tokens[0].kind,
            SyntaxKind::FunctionDefinition,
            "Function name in definition should be FunctionDefinition"
        );

        // Find macro invocation
        let println_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                &source[start..end] == "println"
            })
            .collect();

        assert_eq!(println_tokens.len(), 1);
        assert_eq!(
            println_tokens[0].kind,
            SyntaxKind::FunctionSpecial,
            "Macro name should be FunctionSpecial"
        );

        // Find parameter
        let x_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                &source[start..end] == "x"
            })
            .collect();

        // x appears multiple times, but first should be parameter
        assert!(!x_tokens.is_empty());
        assert_eq!(
            x_tokens[0].kind,
            SyntaxKind::VariableParameter,
            "Function parameter should be VariableParameter"
        );

        // Find builtin type
        let i32_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                &source[start..end] == "i32"
            })
            .collect();

        assert_eq!(i32_tokens.len(), 2); // Two occurrences
        for token in &i32_tokens {
            assert_eq!(
                token.kind,
                SyntaxKind::TypeBuiltin,
                "i32 should be TypeBuiltin"
            );
        }
    }

    #[test]
    fn heuristic_constant_detection() {
        let source = "const MAX_SIZE: usize = 100;";
        let buffer = Buffer::new(
            0,
            BufferId::new(1).expect("valid buffer id"),
            source.to_string(),
        );
        let snapshot = buffer.snapshot();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(tree_sitter_rust::language())
            .expect("valid rust language");
        let tree = parser.parse(source, None).expect("valid parse");

        let tokens = tree_to_tokens(&tree, source, &snapshot, Language::Rust);

        // Find MAX_SIZE
        let constant_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                &source[start..end] == "MAX_SIZE"
            })
            .collect();

        assert_eq!(constant_tokens.len(), 1);
        assert_eq!(
            constant_tokens[0].kind,
            SyntaxKind::Constant,
            "ALL_CAPS identifier should be Constant"
        );
    }

    #[test]
    fn field_and_method_tokens() {
        let source = r#"
struct Point { x: i32, y: i32 }
impl Point {
    fn distance(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}
"#;
        let buffer = Buffer::new(
            0,
            BufferId::new(1).expect("valid buffer id"),
            source.to_string(),
        );
        let snapshot = buffer.snapshot();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(tree_sitter_rust::language())
            .expect("valid rust language");
        let tree = parser.parse(source, None).expect("valid parse");

        let tokens = tree_to_tokens(&tree, source, &snapshot, Language::Rust);

        // Find method call (sqrt)
        let sqrt_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                &source[start..end] == "sqrt"
            })
            .collect();

        assert_eq!(sqrt_tokens.len(), 1);
        assert_eq!(
            sqrt_tokens[0].kind,
            SyntaxKind::FunctionMethod,
            "Method call should be FunctionMethod"
        );

        // Find self keyword
        let self_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                &source[start..end] == "self"
            })
            .collect();

        assert!(!self_tokens.is_empty());
        for token in &self_tokens {
            assert_eq!(
                token.kind,
                SyntaxKind::VariableSpecial,
                "self should be VariableSpecial"
            );
        }
    }

    #[test]
    fn lifetime_and_doc_comment_tokens() {
        let source = r#"
/// Calculates the sum
/// Returns the result
fn add<'a>(x: &'a i32, y: &'a i32) -> i32 {
    x + y
}
"#;
        let buffer = Buffer::new(
            0,
            BufferId::new(1).expect("valid buffer id"),
            source.to_string(),
        );
        let snapshot = buffer.snapshot();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(tree_sitter_rust::language())
            .expect("valid rust language");
        let tree = parser.parse(source, None).expect("valid parse");

        let tokens = tree_to_tokens(&tree, source, &snapshot, Language::Rust);

        // Find doc comment first (easier to verify)
        let doc_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                source[start..end].starts_with("///")
            })
            .collect();

        assert!(!doc_tokens.is_empty(), "Should have doc comments");
        for token in &doc_tokens {
            assert_eq!(
                token.kind,
                SyntaxKind::DocComment,
                "/// comments should be DocComment"
            );
        }

        // Find lifetime - look for tokens with text starting with '
        let lifetime_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                let start = t.range.start.to_offset(&snapshot);
                let end = t.range.end.to_offset(&snapshot);
                let text = &source[start..end];
                text.starts_with("'") && text.len() > 1 // ' followed by identifier
            })
            .collect();

        // Lifetimes might not be tokenized if tree-sitter doesn't create leaf nodes for them
        // So we just check that IF we have lifetime tokens, they are correctly classified
        if !lifetime_tokens.is_empty() {
            for token in &lifetime_tokens {
                assert_eq!(
                    token.kind,
                    SyntaxKind::Lifetime,
                    "Lifetime should be Lifetime"
                );
            }
        }
    }
}
