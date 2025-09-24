//! Navigation dimensions for the SumTree

use crate::{anchor::Anchor, kinds::SyntaxKind, semantic::SemanticKind, token::TokenSummary};
use sum_tree::Dimension;

/// Navigate by byte offset
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteOffset(pub usize);

impl<'a> Dimension<'a, TokenSummary> for ByteOffset {
    fn zero(_cx: &()) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, summary: &'a TokenSummary, _cx: &()) {
        self.0 += summary.byte_count;
    }
}

/// Navigate by token index
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TokenIndex(pub usize);

impl<'a> Dimension<'a, TokenSummary> for TokenIndex {
    fn zero(_cx: &()) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, summary: &'a TokenSummary, _cx: &()) {
        self.0 += summary.token_count;
    }
}

/// Navigate by line number
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineNumber(pub usize);

impl<'a> Dimension<'a, TokenSummary> for LineNumber {
    fn zero(_cx: &()) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, summary: &'a TokenSummary, _cx: &()) {
        self.0 += summary.newline_count;
    }
}

/// Navigate by anchor position
impl<'a> Dimension<'a, TokenSummary> for Anchor {
    fn zero(_cx: &()) -> Self {
        Anchor::MIN
    }

    fn add_summary(&mut self, summary: &'a TokenSummary, _cx: &()) {
        *self = summary.range.end;
    }
}

/// Find tokens of a specific syntax kind
#[derive(Clone, Copy, Debug)]
pub struct SyntaxKindOffset {
    pub kind: SyntaxKind,
    pub occurrence: usize,
    current: usize,
}

impl SyntaxKindOffset {
    pub fn new(kind: SyntaxKind, occurrence: usize) -> Self {
        Self {
            kind,
            occurrence,
            current: 0,
        }
    }
}

impl<'a> Dimension<'a, TokenSummary> for SyntaxKindOffset {
    fn zero(_cx: &()) -> Self {
        Self {
            kind: SyntaxKind::Unknown,
            occurrence: 0,
            current: 0,
        }
    }

    fn add_summary(&mut self, summary: &'a TokenSummary, _cx: &()) {
        if summary.kinds.contains(&self.kind) {
            // This is approximate - we'd need to traverse to get exact count
            // In practice, you'd use a cursor and check each token
            self.current += 1;
        }
    }
}

/// Find tokens with semantic info
#[derive(Clone, Copy, Debug)]
pub struct SemanticOffset {
    pub kind: Option<SemanticKind>,
    pub occurrence: usize,
    current: usize,
}

impl SemanticOffset {
    pub fn new(kind: Option<SemanticKind>, occurrence: usize) -> Self {
        Self {
            kind,
            occurrence,
            current: 0,
        }
    }
}

impl<'a> Dimension<'a, TokenSummary> for SemanticOffset {
    fn zero(_cx: &()) -> Self {
        Self {
            kind: None,
            occurrence: 0,
            current: 0,
        }
    }

    fn add_summary(&mut self, summary: &'a TokenSummary, _cx: &()) {
        if summary.has_semantic_info {
            // This is approximate - actual filtering would happen during traversal
            self.current += 1;
        }
    }
}

/// Find error tokens
#[derive(Clone, Copy, Debug, Default)]
pub struct ErrorOffset {
    pub occurrence: usize,
    current: usize,
}

impl ErrorOffset {
    pub fn new(occurrence: usize) -> Self {
        Self {
            occurrence,
            current: 0,
        }
    }
}

impl<'a> Dimension<'a, TokenSummary> for ErrorOffset {
    fn zero(_cx: &()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, summary: &'a TokenSummary, _cx: &()) {
        if summary.has_errors {
            self.current += 1;
        }
    }
}
