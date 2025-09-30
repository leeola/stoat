//! Tree-sitter based text parsing for rope_v3
//!
//! This crate provides tree-sitter parsers that produce flat token lists
//! compatible with [`stoat_rope_v3::TokenEntry`]. It converts tree-sitter
//! parse trees directly into tokens without intermediate tree structures.

pub mod convert;
pub mod language;
pub mod parser;

pub use language::Language;
pub use parser::Parser;
pub use stoat_rope_v3::TokenEntry;

/// Parse text into tokens using tree-sitter
///
/// Convenience function that creates a parser and parses in one call.
pub fn parse(
    text: &str,
    language: Language,
    buffer: &text::BufferSnapshot,
) -> anyhow::Result<Vec<TokenEntry>> {
    let mut parser = Parser::new(language)?;
    parser.parse(text, buffer)
}
