//! Parser wrapper for tree-sitter

use crate::{convert, language::Language};
use stoat_rope::TokenEntry;
use text::BufferSnapshot;
use tree_sitter::Parser as TsParser;

/// Parser that wraps tree-sitter and produces tokens
pub struct Parser {
    language: Language,
    ts_parser: Option<TsParser>,
}

impl Clone for Parser {
    fn clone(&self) -> Self {
        // Recreate parser since tree-sitter Parser doesn't implement Clone
        Self::new(self.language).expect("Failed to clone parser")
    }
}

impl Parser {
    /// Create a new parser for the given language
    pub fn new(language: Language) -> anyhow::Result<Self> {
        let ts_parser = match language {
            Language::Rust => {
                let mut parser = TsParser::new();
                parser
                    .set_language(tree_sitter_rust::language())
                    .map_err(|e| anyhow::anyhow!("Failed to set Rust language: {e}"))?;
                Some(parser)
            },
            Language::Markdown => {
                let mut parser = TsParser::new();
                parser
                    .set_language(tree_sitter_md::language())
                    .map_err(|e| anyhow::anyhow!("Failed to set Markdown language: {e}"))?;
                Some(parser)
            },
            Language::PlainText => None, // No tree-sitter for plain text
        };

        Ok(Self {
            language,
            ts_parser,
        })
    }

    /// Parse text into tokens
    pub fn parse(
        &mut self,
        text: &str,
        buffer: &BufferSnapshot,
    ) -> anyhow::Result<Vec<TokenEntry>> {
        match self.language {
            Language::PlainText => {
                // Simple tokenization for plain text
                Ok(convert::tokenize_plain_text(text, buffer))
            },
            Language::Rust | Language::Markdown => {
                let tree = self
                    .ts_parser
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("No parser initialized"))?
                    .parse(text, None)
                    .ok_or_else(|| anyhow::anyhow!("Tree-sitter parse failed"))?;

                Ok(convert::tree_to_tokens(&tree, text, buffer, self.language))
            },
        }
    }

    /// Get the language this parser is configured for
    pub fn language(&self) -> Language {
        self.language
    }
}
