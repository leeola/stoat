//! Adapter layer between editor types and rope AST implementation.
//!
//! This module provides conversion utilities and compatibility functions
//! to integrate the [`stoat_rope`] AST-driven rope with the editor's
//! existing text handling system.

use crate::actions::{TextPosition, TextRange};
use std::sync::Arc;
use stoat_rope::{
    ast::{AstNode, TextPos, TextRange as RopeTextRange},
    kind::SyntaxKind,
    RopeAst,
};

/// Convert editor TextPosition to rope TextPos
pub fn editor_pos_to_rope(pos: TextPosition) -> TextPos {
    // Convert line/column to byte offset
    // For now, we'll use a simple approximation - this will need
    // to be updated to properly calculate byte offsets from line/column
    TextPos(pos.line * 80 + pos.column) // Rough approximation
}

/// Convert rope TextPos to editor TextPosition
pub fn rope_pos_to_editor(pos: TextPos, _rope: &RopeAst) -> TextPosition {
    // Convert byte offset to line/column
    // This is a simplified implementation that will need refinement
    let line = pos.0 / 80; // Rough approximation
    let column = pos.0 % 80;
    TextPosition::new(line, column)
}

/// Convert editor TextRange to rope TextRange
pub fn editor_range_to_rope(range: TextRange) -> RopeTextRange {
    RopeTextRange::new(
        range.start.line * 80 + range.start.column,
        range.end.line * 80 + range.end.column,
    )
}

/// Convert rope TextRange to editor TextRange
pub fn rope_range_to_editor(range: RopeTextRange, rope: &RopeAst) -> TextRange {
    TextRange::new(
        rope_pos_to_editor(range.start, rope),
        rope_pos_to_editor(range.end, rope),
    )
}

/// Create a RopeAst from plain text content.
///
/// This function tokenizes plain text into a simple rope structure
/// with basic token types (text, whitespace, newlines).
pub fn rope_from_text(text: &str) -> RopeAst {
    if text.is_empty() {
        let root = Arc::new(AstNode::syntax(
            SyntaxKind::Document,
            RopeTextRange::new(0, 0),
        ));
        return RopeAst::from_root(root);
    }

    // Create a single text token containing all the text
    // This is a simple implementation - later we can add tokenization
    let token = Arc::new(AstNode::token(
        SyntaxKind::Text,
        text.into(),
        RopeTextRange::new(0, text.len()),
    ));

    RopeAst::from_root(token)
}

/// Extract plain text from a RopeAst.
///
/// This reconstructs the original text content from the rope's
/// AST structure by concatenating all token text.
pub fn text_from_rope(rope: &RopeAst) -> String {
    let total_range = RopeTextRange::new(0, rope.len_bytes());
    rope.text_at_range(total_range)
}

/// Get line iterator from rope.
///
/// This provides compatibility with the editor's existing line-based
/// operations by extracting lines from the rope structure.
pub fn lines_from_rope(rope: &RopeAst) -> impl Iterator<Item = String> {
    let text = text_from_rope(rope);
    text.lines()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Calculate line and column from byte offset in rope.
///
/// This function properly converts byte offsets to line/column positions
/// by traversing the rope structure and counting lines/characters.
pub fn byte_offset_to_line_col(rope: &RopeAst, byte_offset: usize) -> TextPosition {
    let text = text_from_rope(rope);
    let mut line = 0;
    let mut col = 0;
    let mut bytes_seen = 0;

    for ch in text.chars() {
        if bytes_seen >= byte_offset {
            break;
        }

        bytes_seen += ch.len_utf8();

        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }

    TextPosition::new(line, col)
}

/// Calculate byte offset from line and column in rope.
///
/// This function converts line/column positions to byte offsets
/// by traversing the text content.
pub fn line_col_to_byte_offset(rope: &RopeAst, pos: TextPosition) -> usize {
    let text = text_from_rope(rope);
    let mut current_line = 0;
    let mut current_col = 0;
    let mut byte_offset = 0;

    for ch in text.chars() {
        if current_line == pos.line && current_col == pos.column {
            return byte_offset;
        }

        if ch == '\n' {
            current_line += 1;
            current_col = 0;
        } else {
            current_col += 1;
        }

        byte_offset += ch.len_utf8();
    }

    byte_offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rope_from_empty_text() {
        let rope = rope_from_text("");
        assert!(rope.is_empty());
    }

    #[test]
    fn test_rope_from_simple_text() {
        let text = "hello world";
        let rope = rope_from_text(text);
        assert_eq!(text_from_rope(&rope), text);
    }

    #[test]
    fn test_rope_with_newlines() {
        let text = "hello\nworld\n";
        let rope = rope_from_text(text);
        assert_eq!(text_from_rope(&rope), text);
    }

    #[test]
    fn test_line_col_conversion() {
        let text = "hello\nworld\ntest";
        let rope = rope_from_text(text);

        // Test position at start of second line
        let pos = TextPosition::new(1, 0);
        let byte_offset = line_col_to_byte_offset(&rope, pos);
        let back_to_pos = byte_offset_to_line_col(&rope, byte_offset);

        assert_eq!(back_to_pos, pos);
    }
}
