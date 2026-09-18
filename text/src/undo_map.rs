use crate::{Bias, ContextLessSummary, Edit, Item, KeyedItem, SumTree};
use serde::{Deserialize, Serialize};
use std::cmp;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct UndoMapKey {
    edit_id: u64,
    undo_id: u64,
}

impl ContextLessSummary for UndoMapKey {
    fn add_summary(&mut self, summary: &Self) {
        *self = *summary;
    }
}

#[derive(Clone, Copy, Debug)]
struct UndoMapEntry {
    key: UndoMapKey,
    undo_count: u32,
}

impl Item for UndoMapEntry {
    type Summary = UndoMapKey;

    fn summary(&self, _cx: ()) -> UndoMapKey {
        self.key
    }
}

impl KeyedItem for UndoMapEntry {
    type Key = UndoMapKey;

    fn key(&self) -> Self::Key {
        self.key
    }
}

/// Tracks undo/redo state for edit operations.
///
/// Each edit can be undone multiple times. An odd undo count means the edit
/// is currently undone; even means it's active. This supports arbitrary
/// undo/redo cycles.
#[derive(Clone, Default)]
pub struct UndoMap(SumTree<UndoMapEntry>);

impl UndoMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `undo`'s count for every edit it names, in one pass over the map.
    ///
    /// An undo of an insert session names one edit per typed character, and a
    /// separate insert per entry costs a slice and an append of the whole map
    /// each time. `counts` names each edit once, so no two entries of the batch
    /// share a key.
    pub fn insert(&mut self, undo: &UndoOperation) {
        let edits = undo
            .counts
            .iter()
            .map(|(&edit_id, &undo_count)| {
                Edit::Insert(UndoMapEntry {
                    key: UndoMapKey {
                        edit_id,
                        undo_id: undo.timestamp,
                    },
                    undo_count,
                })
            })
            .collect();
        self.0.edit(edits, ());
    }

    pub fn is_undone(&self, edit_id: u64) -> bool {
        self.undo_count(edit_id) % 2 == 1
    }

    pub fn was_undone(&self, edit_id: u64, version: u64) -> bool {
        let mut cursor = self.0.cursor::<UndoMapKey>(());
        cursor.seek(
            &UndoMapKey {
                edit_id,
                undo_id: 0,
            },
            Bias::Left,
        );

        let mut undo_count = 0u32;
        while let Some(entry) = cursor.item() {
            if entry.key.edit_id != edit_id {
                break;
            }
            if entry.key.undo_id <= version {
                undo_count = cmp::max(undo_count, entry.undo_count);
            }
            cursor.next();
        }

        undo_count % 2 == 1
    }

    pub fn undo_count(&self, edit_id: u64) -> u32 {
        let mut cursor = self.0.cursor::<UndoMapKey>(());
        cursor.seek(
            &UndoMapKey {
                edit_id,
                undo_id: 0,
            },
            Bias::Left,
        );

        let mut count = 0u32;
        while let Some(entry) = cursor.item() {
            if entry.key.edit_id != edit_id {
                break;
            }
            count = cmp::max(count, entry.undo_count);
            cursor.next();
        }
        count
    }
}

/// An undo operation that reverses one or more prior edits.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UndoOperation {
    pub timestamp: u64,
    pub counts: std::collections::HashMap<u64, u32>,
}

#[cfg(test)]
mod tests {
    use super::{UndoMap, UndoOperation};
    use std::collections::HashMap;

    #[test]
    fn single_undo() {
        let mut map = UndoMap::new();
        assert!(!map.is_undone(1));
        map.insert(&UndoOperation {
            timestamp: 10,
            counts: HashMap::from([(1, 1)]),
        });
        assert!(map.is_undone(1));
    }

    #[test]
    fn undo_then_redo() {
        let mut map = UndoMap::new();
        map.insert(&UndoOperation {
            timestamp: 10,
            counts: HashMap::from([(1, 1)]),
        });
        assert!(map.is_undone(1));
        map.insert(&UndoOperation {
            timestamp: 11,
            counts: HashMap::from([(1, 2)]),
        });
        assert!(!map.is_undone(1));
    }

    #[test]
    fn was_undone_at_version() {
        let mut map = UndoMap::new();
        map.insert(&UndoOperation {
            timestamp: 10,
            counts: HashMap::from([(1, 1)]),
        });
        assert!(!map.was_undone(1, 5));
        assert!(map.was_undone(1, 10));
        assert!(map.was_undone(1, 15));
    }

    #[test]
    fn was_undone_then_redone() {
        let mut map = UndoMap::new();
        map.insert(&UndoOperation {
            timestamp: 10,
            counts: HashMap::from([(1, 1)]),
        });
        map.insert(&UndoOperation {
            timestamp: 20,
            counts: HashMap::from([(1, 2)]),
        });
        assert!(!map.was_undone(1, 5));
        assert!(map.was_undone(1, 15));
        assert!(!map.was_undone(1, 25));
    }

    #[test]
    fn unrelated_edit_unaffected() {
        let mut map = UndoMap::new();
        map.insert(&UndoOperation {
            timestamp: 10,
            counts: HashMap::from([(1, 1)]),
        });
        assert!(!map.is_undone(2));
        assert!(!map.is_undone(99));
    }

    #[test]
    fn one_operation_records_the_count_of_every_edit_it_names() {
        let mut map = UndoMap::new();
        map.insert(&UndoOperation {
            timestamp: 10,
            counts: HashMap::from([(1, 1), (2, 1), (5, 1)]),
        });
        map.insert(&UndoOperation {
            timestamp: 11,
            counts: HashMap::from([(2, 2), (7, 1)]),
        });

        assert_eq!(
            (
                [1, 2, 3, 5, 7].map(|edit| map.undo_count(edit)),
                [1, 2, 5, 7].map(|edit| map.is_undone(edit)),
            ),
            ([1, 2, 0, 1, 1], [true, false, true, true]),
        );
    }
}
