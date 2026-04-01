use crate::{Bias, BufferId};
use std::{cmp::Ordering, ops::Range};

/// A stable reference to a position in a text buffer.
///
/// Unlike simple offsets, anchors remain valid across edits. The `timestamp`
/// identifies which insertion operation created the surrounding text, and
/// `offset` is the byte position within that insertion. Together they form
/// an immutable identity that can be resolved to a concrete buffer offset
/// via the fragment tree in O(log n).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    pub timestamp: u64,
    pub offset: u32,
    pub bias: Bias,
    pub buffer_id: Option<BufferId>,
}

impl Anchor {
    pub fn min() -> Self {
        Self {
            timestamp: 0,
            offset: 0,
            bias: Bias::Left,
            buffer_id: None,
        }
    }

    pub fn max() -> Self {
        Self {
            timestamp: u64::MAX,
            offset: u32::MAX,
            bias: Bias::Right,
            buffer_id: None,
        }
    }

    pub fn min_for_buffer(buffer_id: BufferId) -> Self {
        Self {
            timestamp: 0,
            offset: 0,
            bias: Bias::Left,
            buffer_id: Some(buffer_id),
        }
    }

    pub fn max_for_buffer(buffer_id: BufferId) -> Self {
        Self {
            timestamp: u64::MAX,
            offset: u32::MAX,
            bias: Bias::Right,
            buffer_id: Some(buffer_id),
        }
    }

    pub fn is_min(&self) -> bool {
        self.timestamp == 0 && self.offset == 0 && self.bias == Bias::Left
    }

    pub fn is_max(&self) -> bool {
        self.timestamp == u64::MAX && self.offset == u32::MAX && self.bias == Bias::Right
    }

    pub fn cmp<R: Fn(&Anchor) -> usize>(&self, other: &Anchor, resolve: &R) -> Ordering {
        let self_off = resolve(self);
        let other_off = resolve(other);
        self_off.cmp(&other_off).then(self.bias.cmp(&other.bias))
    }
}

pub trait AnchorRangeExt {
    fn to_offset_range<R: Fn(&Anchor) -> usize>(&self, resolve: &R) -> Range<usize>;
    fn contains_offset<R: Fn(&Anchor) -> usize>(&self, offset: usize, resolve: &R) -> bool;
    fn overlaps_range(&self, range: &Range<usize>, resolve: &impl Fn(&Anchor) -> usize) -> bool;
    fn cmp<R: Fn(&Anchor) -> usize>(&self, other: &Self, resolve: &R) -> Ordering;
    fn is_empty<R: Fn(&Anchor) -> usize>(&self, resolve: &R) -> bool;
    fn len<R: Fn(&Anchor) -> usize>(&self, resolve: &R) -> usize;
    fn intersect<R: Fn(&Anchor) -> usize>(&self, other: &Self, resolve: &R)
        -> Option<Range<usize>>;
    fn canonicalize<R: Fn(&Anchor) -> usize>(&self, resolve: &R) -> Range<Anchor>;
}

impl AnchorRangeExt for Range<Anchor> {
    fn to_offset_range<R: Fn(&Anchor) -> usize>(&self, resolve: &R) -> Range<usize> {
        resolve(&self.start)..resolve(&self.end)
    }

    fn contains_offset<R: Fn(&Anchor) -> usize>(&self, offset: usize, resolve: &R) -> bool {
        let s = resolve(&self.start);
        let e = resolve(&self.end);
        offset >= s && offset < e
    }

    fn overlaps_range(&self, range: &Range<usize>, resolve: &impl Fn(&Anchor) -> usize) -> bool {
        let s = resolve(&self.start);
        let e = resolve(&self.end);
        s < range.end && range.start < e
    }

    fn cmp<R: Fn(&Anchor) -> usize>(&self, other: &Self, resolve: &R) -> Ordering {
        self.start
            .cmp(&other.start, resolve)
            .then_with(|| self.end.cmp(&other.end, resolve))
    }

    fn is_empty<R: Fn(&Anchor) -> usize>(&self, resolve: &R) -> bool {
        resolve(&self.start) >= resolve(&self.end)
    }

    fn len<R: Fn(&Anchor) -> usize>(&self, resolve: &R) -> usize {
        resolve(&self.end).saturating_sub(resolve(&self.start))
    }

    fn intersect<R: Fn(&Anchor) -> usize>(
        &self,
        other: &Self,
        resolve: &R,
    ) -> Option<Range<usize>> {
        let s = resolve(&self.start).max(resolve(&other.start));
        let e = resolve(&self.end).min(resolve(&other.end));
        if s < e {
            Some(s..e)
        } else {
            None
        }
    }

    fn canonicalize<R: Fn(&Anchor) -> usize>(&self, resolve: &R) -> Range<Anchor> {
        if resolve(&self.start) <= resolve(&self.end) {
            self.start..self.end
        } else {
            self.end..self.start
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Anchor, AnchorRangeExt};
    use crate::Bias;
    use std::{cmp::Ordering, collections::HashSet};

    fn anchor(offset: usize) -> Anchor {
        Anchor {
            timestamp: 1,
            offset: offset as u32,
            bias: Bias::Left,
            buffer_id: None,
        }
    }

    fn resolve(a: &Anchor) -> usize {
        a.offset as usize
    }

    #[test]
    fn anchor_hashable() {
        let mut set = HashSet::new();
        set.insert(Anchor::min());
        set.insert(Anchor::max());
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn anchor_is_copy() {
        let a = Anchor::min();
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn anchor_cmp_by_offset() {
        let a = anchor(5);
        let b = anchor(10);
        assert_eq!(a.cmp(&b, &resolve), Ordering::Less);
        assert_eq!(b.cmp(&a, &resolve), Ordering::Greater);
        assert_eq!(a.cmp(&a, &resolve), Ordering::Equal);
    }

    #[test]
    fn anchor_cmp_tiebreak_by_bias() {
        let a = Anchor {
            timestamp: 1,
            offset: 5,
            bias: Bias::Left,
            buffer_id: None,
        };
        let b = Anchor {
            timestamp: 1,
            offset: 5,
            bias: Bias::Right,
            buffer_id: None,
        };
        assert_eq!(a.cmp(&b, &resolve), Ordering::Less);
    }

    #[test]
    fn range_ext_to_offset_range() {
        let range = anchor(3)..anchor(8);
        assert_eq!(range.to_offset_range(&resolve), 3..8);
    }

    #[test]
    fn range_ext_contains_offset() {
        let range = anchor(3)..anchor(8);
        assert!(range.contains_offset(3, &resolve));
        assert!(range.contains_offset(5, &resolve));
        assert!(!range.contains_offset(8, &resolve));
        assert!(!range.contains_offset(2, &resolve));
    }

    #[test]
    fn range_cmp_by_start() {
        let r1 = anchor(3)..anchor(8);
        let r2 = anchor(5)..anchor(10);
        assert_eq!(r1.cmp(&r2, &resolve), Ordering::Less);
    }

    #[test]
    fn range_cmp_same_start_different_end() {
        let r1 = anchor(3)..anchor(8);
        let r2 = anchor(3)..anchor(10);
        assert_eq!(r1.cmp(&r2, &resolve), Ordering::Less);
    }

    #[test]
    fn range_cmp_equal() {
        let r1 = anchor(3)..anchor(8);
        let r2 = anchor(3)..anchor(8);
        assert_eq!(r1.cmp(&r2, &resolve), Ordering::Equal);
    }

    #[test]
    fn range_ext_overlaps_range() {
        let range = anchor(3)..anchor(8);
        assert!(range.overlaps_range(&(5..10), &resolve));
        assert!(range.overlaps_range(&(0..5), &resolve));
        assert!(!range.overlaps_range(&(8..10), &resolve));
        assert!(!range.overlaps_range(&(0..3), &resolve));
    }

    #[test]
    fn range_is_empty() {
        let empty = anchor(5)..anchor(5);
        assert!(empty.is_empty(&resolve));

        let inverted = anchor(8)..anchor(3);
        assert!(inverted.is_empty(&resolve));

        let non_empty = anchor(3)..anchor(8);
        assert!(!non_empty.is_empty(&resolve));
    }

    #[test]
    fn range_len() {
        assert_eq!((anchor(3)..anchor(8)).len(&resolve), 5);
        assert_eq!((anchor(5)..anchor(5)).len(&resolve), 0);
        assert_eq!((anchor(8)..anchor(3)).len(&resolve), 0);
    }

    #[test]
    fn range_intersect_overlapping() {
        let r1 = anchor(2)..anchor(8);
        let r2 = anchor(5)..anchor(10);
        assert_eq!(r1.intersect(&r2, &resolve), Some(5..8));
    }

    #[test]
    fn range_intersect_non_overlapping() {
        let r1 = anchor(2)..anchor(5);
        let r2 = anchor(6)..anchor(10);
        assert_eq!(r1.intersect(&r2, &resolve), None);
    }

    #[test]
    fn range_intersect_touching() {
        let r1 = anchor(2)..anchor(5);
        let r2 = anchor(5)..anchor(10);
        assert_eq!(r1.intersect(&r2, &resolve), None);
    }

    #[test]
    fn canonicalize_normal() {
        let range = anchor(3)..anchor(8);
        let canon = range.canonicalize(&resolve);
        assert_eq!(resolve(&canon.start), 3);
        assert_eq!(resolve(&canon.end), 8);
    }

    #[test]
    fn canonicalize_swaps_inverted() {
        let range = anchor(8)..anchor(3);
        let canon = range.canonicalize(&resolve);
        assert_eq!(resolve(&canon.start), 3);
        assert_eq!(resolve(&canon.end), 8);
    }

    #[test]
    fn is_min_is_max() {
        assert!(Anchor::min().is_min());
        assert!(!Anchor::min().is_max());
        assert!(Anchor::max().is_max());
        assert!(!Anchor::max().is_min());
    }

    #[test]
    fn buffer_scoped_sentinels() {
        use crate::BufferId;
        let id = BufferId::new(42);
        let min = Anchor::min_for_buffer(id);
        let max = Anchor::max_for_buffer(id);
        assert!(min.is_min());
        assert!(max.is_max());
        assert_eq!(min.buffer_id, Some(id));
        assert_eq!(max.buffer_id, Some(id));
    }
}
