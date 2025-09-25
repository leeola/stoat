//! TokenMap - Manages AST tokens synchronized with a text buffer

use crate::{
    kinds::SyntaxKind,
    token::{TokenEntry, TokenSummary},
};
use std::ops::Range;
use sum_tree::SumTree;
use text::{Anchor, BufferSnapshot, ToOffset};

/// Manages AST tokens synchronized with a text buffer (similar to Zed's SyntaxMap)
#[derive(Clone)]
pub struct TokenMap {
    snapshot: TokenSnapshot,
}

/// Immutable snapshot of the token map
#[derive(Clone)]
pub struct TokenSnapshot {
    /// Tokens stored in a SumTree for efficient access
    pub tokens: SumTree<TokenEntry>,
    /// Version of the buffer this snapshot corresponds to
    pub version: clock::Global,
}

impl TokenMap {
    /// Create a new empty token map
    pub fn new(buffer: &BufferSnapshot) -> Self {
        Self {
            snapshot: TokenSnapshot {
                tokens: SumTree::new(buffer),
                version: clock::Global::new(),
            },
        }
    }

    /// Get a snapshot of the current state
    pub fn snapshot(&self) -> TokenSnapshot {
        self.snapshot.clone()
    }

    /// Synchronize tokens with buffer changes
    pub fn sync(&mut self, buffer: &BufferSnapshot, edits: &[text::Edit<usize>]) {
        if edits.is_empty() {
            return;
        }

        // For each edit, update token positions and re-tokenize affected ranges
        for edit in edits {
            let start_anchor = buffer.anchor_before(edit.new.start);
            let end_anchor = buffer.anchor_after(edit.new.end);
            let range = start_anchor..end_anchor;

            // Remove tokens in the edited range
            self.remove_tokens_in_range(&range, buffer);

            // Re-tokenize the edited text
            let new_text = buffer.text_for_range(edit.new.clone()).collect::<String>();
            let new_tokens = tokenize_text(&new_text, range.clone());

            // Insert new tokens
            for token in new_tokens {
                self.snapshot.tokens.push(token, buffer);
            }
        }

        // Update version to match buffer
        self.snapshot.version = buffer.version().clone();
    }

    /// Remove tokens within a range
    fn remove_tokens_in_range(&mut self, range: &Range<Anchor>, buffer: &BufferSnapshot) {
        let mut tokens_to_keep = Vec::new();
        {
            let mut cursor = self.snapshot.tokens.cursor::<TokenSummary>(buffer);
            // Collect tokens outside the range
            cursor.next();
            while let Some(token) = cursor.item() {
                if token.range.end.cmp(&range.start, buffer).is_le()
                    || token.range.start.cmp(&range.end, buffer).is_ge()
                {
                    tokens_to_keep.push(token.clone());
                }
                cursor.next();
            }
        }

        // Rebuild the tree with kept tokens
        self.snapshot.tokens = SumTree::new(buffer);
        for token in tokens_to_keep {
            self.snapshot.tokens.push(token, buffer);
        }
    }
}

impl TokenSnapshot {
    /// Get all tokens of a specific kind
    pub fn tokens_of_kind(&self, kind: SyntaxKind, buffer: &BufferSnapshot) -> Vec<TokenEntry> {
        let mut result = Vec::new();
        let mut cursor = self.tokens.cursor::<TokenSummary>(buffer);
        cursor.next();

        while let Some(token) = cursor.item() {
            if token.kind == kind {
                result.push(token.clone());
            }
            cursor.next();
        }

        result
    }

    /// Get token at a specific byte offset
    pub fn token_at_offset(&self, offset: usize, buffer: &BufferSnapshot) -> Option<TokenEntry> {
        let _anchor = buffer.anchor_before(offset);
        let mut cursor = self.tokens.cursor::<TokenSummary>(buffer);
        cursor.next();

        while let Some(token) = cursor.item() {
            let start_offset = token.range.start.to_offset(buffer);
            let end_offset = token.range.end.to_offset(buffer);

            if start_offset <= offset && offset < end_offset {
                return Some(token.clone());
            }

            if start_offset > offset {
                break;
            }

            cursor.next();
        }

        None
    }

    /// Get all error tokens
    pub fn error_tokens(&self, buffer: &BufferSnapshot) -> Vec<TokenEntry> {
        self.tokens_of_kind(SyntaxKind::Unknown, buffer)
    }

    /// Get tokens in a byte range
    pub fn tokens_in_range(&self, range: Range<usize>, buffer: &BufferSnapshot) -> Vec<TokenEntry> {
        let start_anchor = buffer.anchor_before(range.start);
        let end_anchor = buffer.anchor_after(range.end);
        let mut result = Vec::new();
        let mut cursor = self.tokens.cursor::<TokenSummary>(buffer);
        cursor.next();

        while let Some(token) = cursor.item() {
            if token.range.start.cmp(&end_anchor, buffer).is_ge() {
                break;
            }

            if token.range.end.cmp(&start_anchor, buffer).is_gt() {
                result.push(token.clone());
            }

            cursor.next();
        }

        result
    }

    /// Get total token count
    pub fn token_count(&self, _buffer: &BufferSnapshot) -> usize {
        self.tokens.summary().token_count
    }

    /// Get summary of all tokens
    pub fn summary(&self, _buffer: &BufferSnapshot) -> TokenSummary {
        self.tokens.summary().clone()
    }
}

/// Simple tokenizer that splits on whitespace and punctuation
fn tokenize_text(text: &str, range: Range<Anchor>) -> Vec<TokenEntry> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        let start_idx = idx;

        let kind = if ch.is_whitespace() {
            // Skip whitespace tokens for now
            while chars.peek().map_or(false, |(_, c)| c.is_whitespace()) {
                chars.next();
            }
            continue;
        } else if ch.is_alphanumeric() || ch == '_' {
            let mut _end_idx = start_idx + ch.len_utf8();
            while let Some((next_idx, next_ch)) = chars.peek() {
                if next_ch.is_alphanumeric() || *next_ch == '_' {
                    _end_idx = *next_idx + next_ch.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }

            let word = &text[start_idx.._end_idx];
            match word {
                "let" | "mut" | "fn" | "if" | "else" | "while" | "for" | "return" | "pub"
                | "struct" | "enum" | "impl" | "trait" | "use" | "mod" => SyntaxKind::Keyword,
                _ if word.chars().all(|c| c.is_numeric()) => SyntaxKind::Number,
                _ => SyntaxKind::Identifier,
            }
        } else if ch == '(' {
            SyntaxKind::OpenParen
        } else if ch == ')' {
            SyntaxKind::CloseParen
        } else if ch == '[' {
            SyntaxKind::OpenBracket
        } else if ch == ']' {
            SyntaxKind::CloseBracket
        } else if ch == '{' {
            SyntaxKind::OpenBrace
        } else if ch == '}' {
            SyntaxKind::CloseBrace
        } else if "+-*/%".contains(ch) {
            SyntaxKind::Operator
        } else if ch == '.' {
            SyntaxKind::Dot
        } else if ch == ',' {
            SyntaxKind::Comma
        } else if ch == ':' {
            SyntaxKind::Colon
        } else if ch == ';' {
            SyntaxKind::Semicolon
        } else if ch == '"' || ch == '\'' {
            let quote = ch;
            let mut _end_idx = start_idx + ch.len_utf8();
            let mut escaped = false;
            while let Some((next_idx, next_ch)) = chars.next() {
                _end_idx = next_idx + next_ch.len_utf8();
                if escaped {
                    escaped = false;
                } else if next_ch == '\\' {
                    escaped = true;
                } else if next_ch == quote {
                    break;
                }
            }
            SyntaxKind::String
        } else {
            SyntaxKind::Unknown
        };

        // Note: In a real implementation, we'd calculate proper anchors based on
        // the actual position in the buffer. For now, we reuse the provided range.
        tokens.push(TokenEntry::new(range.clone(), kind));
    }

    tokens
}
