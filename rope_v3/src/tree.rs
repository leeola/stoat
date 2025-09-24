//! Main SyntaxTree implementation using SumTree

use crate::{
    anchor::Anchor,
    dimensions::{ByteOffset, TokenIndex},
    kinds::SyntaxKind,
    semantic::SemanticInfo,
    token::{Token, TokenSummary},
};
use clock::{Global, Lamport};
use rope::Rope;
use std::ops::Range;
use sum_tree::{Bias, SumTree};

/// A syntax tree with semantic token tracking
pub struct SyntaxTree {
    /// The actual text content using Zed's rope for efficiency
    text: Rope,
    /// Tokens stored in a SumTree for efficient access
    tokens: SumTree<Token>,
    /// Version tracking for edits
    version: Global,
    /// Lamport clock for generating anchors
    clock: Lamport,
}

impl SyntaxTree {
    /// Create a new empty syntax tree
    pub fn new() -> Self {
        Self {
            text: Rope::new(),
            tokens: SumTree::new(&()),
            version: Global::new(),
            clock: Lamport::default(),
        }
    }

    /// Create a syntax tree from text with a simple tokenizer
    pub fn from_text(text: impl AsRef<str>) -> Self {
        let text_str = text.as_ref();
        let mut rope = Rope::new();
        rope.push(text_str);

        let mut tree = Self {
            text: rope,
            tokens: SumTree::new(&()),
            version: Global::new(),
            clock: Lamport::default(),
        };

        // Simple tokenization for demonstration
        tree.tokenize_simple(text_str);
        tree
    }

    /// Simple tokenizer that splits on whitespace and punctuation
    fn tokenize_simple(&mut self, text: &str) {
        let offset = 0;
        let mut chars = text.char_indices().peekable();

        while let Some((idx, ch)) = chars.next() {
            let start_offset = offset + idx;

            let (kind, end_offset) = if ch.is_whitespace() {
                // Collect whitespace
                let mut end = start_offset + ch.len_utf8();
                while let Some((next_idx, next_ch)) = chars.peek() {
                    if !next_ch.is_whitespace() {
                        break;
                    }
                    end = offset + next_idx + next_ch.len_utf8();
                    chars.next();
                }
                let kind = if ch == '\n' {
                    SyntaxKind::Newline
                } else {
                    SyntaxKind::Whitespace
                };
                (kind, end)
            } else if ch.is_ascii_digit() {
                // Collect number
                let mut end = start_offset + ch.len_utf8();
                while let Some((next_idx, next_ch)) = chars.peek() {
                    if !next_ch.is_ascii_digit() && *next_ch != '.' {
                        break;
                    }
                    end = offset + next_idx + next_ch.len_utf8();
                    chars.next();
                }
                (SyntaxKind::Number, end)
            } else if ch.is_alphabetic() || ch == '_' {
                // Collect identifier
                let mut end = start_offset + ch.len_utf8();
                while let Some((next_idx, next_ch)) = chars.peek() {
                    if !next_ch.is_alphanumeric() && *next_ch != '_' {
                        break;
                    }
                    end = offset + next_idx + next_ch.len_utf8();
                    chars.next();
                }
                (SyntaxKind::Identifier, end)
            } else {
                // Single character token
                let kind = match ch {
                    '(' => SyntaxKind::OpenParen,
                    ')' => SyntaxKind::CloseParen,
                    '[' => SyntaxKind::OpenBracket,
                    ']' => SyntaxKind::CloseBracket,
                    '{' => SyntaxKind::OpenBrace,
                    '}' => SyntaxKind::CloseBrace,
                    ',' => SyntaxKind::Comma,
                    ';' => SyntaxKind::Semicolon,
                    ':' => SyntaxKind::Colon,
                    '.' => SyntaxKind::Dot,
                    '+' | '-' | '*' | '/' | '=' | '<' | '>' => SyntaxKind::Operator,
                    _ => SyntaxKind::Unknown,
                };
                (kind, start_offset + ch.len_utf8())
            };

            // Create token with anchors
            let token_text = &text[start_offset..end_offset];
            let start_anchor = Anchor::new(self.clock.tick(), start_offset, Bias::Left);
            let end_anchor = Anchor::new(self.clock.tick(), end_offset, Bias::Right);

            let token = Token::new(start_anchor..end_anchor, token_text, kind);

            self.tokens.push(token, &());
        }
    }

    /// Get the total number of tokens
    pub fn token_count(&self) -> usize {
        self.tokens.summary().token_count
    }

    /// Get the total byte count
    pub fn byte_count(&self) -> usize {
        self.tokens.summary().byte_count
    }

    /// Get the total line count
    pub fn line_count(&self) -> usize {
        self.tokens.summary().newline_count + 1
    }

    /// Find the token at a given byte offset
    pub fn token_at_offset(&self, offset: usize) -> Option<Token> {
        let cursor = self.tokens.cursor::<ByteOffset>(&());
        // This would need proper seeking implementation
        // For now, iterate to find
        for token in cursor {
            if token.range.start.offset <= offset && offset < token.range.end.offset {
                return Some(token.clone());
            }
        }
        None
    }

    /// Find the token at a given token index
    pub fn token_at_index(&self, index: usize) -> Option<Token> {
        let cursor = self.tokens.cursor::<TokenIndex>(&());
        cursor
            .enumerate()
            .find(|(i, _)| *i == index)
            .map(|(_, token)| token.clone())
    }

    /// Find all tokens of a specific kind
    pub fn tokens_of_kind(&self, kind: SyntaxKind) -> Vec<Token> {
        self.tokens
            .cursor::<()>(&())
            .filter(|token| token.kind == kind)
            .cloned()
            .collect()
    }

    /// Find all tokens with semantic info
    pub fn semantic_tokens(&self) -> Vec<Token> {
        self.tokens
            .cursor::<()>(&())
            .filter(|token| token.semantic.is_some())
            .cloned()
            .collect()
    }

    /// Find all error tokens
    pub fn error_tokens(&self) -> Vec<Token> {
        self.tokens_of_kind(SyntaxKind::Unknown)
    }

    /// Add semantic information to a token at the given index
    pub fn add_semantic_info(&mut self, _token_index: usize, _info: SemanticInfo) {
        // This would require rebuilding the tree with the updated token
        // For a real implementation, we'd need edit operations on the SumTree
    }

    /// Get a summary of the tree
    pub fn summary(&self) -> TokenSummary {
        self.tokens.summary().clone()
    }

    /// Get the text content as a string
    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// Get text for a specific token
    pub fn token_text(&self, token: &Token) -> String {
        let start = token.range.start.offset;
        let end = token.range.end.offset;
        self.text.slice(start..end).to_string()
    }

    /// Edit the text and update token positions
    /// Accepts multiple edits as (range, new_text) pairs
    pub fn edit<I, T>(&mut self, edits: I)
    where
        I: IntoIterator<Item = (Range<usize>, T)>,
        T: AsRef<str>,
    {
        // Collect and sort edits by range start (in reverse to apply from end to start)
        let mut edits: Vec<_> = edits
            .into_iter()
            .map(|(range, text)| (range, text.as_ref().to_string()))
            .collect();
        edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));

        // Track the cumulative offset changes
        let old_text = self.text.to_string();

        // Apply edits to the rope
        for (range, new_text) in &edits {
            self.text.replace(range.clone(), new_text);
        }

        // Update version
        let timestamp = self.clock.tick();
        self.version.observe(timestamp);

        // Interpolate token positions based on edits
        self.interpolate_tokens(&edits, &old_text);
    }

    /// Update token positions after edits
    fn interpolate_tokens(&mut self, edits: &[(Range<usize>, String)], _old_text: &str) {
        // For now, we'll re-tokenize the affected ranges
        // In a production system, we'd update anchors more efficiently

        if !edits.is_empty() {
            // Clear existing tokens and re-tokenize
            // This is a simple approach - Zed does incremental updates
            self.tokens = SumTree::new(&());
            let text = self.text.to_string();
            self.tokenize_simple(&text);
        }
    }

    /// Get a cursor for navigating tokens
    pub fn cursor<'a, D>(&'a self) -> sum_tree::Cursor<'a, Token, D>
    where
        D: sum_tree::Dimension<'a, TokenSummary>,
    {
        self.tokens.cursor(&())
    }

    /// Get a cursor starting at a specific byte offset
    pub fn cursor_at_offset(&self, offset: usize) -> sum_tree::Cursor<'_, Token, ByteOffset> {
        let mut cursor = self.tokens.cursor::<ByteOffset>(&());
        cursor.seek(&ByteOffset(offset), Bias::Right);
        cursor
    }
}

impl Default for SyntaxTree {
    fn default() -> Self {
        Self::new()
    }
}
