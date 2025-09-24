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
use std::{collections::VecDeque, ops::Range};
use sum_tree::{Bias, SumTree};

pub type TransactionId = Lamport;

/// Represents an edit operation for undo/redo
#[derive(Clone, Debug)]
struct EditOperation {
    #[allow(dead_code)]
    transaction_id: TransactionId,
    edits: Vec<(Range<usize>, String, String)>, // (range, old_text, new_text)
}

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
    /// Undo stack
    undo_stack: VecDeque<EditOperation>,
    /// Redo stack
    redo_stack: VecDeque<EditOperation>,
    /// Maximum undo history size
    max_undo_history: usize,
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
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            max_undo_history: 100,
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
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            max_undo_history: 100,
        };

        // Simple tokenization for demonstration
        tree.tokenize_simple(text_str);
        tree
    }

    /// Simple tokenizer that splits on whitespace and punctuation
    fn tokenize_simple(&mut self, text: &str) {
        // Use a single timestamp for the entire tokenization batch
        let batch_timestamp = self.clock.tick();
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

            // Create token with anchors (use batch timestamp)
            let token_text = &text[start_offset..end_offset];
            let start_anchor = Anchor::new(batch_timestamp, start_offset, Bias::Left);
            let end_anchor = Anchor::new(batch_timestamp, end_offset, Bias::Right);

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

        // Collect old text for undo before applying edits
        let mut undo_edits = Vec::new();
        for (range, new_text) in &edits {
            let old_text = self.text.slice(range.clone()).to_string();
            undo_edits.push((range.clone(), old_text, new_text.clone()));
        }

        // Build a new rope efficiently using cursor (like Zed)
        let mut new_rope = Rope::new();
        let text_len = self.text.len();

        {
            let mut cursor = self.text.cursor(0);
            let mut last_end = 0;

            // Apply edits from start to end using cursor
            for (range, new_text) in &edits {
                // Skip overlapping edits
                if range.start < last_end {
                    continue;
                }

                // Append text before this edit (no allocation)
                if range.start > last_end {
                    cursor.seek_forward(last_end);
                    new_rope.append(cursor.slice(range.start));
                }

                // Add the replacement text
                if !new_text.is_empty() {
                    new_rope.push(new_text);
                }

                last_end = range.end;
            }

            // Append any remaining text after the last edit (no allocation)
            if last_end < text_len {
                cursor.seek_forward(last_end);
                new_rope.append(cursor.suffix());
            }
        } // cursor dropped here

        self.text = new_rope;

        // Store operation for undo (only if not in undo/redo operation)
        if let Some(tx_id) = transaction_id {
            let operation = EditOperation {
                transaction_id: tx_id,
                edits: undo_edits,
            };
            self.undo_stack.push_back(operation);

            // Limit undo history size
            if self.undo_stack.len() > self.max_undo_history {
                self.undo_stack.pop_front();
            }

            // Clear redo stack on new edit
            self.redo_stack.clear();
        }

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
        if edits.is_empty() {
            return;
        }

        // Calculate affected range (union of all edit ranges)
        let mut min_start = usize::MAX;
        let mut max_end = 0;
        for (range, _) in edits {
            min_start = min_start.min(range.start);
            max_end = max_end.max(range.end);
        }

        // Extend range to token boundaries
        let affected_start = self.find_token_boundary_before(min_start);
        let affected_end = self.find_token_boundary_after(max_end);

        // Collect tokens outside affected range
        let mut preserved_before = Vec::new();
        let mut preserved_after = Vec::new();

        {
            let mut cursor = self.tokens.cursor::<ByteOffset>(&());
            cursor.next();

            while let Some(token) = cursor.item() {
                if token.range.end.offset <= affected_start {
                    preserved_before.push(token.clone());
                } else if token.range.start.offset >= affected_end {
                    // Adjust offsets for tokens after the edit
                    let offset_delta = self.calculate_offset_delta(edits, token.range.start.offset);
                    let mut adjusted_token = token.clone();
                    adjusted_token.range.start.offset =
                        (token.range.start.offset as isize + offset_delta) as usize;
                    adjusted_token.range.end.offset =
                        (token.range.end.offset as isize + offset_delta) as usize;
                    preserved_after.push(adjusted_token);
                }
                cursor.next();
            }
        } // cursor dropped here

        // Re-tokenize only the affected range
        let affected_text = self
            .text
            .slice(affected_start..affected_end.min(self.text.len()));
        let affected_str = affected_text.to_string();
        let mut new_tokens = Vec::new();

        // Tokenize the range (extract timestamp generation)
        let batch_timestamp = self.clock.tick();
        Self::tokenize_range_static(
            &affected_str,
            affected_start,
            batch_timestamp,
            &mut new_tokens,
        );

        // Rebuild token tree
        self.tokens = SumTree::new(&());
        for token in preserved_before {
            self.tokens.push(token, &());
        }
        for token in new_tokens {
            self.tokens.push(token, &());
        }
        for token in preserved_after {
            self.tokens.push(token, &());
        }
    }

    /// Find token boundary before offset
    fn find_token_boundary_before(&self, offset: usize) -> usize {
        let mut cursor = self.tokens.cursor::<ByteOffset>(&());
        cursor.seek(&ByteOffset(offset), Bias::Left);
        if let Some(token) = cursor.item() {
            token.range.start.offset
        } else {
            0
        }
    }

    /// Find token boundary after offset
    fn find_token_boundary_after(&self, offset: usize) -> usize {
        let mut cursor = self.tokens.cursor::<ByteOffset>(&());
        cursor.seek(&ByteOffset(offset), Bias::Right);
        if let Some(token) = cursor.item() {
            token.range.end.offset
        } else {
            self.text.len()
        }
    }

    /// Calculate offset delta for positions after edits
    fn calculate_offset_delta(&self, edits: &[(Range<usize>, String)], position: usize) -> isize {
        let mut delta = 0isize;
        for (range, new_text) in edits {
            if range.end <= position {
                delta += new_text.len() as isize - (range.end - range.start) as isize;
            }
        }
        delta
    }

    /// Tokenize a specific range (static version)
    fn tokenize_range_static(
        text: &str,
        base_offset: usize,
        batch_timestamp: Lamport,
        tokens: &mut Vec<Token>,
    ) {
        let mut chars = text.char_indices().peekable();

        while let Some((idx, ch)) = chars.next() {
            let start_offset = base_offset + idx;

            let (kind, local_end) = if ch.is_whitespace() {
                // Collect whitespace
                let mut end = idx + ch.len_utf8();
                while let Some((next_idx, next_ch)) = chars.peek() {
                    if !next_ch.is_whitespace() {
                        break;
                    }
                    end = next_idx + next_ch.len_utf8();
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
                let mut end = idx + ch.len_utf8();
                while let Some((next_idx, next_ch)) = chars.peek() {
                    if !next_ch.is_ascii_digit() && *next_ch != '.' {
                        break;
                    }
                    end = next_idx + next_ch.len_utf8();
                    chars.next();
                }
                (SyntaxKind::Number, end)
            } else if ch.is_alphabetic() || ch == '_' {
                // Collect identifier
                let mut end = idx + ch.len_utf8();
                while let Some((next_idx, next_ch)) = chars.peek() {
                    if !next_ch.is_alphanumeric() && *next_ch != '_' {
                        break;
                    }
                    end = next_idx + next_ch.len_utf8();
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
                (kind, idx + ch.len_utf8())
            };

            let end_offset = base_offset + local_end;
            let token_text = &text[idx..local_end];
            let start_anchor = Anchor::new(batch_timestamp, start_offset, Bias::Left);
            let end_anchor = Anchor::new(batch_timestamp, end_offset, Bias::Right);
            tokens.push(Token::new(start_anchor..end_anchor, token_text, kind));
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

    // === Undo/Redo Support ===

    /// Undo the last edit operation
    pub fn undo(&mut self) -> bool {
        if let Some(operation) = self.undo_stack.pop_back() {
            // Apply inverse edits - need to recalculate ranges for current text
            let mut inverse_edits: Vec<(Range<usize>, String)> = Vec::new();

            // Process edits in forward order to calculate current positions
            let mut offset_delta = 0isize;
            for (orig_range, old_text, new_text) in &operation.edits {
                // Adjust range based on accumulated offset changes
                let current_start = (orig_range.start as isize + offset_delta) as usize;
                let current_end = current_start + new_text.len();

                inverse_edits.push((current_start..current_end, old_text.clone()));

                // Update offset for next edit
                offset_delta += old_text.len() as isize - new_text.len() as isize;
            }

            // Apply without creating undo entry
            self.apply_edits_internal(inverse_edits, false);

            // Move to redo stack
            self.redo_stack.push_back(operation);
            true
        } else {
            false
        }
    }

    /// Redo the last undone operation
    pub fn redo(&mut self) -> bool {
        if let Some(operation) = self.redo_stack.pop_back() {
            // Re-apply original edits
            let mut redo_edits = Vec::new();
            for (range, _old_text, new_text) in &operation.edits {
                redo_edits.push((range.clone(), new_text.clone()));
            }

            // Apply without creating undo entry
            self.apply_edits_internal(redo_edits, false);

            // Move back to undo stack
            self.undo_stack.push_back(operation);
            true
        } else {
            false
        }
    }

    /// Internal method to apply edits without undo tracking
    fn apply_edits_internal(&mut self, edits: Vec<(Range<usize>, String)>, track_undo: bool) {
        if edits.is_empty() {
            return;
        }

        // Store edits for undo if tracking
        let undo_edits = if track_undo {
            let mut undo = Vec::new();
            for (range, new_text) in &edits {
                let old_text = self.text.slice(range.clone()).to_string();
                undo.push((range.clone(), old_text, new_text.clone()));
            }
            Some(undo)
        } else {
            None
        };

        // Build new rope
        let mut new_rope = Rope::new();
        let text_len = self.text.len();

        let mut sorted_edits = edits;
        sorted_edits.sort_by_key(|e| e.0.start);

        {
            let mut cursor = self.text.cursor(0);
            let mut last_end = 0;

            for (range, new_text) in &sorted_edits {
                if range.start < last_end {
                    continue;
                }

                if range.start > last_end {
                    cursor.seek_forward(last_end);
                    new_rope.append(cursor.slice(range.start));
                }

                if !new_text.is_empty() {
                    new_rope.push(new_text);
                }

                last_end = range.end;
            }

            if last_end < text_len {
                cursor.seek_forward(last_end);
                new_rope.append(cursor.suffix());
            }
        } // cursor dropped here

        self.text = new_rope;

        // Update tokens
        self.interpolate_tokens(
            &sorted_edits
                .iter()
                .map(|(r, t)| (r.clone(), t.clone()))
                .collect::<Vec<_>>(),
        );

        // Store undo operation if tracking
        if let Some(undo) = undo_edits {
            let tx_id = self.clock.tick();
            let operation = EditOperation {
                transaction_id: tx_id,
                edits: undo,
            };
            self.undo_stack.push_back(operation);
            if self.undo_stack.len() > self.max_undo_history {
                self.undo_stack.pop_front();
            }
            self.redo_stack.clear();
        }

        let timestamp = self.clock.tick();
        self.version.observe(timestamp);
    }

    /// Check if undo is available
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// Check if redo is available
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Clear undo/redo history
    pub fn clear_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}

impl Default for SyntaxTree {
    fn default() -> Self {
        Self::new()
    }
}
