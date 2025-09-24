//! Anchor system for stable positions in mutable text

use clock::Lamport;
use std::{cmp::Ordering, ops::Range};
use sum_tree::Bias;

/// A timestamped position in a buffer that remains stable across edits
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash, Default)]
pub struct Anchor {
    pub timestamp: Lamport,
    /// The byte offset in the buffer at creation time
    pub offset: usize,
    /// Describes which character the anchor is biased towards
    pub bias: Bias,
}

impl Anchor {
    pub const MIN: Self = Self {
        timestamp: Lamport::MIN,
        offset: usize::MIN,
        bias: Bias::Left,
    };

    pub const MAX: Self = Self {
        timestamp: Lamport::MAX,
        offset: usize::MAX,
        bias: Bias::Right,
    };

    pub fn new(timestamp: Lamport, offset: usize, bias: Bias) -> Self {
        Self {
            timestamp,
            offset,
            bias,
        }
    }

    /// Compare two anchors using their positions
    pub fn cmp_by_offset(&self, other: &Anchor) -> Ordering {
        self.offset
            .cmp(&other.offset)
            .then_with(|| self.bias.cmp(&other.bias))
    }
}

impl Ord for Anchor {
    fn cmp(&self, other: &Self) -> Ordering {
        self.timestamp
            .cmp(&other.timestamp)
            .then_with(|| self.offset.cmp(&other.offset))
            .then_with(|| self.bias.cmp(&other.bias))
    }
}

impl PartialOrd for Anchor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Extension trait for anchor ranges
pub trait AnchorRangeExt {
    fn cmp(&self, other: &Range<Anchor>) -> Ordering;
    fn overlaps(&self, other: &Range<Anchor>) -> bool;
}

impl AnchorRangeExt for Range<Anchor> {
    fn cmp(&self, other: &Range<Anchor>) -> Ordering {
        match self.start.cmp(&other.start) {
            Ordering::Equal => other.end.cmp(&self.end),
            ord => ord,
        }
    }

    fn overlaps(&self, other: &Range<Anchor>) -> bool {
        self.start < other.end && other.start < self.end
    }
}
