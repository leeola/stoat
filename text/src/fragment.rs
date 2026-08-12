use crate::{ContextLessSummary, Dimension, Item, KeyedItem, Locator, Summary, UndoMap};
use smallvec::SmallVec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    pub id: Locator,
    pub timestamp: u64,
    pub insertion_offset: u32,
    pub len: u32,
    pub visible: bool,
    pub deletions: SmallVec<[u64; 2]>,
    /// Version at which an undo or redo last toggled this fragment's
    /// visibility, or 0 if none ever did. Folded into
    /// [`FragmentSummary::max_version`] so a visibility flip counts as a change
    /// for `edits_since`. Without it a toggled fragment carries no newer
    /// timestamp, so the incremental-diff filter would skip it.
    pub max_undos: u64,
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
                .fold(self.timestamp, u64::max)
                .max(self.max_undos),
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

/// The last fragment id folded in, which seeks the tree by fragment id.
///
/// The dimension borrows the summary's id rather than owning a copy. A seek
/// clones its running position once per scanned child and several times per
/// level, and a [`Locator`] heap-allocates past depth 2. Borrowed, the
/// dimension is [`Copy`], so those clones become moves.
///
/// The blanket [`crate::SeekTarget`] impl over an [`Ord`] dimension serves
/// this one, so seeking a fragment id needs no impl of its own. That blanket
/// orders `None` below every `Some`, which is what a fragment tree seek
/// expects.
impl<'a> Dimension<'a, FragmentSummary> for Option<&'a Locator> {
    fn zero(_cx: FragmentContext<'_>) -> Self {
        None
    }

    fn add_summary(&mut self, summary: &'a FragmentSummary, _cx: FragmentContext<'_>) {
        *self = Some(&summary.max_id);
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
    use super::{
        Fragment, FragmentSummary, FragmentTextSummary, InsertionFragment, InsertionFragmentKey,
    };
    use crate::{Item, Locator, Summary};
    use smallvec::SmallVec;

    fn frag(id: Locator) -> Fragment {
        Fragment {
            id,
            timestamp: 1,
            insertion_offset: 0,
            len: 1,
            visible: true,
            deletions: SmallVec::new(),
            max_undos: 0,
        }
    }

    #[test]
    fn fragment_visible_len() {
        let f = Fragment {
            id: Locator::min(),
            timestamp: 1,
            insertion_offset: 0,
            len: 10,
            visible: true,
            deletions: SmallVec::new(),
            max_undos: 0,
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
            max_undos: 0,
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
    fn max_id_carries_the_last_summary_folded_in_not_the_greatest() {
        // The name says maximum, and over a fragment tree the two read alike,
        // since the tree is ordered by id and the last id is the greatest. The
        // buffer takes the root summary as its last fragment's id, which only
        // the assignment gives it. A maximum would answer an interior id for a
        // tree whose ids ever ran the other way.
        let folded = |ids: [Locator; 2]| -> Locator {
            let mut summary = FragmentSummary::zero(&None);
            for id in ids {
                summary.add_summary(&frag(id).summary(&None), &None);
            }
            summary.max_id
        };

        assert_eq!(folded([Locator::min(), Locator::max()]), Locator::max());
        assert_eq!(
            folded([Locator::max(), Locator::min()]),
            Locator::min(),
            "folding a smaller id last leaves it holding, rather than the larger",
        );
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
