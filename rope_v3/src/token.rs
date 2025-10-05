//! Token types for the TokenMap

use crate::{kinds::SyntaxKind, language::Language, semantic::SemanticInfo};
use rustc_hash::FxHashSet as HashSet;
use std::ops::Range;
use sum_tree::{Item, Summary};
use text::Anchor;

/// A token entry stored in the TokenMap
#[derive(Debug, Clone)]
pub struct TokenEntry {
    /// Position in the text (using Zed's anchors for stability)
    pub range: Range<Anchor>,
    /// Syntax kind of this token
    pub kind: SyntaxKind,
    /// Optional semantic information
    pub semantic: Option<SemanticInfo>,
    /// Optional language context
    pub language: Option<Language>,
    /// Highlight ID for syntax highlighting (u32 for performance)
    ///
    /// This stores a highlight identifier that can be mapped to visual styles
    /// by the GUI layer. Using u32 instead of a typed HighlightId to avoid
    /// circular dependencies between rope_v3 and stoat_gui.
    pub highlight_id: Option<u32>,
}

impl TokenEntry {
    /// Create a new token entry
    pub fn new(range: Range<Anchor>, kind: SyntaxKind) -> Self {
        Self {
            range,
            kind,
            semantic: None,
            language: None,
            highlight_id: None,
        }
    }

    /// Create a token with semantic info
    pub fn with_semantic(range: Range<Anchor>, kind: SyntaxKind, semantic: SemanticInfo) -> Self {
        Self {
            range,
            kind,
            semantic: Some(semantic),
            language: None,
            highlight_id: None,
        }
    }

    /// Create a token with highlight ID
    pub fn with_highlight(range: Range<Anchor>, kind: SyntaxKind, highlight_id: u32) -> Self {
        Self {
            range,
            kind,
            semantic: None,
            language: None,
            highlight_id: Some(highlight_id),
        }
    }

    /// Set the highlight ID for this token
    pub fn set_highlight_id(&mut self, highlight_id: u32) {
        self.highlight_id = Some(highlight_id);
    }
}

/// Summary of tokens in a subtree
#[derive(Debug, Clone, Default)]
pub struct TokenSummary {
    /// Range covered by all tokens
    pub range: Range<Anchor>,
    /// Total number of tokens
    pub token_count: usize,
    /// All syntax kinds present
    pub kinds: HashSet<SyntaxKind>,
    /// All languages present
    pub languages: HashSet<Language>,
    /// Whether any tokens have semantic info
    pub has_semantic_info: bool,
    /// Whether any error tokens are present
    pub has_errors: bool,
}

impl Item for TokenEntry {
    type Summary = TokenSummary;

    fn summary(&self, _cx: &text::BufferSnapshot) -> TokenSummary {
        let mut kinds = HashSet::default();
        kinds.insert(self.kind);

        let mut languages = HashSet::default();
        if let Some(lang) = self.language {
            languages.insert(lang);
        }

        TokenSummary {
            range: self.range.clone(),
            token_count: 1,
            kinds,
            languages,
            has_semantic_info: self.semantic.is_some(),
            has_errors: self.kind == SyntaxKind::Unknown,
        }
    }
}

impl Summary for TokenSummary {
    type Context<'a> = &'a text::BufferSnapshot;

    fn zero<'a>(_cx: Self::Context<'a>) -> Self {
        Self::default()
    }

    fn add_summary<'a>(&mut self, other: &Self, buffer: Self::Context<'a>) {
        // Update range to encompass both
        if self.range == (Anchor::MAX..Anchor::MAX) {
            self.range = other.range.clone();
        } else if other.range != (Anchor::MAX..Anchor::MAX) {
            if other.range.start.cmp(&self.range.start, buffer).is_lt() {
                self.range.start = other.range.start;
            }
            if other.range.end.cmp(&self.range.end, buffer).is_gt() {
                self.range.end = other.range.end;
            }
        }

        // Aggregate counts
        self.token_count += other.token_count;

        // Merge sets
        self.kinds.extend(&other.kinds);
        self.languages.extend(&other.languages);

        // Update flags
        self.has_semantic_info |= other.has_semantic_info;
        self.has_errors |= other.has_errors;
    }
}
