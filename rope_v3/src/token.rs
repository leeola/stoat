//! Token implementation for the SumTree

use crate::{anchor::Anchor, kinds::SyntaxKind, language::Language, semantic::SemanticInfo};
use compact_str::CompactString;
use rustc_hash::FxHashSet as HashSet;
use std::ops::Range;
use sum_tree::{Item, Summary};

/// A token in the syntax tree
#[derive(Debug, Clone)]
pub struct Token {
    /// Position in the text (using anchors for stability)
    pub range: Range<Anchor>,
    /// The actual text content
    pub text: CompactString,
    /// Syntax kind of this token
    pub kind: SyntaxKind,
    /// Optional semantic information
    pub semantic: Option<SemanticInfo>,
    /// Optional language context
    pub language: Option<Language>,
}

impl Token {
    /// Create a new token
    pub fn new(range: Range<Anchor>, text: impl Into<CompactString>, kind: SyntaxKind) -> Self {
        Self {
            range,
            text: text.into(),
            kind,
            semantic: None,
            language: None,
        }
    }

    /// Create a token with semantic info
    pub fn with_semantic(
        range: Range<Anchor>,
        text: impl Into<CompactString>,
        kind: SyntaxKind,
        semantic: SemanticInfo,
    ) -> Self {
        Self {
            range,
            text: text.into(),
            kind,
            semantic: Some(semantic),
            language: None,
        }
    }

    /// Create a token with language
    pub fn with_language(
        range: Range<Anchor>,
        text: impl Into<CompactString>,
        kind: SyntaxKind,
        language: Language,
    ) -> Self {
        Self {
            range,
            text: text.into(),
            kind,
            semantic: None,
            language: Some(language),
        }
    }

    /// Get the byte length of this token's text
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Check if this token is empty
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Count newlines in this token
    pub fn newline_count(&self) -> usize {
        self.text.chars().filter(|&c| c == '\n').count()
    }
}

/// Summary of tokens in a subtree
#[derive(Debug, Clone)]
pub struct TokenSummary {
    /// Range covered by all tokens
    pub range: Range<Anchor>,
    /// Total number of tokens
    pub token_count: usize,
    /// Total byte length
    pub byte_count: usize,
    /// Total character count
    pub char_count: usize,
    /// Total newline count
    pub newline_count: usize,
    /// All syntax kinds present
    pub kinds: HashSet<SyntaxKind>,
    /// All languages present
    pub languages: HashSet<Language>,
    /// Whether any tokens have semantic info
    pub has_semantic_info: bool,
    /// Whether any error tokens are present
    pub has_errors: bool,
}

impl Default for TokenSummary {
    fn default() -> Self {
        Self {
            range: Anchor::MIN..Anchor::MAX,
            token_count: 0,
            byte_count: 0,
            char_count: 0,
            newline_count: 0,
            kinds: HashSet::default(),
            languages: HashSet::default(),
            has_semantic_info: false,
            has_errors: false,
        }
    }
}

impl Item for Token {
    type Summary = TokenSummary;

    fn summary(&self, _cx: &()) -> TokenSummary {
        let mut kinds = HashSet::default();
        kinds.insert(self.kind);

        let mut languages = HashSet::default();
        if let Some(lang) = self.language {
            languages.insert(lang);
        }

        TokenSummary {
            range: self.range.clone(),
            token_count: 1,
            byte_count: self.text.len(),
            char_count: self.text.chars().count(),
            newline_count: self.newline_count(),
            kinds,
            languages,
            has_semantic_info: self.semantic.is_some(),
            has_errors: self.kind == SyntaxKind::Unknown,
        }
    }
}

impl Summary for TokenSummary {
    type Context = ();

    fn zero(_cx: &()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self, _cx: &()) {
        // Update range to encompass both
        if other.range.start < self.range.start {
            self.range.start = other.range.start;
        }
        if other.range.end > self.range.end {
            self.range.end = other.range.end;
        }

        // Aggregate counts
        self.token_count += other.token_count;
        self.byte_count += other.byte_count;
        self.char_count += other.char_count;
        self.newline_count += other.newline_count;

        // Merge sets
        self.kinds.extend(&other.kinds);
        self.languages.extend(&other.languages);

        // Update flags
        self.has_semantic_info |= other.has_semantic_info;
        self.has_errors |= other.has_errors;
    }
}
