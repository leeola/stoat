//! Tests for parsing markdown with code blocks and language detection

use crate::parser::{Language, Parser};

#[test]
fn test_markdown_with_text_code_block() {
    let markdown = r#"# Test Heading

Some regular markdown text here.

```text
This is plain text in a code block
It should have Language::PlainText
```

More markdown after the code block."#;

    let mut parser = Parser::from_language(Language::Markdown).unwrap();
    let rope = parser.parse_text(markdown).unwrap();

    // Debug print the tree structure
    println!("=== AST Structure ===");
    debug_print_node(&rope.root(), 0);

    // Let's also test with tree-sitter directly to see what nodes it produces
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser.set_language(tree_sitter_md::language()).unwrap();
    let tree = ts_parser.parse(markdown, None).unwrap();

    println!("\n=== Tree-sitter nodes ===");
    debug_print_ts_node(&tree.root_node(), markdown, 0);

    // Verify the structure has a CodeBlock with PlainText language
    let found_code_block = find_code_block_with_language(&rope.root());
    assert!(
        found_code_block,
        "Should find a code block with PlainText language"
    );
}

#[test]
fn test_markdown_with_multiple_language_code_blocks() {
    let markdown = r#"# Multiple Languages

Here's some Rust code:

```rust
fn main() {
    println!("Hello, world!");
}
```

And some markdown:

```markdown
# This is markdown
**Bold** and *italic*
```

And plain text:

```text
Just plain text here
```
"#;

    let mut parser = Parser::from_language(Language::Markdown).unwrap();
    let rope = parser.parse_text(markdown).unwrap();

    // Count languages found
    let mut found_languages = std::collections::HashSet::new();
    collect_languages(&rope.root(), &mut found_languages);

    assert!(
        found_languages.contains(&Some(stoat_rope::Language::Rust)),
        "Should find Rust language"
    );
    assert!(
        found_languages.contains(&Some(stoat_rope::Language::Markdown)),
        "Should find Markdown language"
    );
    assert!(
        found_languages.contains(&Some(stoat_rope::Language::PlainText)),
        "Should find PlainText language"
    );
}

fn collect_languages(
    node: &stoat_rope::ast::AstNode,
    languages: &mut std::collections::HashSet<Option<stoat_rope::Language>>,
) {
    match node {
        stoat_rope::ast::AstNode::Token { language, .. } => {
            languages.insert(*language);
        },
        stoat_rope::ast::AstNode::Syntax {
            children, language, ..
        } => {
            languages.insert(*language);
            for (child, _) in children {
                collect_languages(child, languages);
            }
        },
    }
}

fn debug_print_node(node: &stoat_rope::ast::AstNode, indent: usize) {
    let indent_str = "  ".repeat(indent);

    match node {
        stoat_rope::ast::AstNode::Token {
            kind,
            text,
            language,
            ..
        } => {
            let lang_str = language
                .map(|l| format!(" [{}]", l.name()))
                .unwrap_or_default();
            println!("{}{:?}: {:?}{}", indent_str, kind, text, lang_str);
        },
        stoat_rope::ast::AstNode::Syntax {
            kind,
            children,
            language,
            ..
        } => {
            let lang_str = language
                .map(|l| format!(" [{}]", l.name()))
                .unwrap_or_default();
            println!("{}{:?}{}", indent_str, kind, lang_str);
            for (child, _) in children {
                debug_print_node(child, indent + 1);
            }
        },
    }
}

fn find_code_block_with_language(node: &stoat_rope::ast::AstNode) -> bool {
    use stoat_rope::kind::SyntaxKind;

    match node {
        stoat_rope::ast::AstNode::Token { language, .. } => {
            // Check if this token has PlainText language
            language.map_or(false, |l| l == stoat_rope::Language::PlainText)
        },
        stoat_rope::ast::AstNode::Syntax { kind, children, .. } => {
            // Check if this is a code block
            if *kind == SyntaxKind::CodeBlock {
                // Check if any child has PlainText language
                for (child, _) in children {
                    if find_code_block_with_language(child) {
                        return true;
                    }
                }
            }
            // Recursively check children
            for (child, _) in children {
                if find_code_block_with_language(child) {
                    return true;
                }
            }
            false
        },
    }
}

fn debug_print_ts_node(node: &tree_sitter::Node<'_>, source: &str, indent: usize) {
    let indent_str = "  ".repeat(indent);

    let text = if node.child_count() == 0 {
        node.utf8_text(source.as_bytes())
            .unwrap_or("<invalid>")
            .chars()
            .take(50)
            .collect::<String>()
    } else {
        String::new()
    };

    if !text.is_empty() {
        println!("{}{}: {:?}", indent_str, node.kind(), text);
    } else {
        println!("{}{}", indent_str, node.kind());
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        debug_print_ts_node(&child, source, indent + 1);
    }
}
