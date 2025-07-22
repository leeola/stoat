//! Tree-sitter based parsing for converting source text to rope AST
//!
//! This module provides language-agnostic parsing using tree-sitter grammars.
//! The parser converts tree-sitter parse trees into rope AST structures.

pub use plain_text::parse_plain_text;
use snafu::Snafu;
use std::sync::Arc;
use stoat_rope::RopeAst;
use tree_sitter::Parser as TsParser;

mod convert;
mod plain_text;

#[cfg(test)]
mod tests;

/// Supported languages for parsing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// Plain text
    PlainText,
    /// Markdown language
    Markdown,
}

/// Parser errors
#[derive(Debug, Snafu)]
pub enum ParseError {
    #[snafu(display("Tree-sitter parsing failed"))]
    TreeSitterError,

    #[snafu(display("Language not supported: {:?}", language))]
    UnsupportedLanguage { language: Language },

    #[snafu(display("Conversion error: {}", message))]
    ConversionError { message: String },
}

/// Language-specific parser using tree-sitter or manual parsing
pub struct Parser {
    /// The language this parser is configured for
    language: Language,

    /// Tree-sitter parser instance (only for languages that use tree-sitter)
    ts_parser: Option<TsParser>,
}

impl Parser {
    /// Create a parser for the specified language
    pub fn from_language(language: Language) -> Result<Self, ParseError> {
        let ts_parser = match language {
            Language::Markdown => {
                let mut parser = TsParser::new();
                parser
                    .set_language(tree_sitter_md::language())
                    .map_err(|_| ParseError::TreeSitterError)?;
                Some(parser)
            },
            Language::PlainText => None, // Plain text uses manual parsing
        };

        Ok(Self {
            language,
            ts_parser,
        })
    }

    /// Parse text into a rope AST
    pub fn parse_text(&mut self, text: &str) -> Result<Arc<RopeAst>, ParseError> {
        match self.language {
            Language::PlainText => {
                // Use manual plain text parsing
                parse_plain_text(text)
            },
            Language::Markdown => {
                // Use tree-sitter parsing
                let tree = self
                    .ts_parser
                    .as_mut()
                    .ok_or(ParseError::TreeSitterError)?
                    .parse(text, None)
                    .ok_or(ParseError::TreeSitterError)?;

                // Convert tree-sitter tree to rope AST
                let root_node = convert::convert_tree(&tree, text, self.language)?;

                // Create RopeAst from the root node
                Ok(Arc::new(RopeAst::from_root(root_node)))
            },
        }
    }

    /// Get the language this parser is configured for
    pub fn language(&self) -> Language {
        self.language
    }
}

impl Language {
    /// Get the file extensions associated with this language
    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            Language::Markdown => &["md", "markdown", "mdown", "mkdn", "mkd"],
            Language::PlainText => &["txt", "text"],
        }
    }

    /// Get the human-readable name of the language
    pub fn name(&self) -> &'static str {
        match self {
            Language::Markdown => "Markdown",
            Language::PlainText => "Plain Text",
        }
    }
}
