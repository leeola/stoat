//! Text ranges and positions

use std::ops::Range;
use text_size::{TextRange as TSTextRange, TextSize};

/// A range in text, represented as byte offsets
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextRange {
    start: TextSize,
    end: TextSize,
}

impl TextRange {
    /// Create a new text range
    pub fn new(start: TextSize, end: TextSize) -> Self {
        assert!(start <= end, "Invalid range: start > end");
        Self { start, end }
    }

    /// Create an empty range at the given position
    pub fn empty(pos: TextSize) -> Self {
        Self {
            start: pos,
            end: pos,
        }
    }

    /// Get the start position
    pub fn start(&self) -> TextSize {
        self.start
    }

    /// Get the end position
    pub fn end(&self) -> TextSize {
        self.end
    }

    /// Get the length of the range
    pub fn len(&self) -> TextSize {
        self.end - self.start
    }

    /// Check if the range is empty
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Check if this range contains the given offset
    pub fn contains(&self, offset: TextSize) -> bool {
        self.start <= offset && offset < self.end
    }

    /// Check if this range contains the given range
    pub fn contains_range(&self, other: &TextRange) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// Get the intersection of two ranges
    pub fn intersect(&self, other: &TextRange) -> Option<TextRange> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if start < end {
            Some(TextRange::new(start, end))
        } else {
            None
        }
    }

    /// Extend this range to include the other range
    pub fn extend(&self, other: &TextRange) -> TextRange {
        TextRange::new(self.start.min(other.start), self.end.max(other.end))
    }

    /// Check if this range intersects with another range
    pub fn intersects(&self, other: TextRange) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Get the union of two ranges
    pub fn union(&self, other: TextRange) -> TextRange {
        TextRange::new(self.start.min(other.start), self.end.max(other.end))
    }

    /// Shift this range by the given offset
    pub fn shift(&self, offset: TextSize) -> TextRange {
        TextRange::new(self.start + offset, self.end + offset)
    }
}

impl From<TSTextRange> for TextRange {
    fn from(range: TSTextRange) -> Self {
        Self {
            start: range.start(),
            end: range.end(),
        }
    }
}

impl From<TextRange> for TSTextRange {
    fn from(range: TextRange) -> Self {
        TSTextRange::new(range.start, range.end)
    }
}

impl From<Range<TextSize>> for TextRange {
    fn from(range: Range<TextSize>) -> Self {
        Self::new(range.start, range.end)
    }
}

impl From<Range<usize>> for TextRange {
    fn from(range: Range<usize>) -> Self {
        Self::new(
            TextSize::from(range.start as u32),
            TextSize::from(range.end as u32),
        )
    }
}

impl From<TextRange> for Range<usize> {
    fn from(range: TextRange) -> Self {
        (range.start.into())..(range.end.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_operations() {
        let r1 = TextRange::new(10.into(), 20.into());
        let r2 = TextRange::new(15.into(), 25.into());

        assert_eq!(r1.len(), 10.into());
        assert!(r1.contains(15.into()));
        assert!(!r1.contains(20.into()));

        let intersection = r1.intersect(&r2).expect("Ranges should intersect");
        assert_eq!(intersection, TextRange::new(15.into(), 20.into()));

        let union = r1.extend(&r2);
        assert_eq!(union, TextRange::new(10.into(), 25.into()));
    }
}
