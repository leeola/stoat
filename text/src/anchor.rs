use crate::Bias;
use std::{cmp::Ordering, ops::Range};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    pub version: usize,
    pub offset: usize,
    pub bias: Bias,
}

impl Anchor {
    pub fn min() -> Self {
        Self {
            version: 0,
            offset: 0,
            bias: Bias::Left,
        }
    }

    pub fn max() -> Self {
        Self {
            version: 0,
            offset: usize::MAX,
            bias: Bias::Right,
        }
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
        let a = Anchor {
            version: 0,
            offset: 5,
            bias: Bias::Left,
        };
        let b = Anchor {
            version: 0,
            offset: 10,
            bias: Bias::Left,
        };
        let resolve = |anchor: &Anchor| anchor.offset;
        assert_eq!(a.cmp(&b, &resolve), Ordering::Less);
        assert_eq!(b.cmp(&a, &resolve), Ordering::Greater);
        assert_eq!(a.cmp(&a, &resolve), Ordering::Equal);
    }

    #[test]
    fn anchor_cmp_tiebreak_by_bias() {
        let a = Anchor {
            version: 0,
            offset: 5,
            bias: Bias::Left,
        };
        let b = Anchor {
            version: 0,
            offset: 5,
            bias: Bias::Right,
        };
        let resolve = |anchor: &Anchor| anchor.offset;
        assert_eq!(a.cmp(&b, &resolve), Ordering::Less);
    }

    #[test]
    fn range_ext_to_offset_range() {
        let range = Anchor {
            version: 0,
            offset: 3,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 8,
            bias: Bias::Right,
        };
        let resolve = |a: &Anchor| a.offset;
        assert_eq!(range.to_offset_range(&resolve), 3..8);
    }

    #[test]
    fn range_ext_contains_offset() {
        let range = Anchor {
            version: 0,
            offset: 3,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 8,
            bias: Bias::Right,
        };
        let resolve = |a: &Anchor| a.offset;
        assert!(range.contains_offset(3, &resolve));
        assert!(range.contains_offset(5, &resolve));
        assert!(!range.contains_offset(8, &resolve));
        assert!(!range.contains_offset(2, &resolve));
    }

    #[test]
    fn range_cmp_by_start() {
        let r1 = Anchor {
            version: 0,
            offset: 3,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 8,
            bias: Bias::Right,
        };
        let r2 = Anchor {
            version: 0,
            offset: 5,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 10,
            bias: Bias::Right,
        };
        let resolve = |a: &Anchor| a.offset;
        assert_eq!(r1.cmp(&r2, &resolve), Ordering::Less);
    }

    #[test]
    fn range_cmp_same_start_different_end() {
        let r1 = Anchor {
            version: 0,
            offset: 3,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 8,
            bias: Bias::Right,
        };
        let r2 = Anchor {
            version: 0,
            offset: 3,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 10,
            bias: Bias::Right,
        };
        let resolve = |a: &Anchor| a.offset;
        assert_eq!(r1.cmp(&r2, &resolve), Ordering::Less);
    }

    #[test]
    fn range_cmp_equal() {
        let r1 = Anchor {
            version: 0,
            offset: 3,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 8,
            bias: Bias::Right,
        };
        let r2 = Anchor {
            version: 0,
            offset: 3,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 8,
            bias: Bias::Right,
        };
        let resolve = |a: &Anchor| a.offset;
        assert_eq!(r1.cmp(&r2, &resolve), Ordering::Equal);
    }

    #[test]
    fn range_ext_overlaps_range() {
        let range = Anchor {
            version: 0,
            offset: 3,
            bias: Bias::Left,
        }..Anchor {
            version: 0,
            offset: 8,
            bias: Bias::Right,
        };
        let resolve = |a: &Anchor| a.offset;
        assert!(range.overlaps_range(&(5..10), &resolve));
        assert!(range.overlaps_range(&(0..5), &resolve));
        assert!(!range.overlaps_range(&(8..10), &resolve));
        assert!(!range.overlaps_range(&(0..3), &resolve));
    }

    fn anchor(offset: usize) -> Anchor {
        Anchor {
            version: 0,
            offset,
            bias: Bias::Left,
        }
    }

    #[test]
    fn range_is_empty() {
        let resolve = |a: &Anchor| a.offset;
        let empty = anchor(5)..anchor(5);
        assert!(empty.is_empty(&resolve));

        let inverted = anchor(8)..anchor(3);
        assert!(inverted.is_empty(&resolve));

        let non_empty = anchor(3)..anchor(8);
        assert!(!non_empty.is_empty(&resolve));
    }

    #[test]
    fn range_len() {
        let resolve = |a: &Anchor| a.offset;
        assert_eq!((anchor(3)..anchor(8)).len(&resolve), 5);
        assert_eq!((anchor(5)..anchor(5)).len(&resolve), 0);
        assert_eq!((anchor(8)..anchor(3)).len(&resolve), 0);
    }

    #[test]
    fn range_intersect_overlapping() {
        let resolve = |a: &Anchor| a.offset;
        let r1 = anchor(2)..anchor(8);
        let r2 = anchor(5)..anchor(10);
        assert_eq!(r1.intersect(&r2, &resolve), Some(5..8));
    }

    #[test]
    fn range_intersect_non_overlapping() {
        let resolve = |a: &Anchor| a.offset;
        let r1 = anchor(2)..anchor(5);
        let r2 = anchor(6)..anchor(10);
        assert_eq!(r1.intersect(&r2, &resolve), None);
    }

    #[test]
    fn range_intersect_touching() {
        let resolve = |a: &Anchor| a.offset;
        let r1 = anchor(2)..anchor(5);
        let r2 = anchor(5)..anchor(10);
        assert_eq!(r1.intersect(&r2, &resolve), None);
    }

    #[test]
    fn canonicalize_normal() {
        let resolve = |a: &Anchor| a.offset;
        let range = anchor(3)..anchor(8);
        let canon = range.canonicalize(&resolve);
        assert_eq!(resolve(&canon.start), 3);
        assert_eq!(resolve(&canon.end), 8);
    }

    #[test]
    fn canonicalize_swaps_inverted() {
        let resolve = |a: &Anchor| a.offset;
        let range = anchor(8)..anchor(3);
        let canon = range.canonicalize(&resolve);
        assert_eq!(resolve(&canon.start), 3);
        assert_eq!(resolve(&canon.end), 8);
    }
}
