//! Language parsing for rope AST construction
//!
//! Provides the [`Parse`] type that defines how to parse different programming languages
//! into the rope AST structure. This enables syntax-aware editing and allows the editor
//! to understand code structure across different languages.

use crate::parser::{Language, ParseError, Parser};
use std::sync::Arc;
use stoat_rope::RopeAst;

/// Language parser configuration
///
/// Parse defines how to convert raw text into a structured rope AST for a specific
/// language. Different languages will have different parsing rules, token types,
/// and syntax node structures.
#[allow(dead_code)]
pub struct Parse {
    /// The language this parser is configured for
    language: Language,

    /// Internal parser instance
    parser: Parser,
}

impl Parse {
    /// Create a parser for the specified language
    pub fn from_language(language: Language) -> Result<Self, ParseError> {
        let parser = Parser::from_language(language)?;
        Ok(Self { language, parser })
    }

    /// Parse text into a rope AST
    pub fn parse_text(&mut self, text: &str) -> Result<Arc<RopeAst>, ParseError> {
        self.parser.parse_text(text)
    }

    /// Get the language this parser is configured for
    pub fn language(&self) -> Language {
        self.language
    }
}
