//! Parser wrapper for tree-sitter

use crate::{convert, language::Language};
use std::ops::Range;
use stoat_rope::TokenEntry;
use text::BufferSnapshot;
use tree_sitter::Parser as TsParser;

/// Result of an incremental parse operation
pub struct ParseResult {
    pub tokens: Vec<TokenEntry>,
    pub changed_ranges: Vec<Range<usize>>,
}

/// Parser that wraps tree-sitter and produces tokens
pub struct Parser {
    language: Language,
    ts_parser: Option<TsParser>,
    old_tree: Option<tree_sitter::Tree>,
}

impl Clone for Parser {
    fn clone(&self) -> Self {
        // Recreate parser since tree-sitter Parser doesn't implement Clone
        // Note: old_tree is not cloned - each clone starts fresh for incremental parsing
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
            Language::Json => {
                let mut parser = TsParser::new();
                parser
                    .set_language(tree_sitter_json::language())
                    .map_err(|e| anyhow::anyhow!("Failed to set JSON language: {e}"))?;
                Some(parser)
            },
            Language::Toml => {
                let mut parser = TsParser::new();
                parser
                    .set_language(tree_sitter_toml::language())
                    .map_err(|e| anyhow::anyhow!("Failed to set TOML language: {e}"))?;
                Some(parser)
            },
            Language::PlainText => None, // No tree-sitter for plain text
        };

        Ok(Self {
            language,
            ts_parser,
            old_tree: None,
        })
    }

    /// Parse text into tokens (full parse, resets incremental state)
    pub fn parse(
        &mut self,
        text: &str,
        buffer: &BufferSnapshot,
    ) -> anyhow::Result<Vec<TokenEntry>> {
        match self.language {
            Language::PlainText => {
                self.old_tree = None;
                Ok(convert::tokenize_plain_text(text, buffer))
            },
            Language::Rust | Language::Markdown | Language::Json | Language::Toml => {
                let tree = self
                    .ts_parser
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("No parser initialized"))?
                    .parse(text, None)
                    .ok_or_else(|| anyhow::anyhow!("Tree-sitter parse failed"))?;

                let tokens = convert::tree_to_tokens(&tree, text, buffer, self.language);
                self.old_tree = Some(tree);
                Ok(tokens)
            },
        }
    }

    /// Parse text incrementally using edit information
    ///
    /// Uses the stored old tree to perform incremental parsing. Call this after
    /// buffer edits for faster reparsing. Falls back to full parse if no old tree
    /// is available.
    #[allow(clippy::single_range_in_vec_init)]
    pub fn parse_incremental(
        &mut self,
        text: &str,
        buffer: &BufferSnapshot,
        edits: &[text::Edit<usize>],
    ) -> anyhow::Result<ParseResult> {
        match self.language {
            Language::PlainText => Ok(ParseResult {
                tokens: convert::tokenize_plain_text(text, buffer),
                changed_ranges: vec![0..text.len()],
            }),
            Language::Rust | Language::Markdown | Language::Json | Language::Toml => {
                for edit in edits {
                    if let Some(ref mut old_tree) = self.old_tree {
                        let input_edit = make_input_edit(edit, buffer);
                        old_tree.edit(&input_edit);
                    }
                }

                let new_tree = self
                    .ts_parser
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("No parser initialized"))?
                    .parse(text, self.old_tree.as_ref())
                    .ok_or_else(|| anyhow::anyhow!("Tree-sitter parse failed"))?;

                let changed_ranges = match &self.old_tree {
                    Some(old) => old
                        .changed_ranges(&new_tree)
                        .map(|r| r.start_byte..r.end_byte)
                        .collect(),
                    None => vec![0..text.len()],
                };

                let tokens = if changed_ranges.is_empty() || changed_ranges[0] == (0..text.len()) {
                    convert::tree_to_tokens(&new_tree, text, buffer, self.language)
                } else {
                    convert::tree_to_tokens_in_ranges(
                        &new_tree,
                        text,
                        buffer,
                        self.language,
                        &changed_ranges,
                    )
                };
                self.old_tree = Some(new_tree);

                Ok(ParseResult {
                    tokens,
                    changed_ranges,
                })
            },
        }
    }

    /// Reset the incremental parsing state
    pub fn reset(&mut self) {
        self.old_tree = None;
    }

    /// Get the language this parser is configured for
    pub fn language(&self) -> Language {
        self.language
    }
}

fn make_input_edit(edit: &text::Edit<usize>, buffer: &BufferSnapshot) -> tree_sitter::InputEdit {
    let start_point = buffer.offset_to_point(edit.new.start);
    let old_end_point = buffer.offset_to_point(edit.old.end);
    let new_end_point = buffer.offset_to_point(edit.new.end);

    tree_sitter::InputEdit {
        start_byte: edit.new.start,
        old_end_byte: edit.old.end,
        new_end_byte: edit.new.end,
        start_position: tree_sitter::Point::new(
            start_point.row as usize,
            start_point.column as usize,
        ),
        old_end_position: tree_sitter::Point::new(
            old_end_point.row as usize,
            old_end_point.column as usize,
        ),
        new_end_position: tree_sitter::Point::new(
            new_end_point.row as usize,
            new_end_point.column as usize,
        ),
    }
}
