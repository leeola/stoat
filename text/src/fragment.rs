use crate::{
    ContextLessSummary, Dimension, Item, KeyedItem, Locator, SeekTarget, Summary, UndoMap,
};
use smallvec::SmallVec;
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    pub id: Locator,
    pub timestamp: u64,
    pub insertion_offset: u32,
    pub len: u32,
    pub visible: bool,
    pub deletions: SmallVec<[u64; 2]>,
}

impl Fragment {
    pub fn visible_len(&self) -> usize {
        if self.visible {
            self.len as usize
        } else {
            0
        }
    }

    pub fn deleted_len(&self) -> usize {
        if self.visible {
            0
        } else {
            self.len as usize
        }
    }

    /// Whether the fragment is visible given the current undo state.
    pub fn is_visible_with_undos(&self, undos: &UndoMap) -> bool {
        !undos.is_undone(self.timestamp) && self.deletions.iter().all(|d| undos.is_undone(*d))
    }

    /// Whether the fragment was visible at the given version, considering undos
    /// that had been applied by that version.
    pub fn was_visible(&self, version: u64, undos: &UndoMap) -> bool {
        (self.timestamp <= version && !undos.was_undone(self.timestamp, version))
            && self
                .deletions
                .iter()
                .all(|d| *d > version || undos.was_undone(*d, version))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FragmentTextSummary {
    pub visible: usize,
    pub deleted: usize,
}

impl FragmentTextSummary {
    fn add(&mut self, other: &Self) {
        self.visible += other.visible;
        self.deleted += other.deleted;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FragmentSummary {
    pub text: FragmentTextSummary,
    pub max_id: Locator,
    pub max_version: u64,
}

/// Context for the fragment SumTree: an optional version threshold for
/// filtering queries (used by `edits_since`).
type FragmentContext<'a> = &'a Option<u64>;

impl Summary for FragmentSummary {
    type Context<'a> = FragmentContext<'a>;

    fn zero(_cx: Self::Context<'_>) -> Self {
        Self {
            text: FragmentTextSummary::default(),
            max_id: Locator::min(),
            max_version: 0,
        }
    }

    fn add_summary(&mut self, other: &Self, _cx: Self::Context<'_>) {
        self.text.add(&other.text);
        self.max_id.assign(&other.max_id);
        self.max_version = self.max_version.max(other.max_version);
    }
}

impl Item for Fragment {
    type Summary = FragmentSummary;

    fn summary(&self, _cx: FragmentContext<'_>) -> FragmentSummary {
        FragmentSummary {
            text: FragmentTextSummary {
                visible: self.visible_len(),
                deleted: self.deleted_len(),
            },
            max_id: self.id.clone(),
            max_version: self
                .deletions
                .iter()
                .copied()
                .fold(self.timestamp, u64::max),
        }
    }
}

// Dimension: cumulative visible byte count
impl<'a> Dimension<'a, FragmentSummary> for usize {
    fn zero(_cx: FragmentContext<'_>) -> Self {
        0
    }

    fn add_summary(&mut self, summary: &'a FragmentSummary, _cx: FragmentContext<'_>) {
        *self += summary.text.visible;
    }
}

// Dimension: max fragment Locator (for seeking by fragment ID)
impl<'a> Dimension<'a, FragmentSummary> for Option<Locator> {
    fn zero(_cx: FragmentContext<'_>) -> Self {
        None
    }

    fn add_summary(&mut self, summary: &'a FragmentSummary, _cx: FragmentContext<'_>) {
        *self = Some(summary.max_id.clone());
    }
}

// SeekTarget: seek to a specific Locator in the fragment tree
impl<'a> SeekTarget<'a, FragmentSummary, Option<Locator>> for Option<&Locator> {
    fn cmp(&self, cursor_location: &Option<Locator>, _cx: FragmentContext<'_>) -> Ordering {
        match (self, cursor_location) {
            (Some(target), Some(loc)) => Ord::cmp(*target, loc),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        }
    }
}

// ---- InsertionFragment ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InsertionFragment {
    pub timestamp: u64,
    pub split_offset: u32,
    pub fragment_id: Locator,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InsertionFragmentKey {
    pub timestamp: u64,
    pub split_offset: u32,
}

impl ContextLessSummary for InsertionFragmentKey {
    fn add_summary(&mut self, summary: &Self) {
        *self = *summary;
    }
}

impl Item for InsertionFragment {
    type Summary = InsertionFragmentKey;

    fn summary(&self, _cx: ()) -> InsertionFragmentKey {
        InsertionFragmentKey {
            timestamp: self.timestamp,
            split_offset: self.split_offset,
        }
    }
}

impl KeyedItem for InsertionFragment {
    type Key = InsertionFragmentKey;

    fn key(&self) -> Self::Key {
        InsertionFragmentKey {
            timestamp: self.timestamp,
            split_offset: self.split_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Fragment, FragmentTextSummary, InsertionFragment, InsertionFragmentKey};
    use crate::Locator;
    use smallvec::SmallVec;

    #[test]
    fn fragment_visible_len() {
        let f = Fragment {
            id: Locator::min(),
            timestamp: 1,
            insertion_offset: 0,
            len: 10,
            visible: true,
            deletions: SmallVec::new(),
        };
        assert_eq!(f.visible_len(), 10);
        assert_eq!(f.deleted_len(), 0);
    }

    #[test]
    fn fragment_deleted_len() {
        let f = Fragment {
            id: Locator::min(),
            timestamp: 1,
            insertion_offset: 0,
            len: 10,
            visible: false,
            deletions: SmallVec::new(),
        };
        assert_eq!(f.visible_len(), 0);
        assert_eq!(f.deleted_len(), 10);
    }

    #[test]
    fn text_summary_add() {
        let mut a = FragmentTextSummary {
            visible: 5,
            deleted: 3,
        };
        let b = FragmentTextSummary {
            visible: 10,
            deleted: 2,
        };
        a.add(&b);
        assert_eq!(a.visible, 15);
        assert_eq!(a.deleted, 5);
    }

    #[test]
    fn insertion_fragment_key_ordering() {
        let a = InsertionFragmentKey {
            timestamp: 1,
            split_offset: 5,
        };
        let b = InsertionFragmentKey {
            timestamp: 1,
            split_offset: 10,
        };
        let c = InsertionFragmentKey {
            timestamp: 2,
            split_offset: 0,
        };
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn insertion_fragment_key() {
        use crate::KeyedItem;
        let frag = InsertionFragment {
            timestamp: 42,
            split_offset: 7,
            fragment_id: Locator::min(),
        };
        let key = frag.key();
        assert_eq!(key.timestamp, 42);
        assert_eq!(key.split_offset, 7);
    }
}
