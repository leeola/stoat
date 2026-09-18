use crate::{ContextLessSummary, Item, KeyedItem};
use smallvec::SmallVec;
use std::iter;

/// Fractional position identifier for stable ordering in a collection.
///
/// Allows inserting between existing positions without renumbering via
/// [`Locator::between`]. Initial positions should use
/// `Locator::between(Locator::min(), Locator::max())`.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Locator(SmallVec<[u64; 2]>);

impl Clone for Locator {
    fn clone(&self) -> Self {
        Self(SmallVec::from_slice(&self.0))
    }

    fn clone_from(&mut self, source: &Self) {
        self.0.clone_from(&source.0);
    }
}

impl Locator {
    pub fn min() -> Self {
        Self(SmallVec::from_buf_and_len([u64::MIN; 2], 1))
    }

    pub fn max() -> Self {
        Self(SmallVec::from_buf_and_len([u64::MAX; 2], 1))
    }

    pub fn min_ref() -> &'static Self {
        use std::sync::LazyLock;
        static MIN: LazyLock<Locator> = LazyLock::new(Locator::min);
        &MIN
    }

    pub fn max_ref() -> &'static Self {
        use std::sync::LazyLock;
        static MAX: LazyLock<Locator> = LazyLock::new(Locator::max);
        &MAX
    }

    pub fn assign(&mut self, other: &Self) {
        self.0.resize(other.0.len(), 0);
        self.0.copy_from_slice(&other.0);
    }

    /// Produces a locator strictly between `lhs` and `rhs`.
    ///
    /// On a level where `lhs` has a component, the step is 1/65536 of the gap,
    /// which leaves room to the right for the next id. Forward typing uses that
    /// room one character at a time, so the ids of text typed at a cursor stay
    /// at one depth.
    ///
    /// On a level past the end of `lhs`, the step is half the gap. A
    /// replacement of the span the previous edit inserted lands there each
    /// time, between the same left neighbour and a newer right one. Halving
    /// descends one level per 63 such replacements, where the small step
    /// descends one level per replacement.
    pub fn between(lhs: &Self, rhs: &Self) -> Self {
        let lhs_len = lhs.0.len();
        let lhs = lhs.0.iter().copied().chain(iter::repeat(u64::MIN));
        let rhs = rhs.0.iter().copied().chain(iter::repeat(u64::MAX));
        let mut location = SmallVec::new();
        for (level, (lhs, rhs)) in lhs.zip(rhs).enumerate() {
            let gap = rhs.saturating_sub(lhs);
            let step = if level < lhs_len { gap >> 48 } else { gap / 2 };
            let mid = lhs + step;
            location.push(mid);
            if mid > lhs {
                break;
            }
        }
        Self(location)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Locator {
    fn default() -> Self {
        Self::min()
    }
}

impl Item for Locator {
    type Summary = Locator;

    fn summary(&self, _cx: ()) -> Self::Summary {
        self.clone()
    }
}

impl KeyedItem for Locator {
    type Key = Locator;

    fn key(&self) -> Self::Key {
        self.clone()
    }
}

impl ContextLessSummary for Locator {
    fn add_summary(&mut self, summary: &Self) {
        self.assign(summary);
    }
}

#[cfg(test)]
mod tests {
    use super::Locator;

    #[test]
    fn between_produces_value_in_range() {
        let lhs = Locator::min();
        let rhs = Locator::max();
        let mid = Locator::between(&lhs, &rhs);
        assert!(mid > lhs);
        assert!(mid < rhs);
    }

    #[test]
    fn sequential_forward_append_stays_at_depth_1() {
        let mut prev = Locator::min();
        let max = Locator::max();
        for _ in 0..100_000 {
            let loc = Locator::between(&prev, &max);
            assert_eq!(loc.len(), 1, "sequential forward append grew past depth 1");
            prev = loc;
        }
    }

    #[test]
    fn typing_at_cursor_stays_at_depth_2() {
        let initial = Locator::between(&Locator::min(), &Locator::max());
        let prefix = Locator::between(&Locator::min(), &initial);
        assert_eq!(prefix.len(), 2);

        let suffix_id = initial;
        let mut prev = prefix;
        for _ in 0..10_000 {
            let loc = Locator::between(&prev, &suffix_id);
            assert_eq!(loc.len(), 2, "forward typing after split grew past depth 2");
            prev = loc;
        }
    }

    /// Each replacement of the span the previous one inserted sits between the
    /// same left neighbour and that newer id.
    #[test]
    fn replacing_one_span_repeatedly_stays_shallow() {
        let left = Locator::between(&Locator::min(), &Locator::max());
        let mut right = Locator::between(&left, &Locator::max());
        for round in 0..1_000 {
            let next = Locator::between(&left, &right);
            assert!(
                left < next && next < right,
                "round {round}: the id lands between its neighbours"
            );
            right = next;
        }
        assert_eq!(
            right.len(),
            17,
            "one level per 63 replacements past the first, not one per replacement"
        );
    }

    #[test]
    fn min_less_than_max() {
        assert!(Locator::min() < Locator::max());
    }

    #[test]
    fn default_is_min() {
        assert_eq!(Locator::default(), Locator::min());
    }

    #[test]
    fn clone_roundtrip() {
        let a = Locator::between(&Locator::min(), &Locator::max());
        let b = a.clone();
        assert_eq!(a, b);
    }
}
