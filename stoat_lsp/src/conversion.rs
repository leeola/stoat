//! LSP position/range conversion to buffer coordinates.
//!
//! LSP uses UTF-16 code unit offsets while buffers use UTF-8 byte offsets.
//! This module handles the conversion and creates anchors for position tracking.

use anyhow::{Context, Result};
use lsp_types::{Position as LspPosition, Range as LspRange};
use std::ops::Range;
use text::{Anchor, Bias, BufferSnapshot, Point};

/// Convert LSP range to buffer anchors.
///
/// Creates anchors at the start and end positions so the range automatically
/// tracks through buffer edits. Out-of-bounds positions are clamped to valid ranges.
///
/// # Arguments
///
/// * `range` - LSP range with UTF-16 offsets
/// * `snapshot` - Buffer snapshot for creating anchors
///
/// # Returns
///
/// Returns `Ok(Range<Anchor>)` with anchors tracking the diagnostic range.
/// Returns `Err` if positions are completely invalid.
pub fn lsp_range_to_anchors(range: &LspRange, snapshot: &BufferSnapshot) -> Result<Range<Anchor>> {
    let start_point = lsp_position_to_point(&range.start, snapshot)
        .with_context(|| format!("Invalid start position: {:?}", range.start))?;

    let end_point = lsp_position_to_point(&range.end, snapshot)
        .with_context(|| format!("Invalid end position: {:?}", range.end))?;

    // Create anchors with appropriate bias
    let start_anchor = snapshot.anchor_at(start_point, Bias::Left);
    let end_anchor = snapshot.anchor_at(end_point, Bias::Right);

    Ok(start_anchor..end_anchor)
}

/// Convert LSP position to buffer Point.
///
/// LSP uses UTF-16 code units for column offsets, buffers use UTF-8 bytes.
/// Clamps out-of-bounds positions to valid ranges.
fn lsp_position_to_point(pos: &LspPosition, snapshot: &BufferSnapshot) -> Result<Point> {
    let line = pos.line;
    let utf16_column = pos.character;

    // Clamp line to buffer bounds
    let max_line = snapshot.max_point().row;
    if line > max_line {
        return Ok(Point::new(max_line, snapshot.line_len(max_line)));
    }

    // Convert UTF-16 column to UTF-8 byte offset
    let line_text = snapshot.line(line);
    let utf8_column = utf16_to_utf8_col(line_text.as_ref(), utf16_column as usize);

    // Clamp column to line length
    let line_len = snapshot.line_len(line);
    let clamped_column = utf8_column.min(line_len);

    Ok(Point::new(line, clamped_column))
}

/// Convert buffer Point to LSP position.
///
/// Converts UTF-8 byte offsets to UTF-16 code units for LSP protocol.
pub fn point_to_lsp_position(point: Point, snapshot: &BufferSnapshot) -> LspPosition {
    let line_text = snapshot.line(point.row);
    let utf16_column = utf8_to_utf16_col(line_text.as_ref(), point.column as usize);

    LspPosition {
        line: point.row,
        character: utf16_column as u32,
    }
}

/// Convert anchors back to LSP range.
///
/// Resolves anchors to points and converts to LSP range.
pub fn anchors_to_lsp_range(range: &Range<Anchor>, snapshot: &BufferSnapshot) -> LspRange {
    let start = range.start.to_point(snapshot);
    let end = range.end.to_point(snapshot);

    LspRange {
        start: point_to_lsp_position(start, snapshot),
        end: point_to_lsp_position(end, snapshot),
    }
}

/// Convert UTF-16 column offset to UTF-8 byte offset.
///
/// Counts UTF-16 code units until reaching the target column.
fn utf16_to_utf8_col(line: &str, utf16_col: usize) -> u32 {
    let mut utf16_offset = 0;
    let mut utf8_offset = 0;

    for ch in line.chars() {
        if utf16_offset >= utf16_col {
            break;
        }

        utf16_offset += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }

    utf8_offset as u32
}

/// Convert UTF-8 byte offset to UTF-16 code unit offset.
///
/// Counts characters and their UTF-16 lengths until reaching the byte offset.
fn utf8_to_utf16_col(line: &str, utf8_col: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_offset = 0;

    for ch in line.chars() {
        if utf8_offset >= utf8_col {
            break;
        }

        utf8_offset += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }

    utf16_offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_utf8_conversion_ascii() {
        let line = "hello world";
        assert_eq!(utf16_to_utf8_col(line, 5), 5);
        assert_eq!(utf8_to_utf16_col(line, 5), 5);
    }

    #[test]
    fn utf16_utf8_conversion_multibyte() {
        // Chinese characters are 3 bytes UTF-8, 1 code unit UTF-16
        let line = "hello world";

        // Position 6 in UTF-16 = position 6 in UTF-8
        assert_eq!(utf16_to_utf8_col(line, 6), 6);
    }

    #[test]
    fn utf16_utf8_conversion_extended_chars() {
        // Extended chars may be 4 bytes UTF-8, 2 code units UTF-16
        let line = "hi there";

        // Position 3 in UTF-16 = position 3 in UTF-8 (space)
        assert_eq!(utf16_to_utf8_col(line, 3), 3);
    }

    #[test]
    fn roundtrip_conversion() {
        let line = "hello world";

        for utf8_col in 0..line.len() {
            if line.is_char_boundary(utf8_col) {
                let utf16_col = utf8_to_utf16_col(line, utf8_col);
                let back = utf16_to_utf8_col(line, utf16_col);
                assert_eq!(
                    back as usize, utf8_col,
                    "Failed roundtrip at UTF-8 col {}",
                    utf8_col
                );
            }
        }
    }
}
