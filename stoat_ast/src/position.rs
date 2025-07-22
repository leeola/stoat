//! Position tracking types for AST nodes

use std::ops::Range;

/// Byte position in the source text
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextPos(pub usize);

impl TextPos {
    /// Create a new text position
    pub const fn new(offset: usize) -> Self {
        Self(offset)
    }

    /// Get the byte offset
    pub const fn offset(&self) -> usize {
        self.0
    }

    /// Advance the position by the given number of bytes
    pub fn advance(&mut self, bytes: usize) {
        self.0 += bytes;
    }
}

/// Extension trait for Range<TextPos> to add convenience methods
pub trait TextRangeExt {
    /// Create a range from byte offsets
    fn from_offsets(start: usize, end: usize) -> Self;

    /// Get the length of this range in bytes
    fn len(&self) -> usize;

    /// Check if this range is empty
    fn is_empty(&self) -> bool;

    /// Check if this range contains the given position
    fn contains_pos(&self, pos: TextPos) -> bool;

    /// Check if this range overlaps with another
    fn overlaps(&self, other: &Range<TextPos>) -> bool;

    /// Combine two ranges to create a range that spans both
    fn union(&self, other: &Range<TextPos>) -> Self;
}

impl TextRangeExt for Range<TextPos> {
    fn from_offsets(start: usize, end: usize) -> Self {
        TextPos(start)..TextPos(end)
    }

    fn len(&self) -> usize {
        self.end.0.saturating_sub(self.start.0)
    }

    fn is_empty(&self) -> bool {
        self.start.0 >= self.end.0
    }

    fn contains_pos(&self, pos: TextPos) -> bool {
        self.contains(&pos)
    }

    fn overlaps(&self, other: &Range<TextPos>) -> bool {
        self.start.0 < other.end.0 && other.start.0 < self.end.0
    }

    fn union(&self, other: &Range<TextPos>) -> Self {
        let start = self.start.0.min(other.start.0);
        let end = self.end.0.max(other.end.0);
        TextPos(start)..TextPos(end)
    }
}

/// Cached metadata about text content and structure
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInfo {
    /// Total bytes in this subtree
    pub bytes: usize,
    /// Total UTF-8 characters in this subtree
    pub chars: usize,
    /// Total tokens in this subtree (leaves count as 1)
    pub tokens: usize,
    /// Total newlines in this subtree
    pub newlines: usize,
}

impl TextInfo {
    /// Create text info for an empty node
    pub const fn empty() -> Self {
        Self {
            bytes: 0,
            chars: 0,
            tokens: 0,
            newlines: 0,
        }
    }

    /// Create text info from a text string
    pub fn from_text(text: &str) -> Self {
        Self {
            bytes: text.len(),
            chars: text.chars().count(),
            tokens: 1,
            newlines: text.chars().filter(|&c| c == '\n').count(),
        }
    }

    /// Combine two text infos by summing their fields
    pub const fn combine(&self, other: &Self) -> Self {
        Self {
            bytes: self.bytes + other.bytes,
            chars: self.chars + other.chars,
            tokens: self.tokens + other.tokens,
            newlines: self.newlines + other.newlines,
        }
    }

    /// Combine multiple text infos
    pub fn combine_many(infos: &[TextInfo]) -> Self {
        infos
            .iter()
            .fold(Self::empty(), |acc, info| acc.combine(info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::TextRangeExt;

    #[test]
    fn text_pos_operations() {
        let mut pos = TextPos::new(10);
        assert_eq!(pos.offset(), 10);

        pos.advance(5);
        assert_eq!(pos.offset(), 15);
    }

    #[test]
    fn text_range_operations() {
        let range = Range::<TextPos>::from_offsets(10, 20);
        assert_eq!(range.len(), 10);
        assert!(!range.is_empty());

        let empty = Range::<TextPos>::from_offsets(10, 10);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());

        // Test contains
        assert!(range.contains_pos(TextPos(15)));
        assert!(!range.contains_pos(TextPos(5)));
        assert!(!range.contains_pos(TextPos(20))); // end is exclusive

        // Test overlaps
        let other = Range::<TextPos>::from_offsets(15, 25);
        assert!(range.overlaps(&other));

        let disjoint = Range::<TextPos>::from_offsets(25, 30);
        assert!(!range.overlaps(&disjoint));

        // Test union
        let union = range.union(&other);
        assert_eq!(union.start.0, 10);
        assert_eq!(union.end.0, 25);
    }

    #[test]
    fn text_info_operations() {
        let info1 = TextInfo::from_text("hello\nworld");
        assert_eq!(info1.bytes, 11);
        assert_eq!(info1.chars, 11);
        assert_eq!(info1.tokens, 1);
        assert_eq!(info1.newlines, 1);

        let info2 = TextInfo::from_text("foo");
        let combined = info1.combine(&info2);
        assert_eq!(combined.bytes, 14);
        assert_eq!(combined.chars, 14);
        assert_eq!(combined.tokens, 2);
        assert_eq!(combined.newlines, 1);

        // Test unicode
        let unicode_info = TextInfo::from_text("crab_rust");
        assert_eq!(unicode_info.bytes, 9);
        assert_eq!(unicode_info.chars, 9);
    }

    #[test]
    fn text_info_combine_many() {
        let infos = vec![
            TextInfo::from_text("hello"),
            TextInfo::from_text("\n"),
            TextInfo::from_text("world"),
        ];

        let combined = TextInfo::combine_many(&infos);
        assert_eq!(combined.bytes, 11);
        assert_eq!(combined.chars, 11);
        assert_eq!(combined.tokens, 3);
        assert_eq!(combined.newlines, 1);
    }
}
