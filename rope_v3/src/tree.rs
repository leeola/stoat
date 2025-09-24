//! Main SyntaxTree implementation using SumTree

use crate::{
    anchor::Anchor,
    dimensions::{ByteOffset, TokenIndex},
    kinds::SyntaxKind,
    semantic::SemanticInfo,
    token::{Token, TokenSummary},
};
use clock::{Global, Lamport};
use rope::{Point, Rope};
use std::ops::Range;
use sum_tree::{Bias, SumTree};

pub type TransactionId = Lamport;

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
    /// Current transaction ID if in a transaction
    transaction_id: Option<TransactionId>,
    /// Depth of nested transactions
    transaction_depth: usize,
}

impl SyntaxTree {
    /// Create a new empty syntax tree
    pub fn new() -> Self {
        Self {
            text: Rope::new(),
            tokens: SumTree::new(&()),
            version: Global::new(),
            clock: Lamport::default(),
            transaction_id: None,
            transaction_depth: 0,
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
            transaction_id: None,
            transaction_depth: 0,
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
    pub fn edit<I, T>(&mut self, edits: I) -> Option<TransactionId>
    where
        I: IntoIterator<Item = (Range<usize>, T)>,
        T: AsRef<str>,
    {
        let transaction_id = self.start_transaction();

        // Collect and sort edits by range start (in reverse to apply from end to start)
        let mut edits: Vec<_> = edits
            .into_iter()
            .map(|(range, text)| (range, text.as_ref().to_string()))
            .collect();

        if edits.is_empty() {
            self.end_transaction();
            return None;
        }

        // Sort edits by start position (ascending)
        edits.sort_by_key(|e| e.0.start);

        // Build a new rope with all edits applied at once
        let mut new_rope = Rope::new();
        let mut last_end = 0;

        // Apply edits from start to end
        for (range, new_text) in &edits {
            // Add text before this edit
            if range.start > last_end {
                let between = self.text.slice(last_end..range.start);
                new_rope.push(&between.to_string());
            }
            // Add the replacement text
            if !new_text.is_empty() {
                new_rope.push(new_text);
            }
            last_end = range.end.max(last_end);
        }

        // Add any remaining text after the last edit
        if last_end < self.text.len() {
            let suffix = self.text.slice(last_end..self.text.len());
            new_rope.push(&suffix.to_string());
        }

        self.text = new_rope;

        // Update version with single timestamp
        let timestamp = self.clock.tick();
        self.version.observe(timestamp);

        // Interpolate token positions based on edits
        self.interpolate_tokens(&edits);

        self.end_transaction();
        transaction_id
    }

    /// Update token positions after edits
    fn interpolate_tokens(&mut self, edits: &[(Range<usize>, String)]) {
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

    /// Start a new transaction
    pub fn start_transaction(&mut self) -> Option<TransactionId> {
        self.transaction_depth += 1;
        if self.transaction_depth == 1 {
            let id = self.clock.tick();
            self.transaction_id = Some(id);
            Some(id)
        } else {
            self.transaction_id
        }
    }

    /// End the current transaction
    pub fn end_transaction(&mut self) -> Option<TransactionId> {
        if self.transaction_depth > 0 {
            self.transaction_depth -= 1;
            if self.transaction_depth == 0 {
                let id = self.transaction_id.take();
                return id;
            }
        }
        self.transaction_id
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

    // === Coordinate Conversion Methods ===

    /// Convert byte offset to Point (line/column)
    pub fn offset_to_point(&self, offset: usize) -> Point {
        self.text.offset_to_point(offset)
    }

    /// Convert Point (line/column) to byte offset
    pub fn point_to_offset(&self, point: Point) -> usize {
        self.text.point_to_offset(point)
    }

    /// Clip offset to valid position
    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        self.text.clip_offset(offset, bias)
    }

    /// Get a cursor at a specific point
    pub fn cursor_at_point(&self, point: Point) -> sum_tree::Cursor<'_, Token, ByteOffset> {
        let offset = self.point_to_offset(point);
        self.cursor_at_offset(offset)
    }

    // === Efficient Text Access ===

    /// Get the total byte length
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Get text slice as a rope (zero-copy)
    pub fn text_slice(&self, range: Range<usize>) -> Rope {
        self.text.slice(range)
    }

    /// Iterate over text chunks (zero-copy)
    pub fn chunks(&self) -> rope::Chunks<'_> {
        self.text.chunks()
    }

    /// Iterate over text chunks in range (zero-copy)
    pub fn chunks_in_range(&self, range: Range<usize>) -> rope::Chunks<'_> {
        self.text.chunks_in_range(range)
    }

    // === Range Queries ===

    /// Get tokens in a byte range
    pub fn tokens_in_range(&self, range: Range<usize>) -> Vec<Token> {
        let mut result = Vec::new();
        let mut cursor = self.cursor_at_offset(range.start);

        while let Some(token) = cursor.item() {
            if token.range.start.offset >= range.end {
                break;
            }
            if token.range.end.offset > range.start {
                result.push(token.clone());
            }
            cursor.next();
        }

        result
    }

    /// Get the line length for a given row
    pub fn line_len(&self, row: u32) -> u32 {
        self.text.line_len(row)
    }

    /// Get maximum point in the text
    pub fn max_point(&self) -> Point {
        self.text.max_point()
    }
}

impl Default for SyntaxTree {
    fn default() -> Self {
        Self::new()
    }
}
