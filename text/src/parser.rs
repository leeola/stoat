use crate::{highlight_query::HighlightQuery, language::Language};
use std::ops::Range;
use text::BufferSnapshot;
use tree_sitter::Parser as TsParser;

pub struct Parser {
    language: Language,
    ts_parser: Option<TsParser>,
    old_tree: Option<tree_sitter::Tree>,
    highlight_query: Option<HighlightQuery>,
}

impl Clone for Parser {
    fn clone(&self) -> Self {
        Self::new(self.language).expect("Failed to clone parser")
    }
}

impl Parser {
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
            Language::PlainText => None,
        };

        let highlight_query = HighlightQuery::new(language);

        Ok(Self {
            language,
            ts_parser,
            old_tree: None,
            highlight_query,
        })
    }

    /// Full parse (resets incremental state). Stores tree internally.
    pub fn parse(&mut self, text: &str) -> anyhow::Result<()> {
        match self.language {
            Language::PlainText => {
                self.old_tree = None;
                Ok(())
            },
            Language::Rust | Language::Markdown | Language::Json | Language::Toml => {
                let tree = self
                    .ts_parser
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("No parser initialized"))?
                    .parse(text, None)
                    .ok_or_else(|| anyhow::anyhow!("Tree-sitter parse failed"))?;

                self.old_tree = Some(tree);
                Ok(())
            },
        }
    }

    /// Incremental parse using edit information. Returns changed byte ranges.
    #[allow(clippy::single_range_in_vec_init)]
    pub fn parse_incremental(
        &mut self,
        text: &str,
        buffer: &BufferSnapshot,
        edits: &[text::Edit<usize>],
    ) -> anyhow::Result<Vec<Range<usize>>> {
        match self.language {
            Language::PlainText => Ok(vec![0..text.len()]),
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

                self.old_tree = Some(new_tree);
                Ok(changed_ranges)
            },
        }
    }

    /// Get the most recent parse tree, if available.
    ///
    /// Returns [`None`] for plain text (no tree-sitter grammar) or before any parse.
    pub fn tree(&self) -> Option<&tree_sitter::Tree> {
        self.old_tree.as_ref()
    }

    pub fn highlight_query(&self) -> Option<&HighlightQuery> {
        self.highlight_query.as_ref()
    }

    pub fn reset(&mut self) {
        self.old_tree = None;
    }

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
