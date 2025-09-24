//! Main SyntaxTree implementation using SumTree

use crate::{
    anchor::Anchor,
    dimensions::{ByteOffset, TokenIndex},
    kinds::SyntaxKind,
    semantic::SemanticInfo,
    token::{Token, TokenSummary},
};
use clock::{Global, Lamport};
use sum_tree::{Bias, SumTree};

/// A syntax tree with semantic token tracking
pub struct SyntaxTree {
    /// The actual text content (could be replaced with Zed's rope)
    text: String,
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
            text: String::new(),
            tokens: SumTree::new(&()),
            version: Global::new(),
            clock: Lamport::default(),
        }
    }

    /// Create a syntax tree from text with a simple tokenizer
    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();
        let mut tree = Self {
            text: text.clone(),
            tokens: SumTree::new(&()),
            version: Global::new(),
            clock: Lamport::default(),
        };

        // Simple tokenization for demonstration
        tree.tokenize_simple(&text);
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

    /// Get the text content
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Get text for a specific token
    pub fn token_text(&self, token: &Token) -> &str {
        let start = token.range.start.offset;
        let end = token.range.end.offset;
        &self.text[start..end]
    }
}

impl Default for SyntaxTree {
    fn default() -> Self {
        Self::new()
    }
}
