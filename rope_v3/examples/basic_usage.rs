//! Basic usage example for stoat_rope_v3

use stoat_rope_v3::{SyntaxKind, SyntaxTree};

fn main() {
    // Create a syntax tree from some code
    let code = r#"
    fn hello_world() {
        println!("Hello, world!");
        let x = 42;
    }
    "#;

    let tree = SyntaxTree::from_text(code);

    // Print basic statistics
    println!("=== Syntax Tree Statistics ===");
    println!("Total tokens: {}", tree.token_count());
    println!("Total bytes: {}", tree.byte_count());
    println!("Total lines: {}", tree.line_count());

    // Get summary information
    let summary = tree.summary();
    println!("\n=== Token Types Present ===");
    for kind in &summary.kinds {
        println!("  - {:?}", kind);
    }

    // Find specific tokens
    println!("\n=== Identifiers Found ===");
    let identifiers = tree.tokens_of_kind(SyntaxKind::Identifier);
    for (i, token) in identifiers.iter().enumerate() {
        println!(
            "  {}. '{}' at byte offset {}",
            i + 1,
            tree.token_text(token),
            token.range.start.offset
        );
    }

    // Find numbers
    println!("\n=== Numbers Found ===");
    let numbers = tree.tokens_of_kind(SyntaxKind::Number);
    for token in &numbers {
        println!(
            "  '{}' at byte offset {}",
            tree.token_text(token),
            token.range.start.offset
        );
    }

    // Check for errors
    let errors = tree.error_tokens();
    if errors.is_empty() {
        println!("\n[OK] No syntax errors detected");
    } else {
        println!("\n[WARNING] Found {} syntax errors", errors.len());
        for error in &errors {
            println!("  Error at byte offset {}", error.range.start.offset);
        }
    }

    // Demonstrate token navigation
    println!("\n=== Token at specific index ===");
    if let Some(token) = tree.token_at_index(5) {
        println!(
            "Token at index 5: '{}' (kind: {:?})",
            tree.token_text(&token),
            token.kind
        );
    }
}
