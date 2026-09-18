use crate::{Bias, ContextLessSummary, Edit, Item, KeyedItem, SumTree};
use serde::{Deserialize, Serialize};

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
/// Each edit toggles any number of times. An odd undo count means the edit is
/// undone, and an even count means it is applied.
///
/// The map has one writer, the buffer's undo pass. It stamps each operation
/// with a strictly increasing timestamp and gives each edit a count one above
/// its current count, so an edit's counts rise with its undo ids. The last
/// entry at or before a version then holds the greatest count up to that
/// version, and each lookup reads that one entry and not every toggle the edit
/// took. A path that applies undo operations out of timestamp order, such as a
/// remote replica's, breaks that order and must walk the entries again.
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
        debug_assert!(
            undo.counts
                .iter()
                .all(|(&edit_id, &count)| count > self.undo_count(edit_id)),
            "an undo operation must raise the count of every edit it names",
        );
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
                undo_id: version,
            },
            Bias::Right,
        );
        cursor
            .prev_item()
            .is_some_and(|entry| entry.key.edit_id == edit_id && entry.undo_count % 2 == 1)
    }

    pub fn undo_count(&self, edit_id: u64) -> u32 {
        let mut cursor = self.0.cursor::<UndoMapKey>(());
        cursor.seek(
            &UndoMapKey {
                edit_id,
                undo_id: u64::MAX,
            },
            Bias::Right,
        );
        cursor
            .prev_item()
            .filter(|entry| entry.key.edit_id == edit_id)
            .map_or(0, |entry| entry.undo_count)
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

    /// The greatest count `edit_id` held at or before `version`, from a walk over
    /// every entry the map holds.
    fn walked(map: &UndoMap, edit_id: u64, version: u64) -> u32 {
        map.0
            .iter()
            .filter(|entry| entry.key.edit_id == edit_id && entry.key.undo_id <= version)
            .map(|entry| entry.undo_count)
            .max()
            .unwrap_or(0)
    }

    /// Groups written the way the buffer writes them, each count one above the
    /// edit's current count, give the one-entry lookups the answers a walk over
    /// every entry gives, at any version.
    #[test]
    fn the_last_entry_answers_what_the_walk_answers() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut map = UndoMap::new();
        for timestamp in 1..=400 {
            let counts: HashMap<u64, u32> = (0..1 + next() % 8)
                .map(|_| next() % 200)
                .map(|edit| (edit, walked(&map, edit, u64::MAX) + 1))
                .collect();
            map.insert(&UndoOperation { timestamp, counts });
        }

        let mismatches: Vec<(u64, u64)> = (0..=401)
            .step_by(7)
            .flat_map(|version| (0..200).map(move |edit| (edit, version)))
            .filter(|&(edit, version)| {
                map.was_undone(edit, version) != (walked(&map, edit, version) % 2 == 1)
                    || map.undo_count(edit) != walked(&map, edit, u64::MAX)
            })
            .collect();
        assert_eq!(mismatches, []);
    }

    /// One operation over a long group lands every edit it names at its own
    /// timestamp and none of them a version earlier.
    #[test]
    fn one_insert_lands_every_edit_of_a_group() {
        let mut map = UndoMap::new();
        map.insert(&UndoOperation {
            timestamp: 5000,
            counts: (0..3000).map(|edit| (edit, 1)).collect(),
        });

        let undone_at = |version| {
            (0..3000)
                .filter(|&edit| map.was_undone(edit, version))
                .count()
        };
        assert_eq!((undone_at(5000), undone_at(4999)), (3000, 0));
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
