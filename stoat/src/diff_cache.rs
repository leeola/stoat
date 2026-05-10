//! Content-hash-keyed cache of pre-computed review hunks.
//!
//! The review pane feeds file contents through
//! [`crate::review::extract_review_hunks_changeset`] and stores the
//! resulting hunks here so subsequent lookups against the same
//! content (left hash, right hash, language) can skip the
//! structural-diff pass.
//!
//! The cache is bounded by entry count (default 256) with a simple
//! counter-based LRU eviction. Hits and inserts both bump the
//! generation counter; on overflow, the entry with the smallest
//! `last_used` is dropped.

use crate::review::ReviewHunk;
use std::{collections::BTreeMap, sync::Arc};

pub type ContentHash = [u8; 32];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiffCacheKey {
    pub left_hash: ContentHash,
    pub right_hash: ContentHash,
    pub language: Option<String>,
}

struct Entry {
    hunks: Arc<Vec<ReviewHunk>>,
    last_used: u64,
}

pub struct DiffCache {
    map: BTreeMap<DiffCacheKey, Entry>,
    counter: u64,
    cap: usize,
}

impl DiffCache {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0, "DiffCache capacity must be positive");
        Self {
            map: BTreeMap::new(),
            counter: 0,
            cap,
        }
    }

    pub fn lookup(&mut self, key: &DiffCacheKey) -> Option<Arc<Vec<ReviewHunk>>> {
        let counter = self.tick();
        let entry = self.map.get_mut(key)?;
        entry.last_used = counter;
        Some(entry.hunks.clone())
    }

    pub fn insert(&mut self, key: DiffCacheKey, hunks: Arc<Vec<ReviewHunk>>) {
        let counter = self.tick();
        self.map.insert(
            key,
            Entry {
                hunks,
                last_used: counter,
            },
        );
        if self.map.len() > self.cap {
            self.evict_lru();
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn tick(&mut self) -> u64 {
        self.counter = self.counter.wrapping_add(1);
        self.counter
    }

    fn evict_lru(&mut self) {
        let victim = self
            .map
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone());
        if let Some(k) = victim {
            self.map.remove(&k);
        }
    }
}

/// Serialize hunks to a byte payload. The exact byte format is
/// internal and may change between versions.
pub fn serialize_hunks(hunks: &[ReviewHunk]) -> Vec<u8> {
    serde_json::to_vec(hunks).expect("review hunk types are serde-serializable")
}

pub fn deserialize_hunks(bytes: &[u8]) -> serde_json::Result<Vec<ReviewHunk>> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::{ReviewRow, ReviewSide};

    fn key(l: u8, r: u8, lang: Option<&str>) -> DiffCacheKey {
        DiffCacheKey {
            left_hash: [l; 32],
            right_hash: [r; 32],
            language: lang.map(str::to_string),
        }
    }

    fn hunks(text: &str) -> Arc<Vec<ReviewHunk>> {
        Arc::new(vec![ReviewHunk {
            rows: vec![ReviewRow::Context {
                left: ReviewSide {
                    text: text.to_string(),
                    line_num: 1,
                    change_spans: vec![],
                    moved_spans: vec![],
                    move_provenance: None,
                },
                right: ReviewSide {
                    text: text.to_string(),
                    line_num: 1,
                    change_spans: vec![],
                    moved_spans: vec![],
                    move_provenance: None,
                },
            }],
        }])
    }

    #[test]
    fn lookup_miss_returns_none() {
        let mut cache = DiffCache::new(4);
        assert!(cache.lookup(&key(1, 2, None)).is_none());
    }

    #[test]
    fn insert_then_lookup_hits() {
        let mut cache = DiffCache::new(4);
        let h = hunks("hello");
        cache.insert(key(1, 2, Some("rust")), h.clone());
        let got = cache.lookup(&key(1, 2, Some("rust"))).expect("hit");
        assert!(Arc::ptr_eq(&got, &h));
    }

    #[test]
    fn key_distinguishes_language() {
        let mut cache = DiffCache::new(4);
        cache.insert(key(1, 2, Some("rust")), hunks("rs"));
        cache.insert(key(1, 2, Some("toml")), hunks("toml"));
        assert_eq!(cache.len(), 2);
        assert!(cache.lookup(&key(1, 2, None)).is_none());
    }

    #[test]
    fn lru_eviction_drops_oldest() {
        let mut cache = DiffCache::new(2);
        cache.insert(key(1, 1, None), hunks("a"));
        cache.insert(key(2, 2, None), hunks("b"));
        cache.insert(key(3, 3, None), hunks("c"));
        assert_eq!(cache.len(), 2);
        assert!(cache.lookup(&key(1, 1, None)).is_none(), "oldest evicted");
        assert!(cache.lookup(&key(2, 2, None)).is_some());
        assert!(cache.lookup(&key(3, 3, None)).is_some());
    }

    #[test]
    fn lookup_refreshes_lru() {
        let mut cache = DiffCache::new(2);
        cache.insert(key(1, 1, None), hunks("a"));
        cache.insert(key(2, 2, None), hunks("b"));
        cache.lookup(&key(1, 1, None));
        cache.insert(key(3, 3, None), hunks("c"));
        assert!(
            cache.lookup(&key(2, 2, None)).is_none(),
            "key 2 should evict because key 1 was just touched"
        );
        assert!(cache.lookup(&key(1, 1, None)).is_some());
        assert!(cache.lookup(&key(3, 3, None)).is_some());
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn serialize_round_trip() {
        let original = vec![ReviewHunk {
            rows: vec![
                ReviewRow::Context {
                    left: ReviewSide {
                        text: "ctx".into(),
                        line_num: 1,
                        change_spans: vec![],
                        moved_spans: vec![],
                        move_provenance: None,
                    },
                    right: ReviewSide {
                        text: "ctx".into(),
                        line_num: 1,
                        change_spans: vec![],
                        moved_spans: vec![],
                        move_provenance: None,
                    },
                },
                ReviewRow::Changed {
                    left: Some(ReviewSide {
                        text: "old".into(),
                        line_num: 2,
                        change_spans: vec![0..3],
                        moved_spans: vec![],
                        move_provenance: None,
                    }),
                    right: Some(ReviewSide {
                        text: "new".into(),
                        line_num: 2,
                        change_spans: vec![0..3],
                        moved_spans: vec![1..2],
                        move_provenance: Some(crate::review::MoveProvenance {
                            rel_path: "other.rs".into(),
                            line: 7,
                        }),
                    }),
                },
            ],
        }];
        let bytes = serialize_hunks(&original);
        let decoded = deserialize_hunks(&bytes).unwrap();

        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].rows.len(), 2);

        let ReviewRow::Changed { right: Some(r), .. } = &decoded[0].rows[1] else {
            panic!("expected Changed");
        };
        assert_eq!(r.text, "new");
        assert_eq!(r.change_spans, vec![0..3]);
        assert_eq!(r.moved_spans, vec![1..2]);
        assert_eq!(
            r.move_provenance,
            Some(crate::review::MoveProvenance {
                rel_path: "other.rs".into(),
                line: 7
            })
        );
    }
}
