//! Content-hash-keyed cache of pre-computed review hunks.
//!
//! The TUI review pane and the `stoat diff` CLI both feed file
//! contents through [`crate::review::extract_review_hunks_changeset`].
//! When a `stoat diff` invocation hits a running editor over the
//! viewport socket, the editor consults this cache first so the CLI
//! can skip the structural-diff pass for content the editor has
//! already diffed.
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
    #[cfg(test)]
    hits: u64,
    #[cfg(test)]
    misses: u64,
}

impl DiffCache {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0, "DiffCache capacity must be positive");
        Self {
            map: BTreeMap::new(),
            counter: 0,
            cap,
            #[cfg(test)]
            hits: 0,
            #[cfg(test)]
            misses: 0,
        }
    }

    pub fn lookup(&mut self, key: &DiffCacheKey) -> Option<Arc<Vec<ReviewHunk>>> {
        let counter = self.tick();
        let hit = self.map.get_mut(key).map(|entry| {
            entry.last_used = counter;
            entry.hunks.clone()
        });

        #[cfg(test)]
        {
            if hit.is_some() {
                self.hits += 1;
            } else {
                self.misses += 1;
            }
        }

        hit
    }

    /// Total cache hits observed by [`Self::lookup`] since construction.
    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.hits
    }

    /// Total cache misses observed by [`Self::lookup`] since construction.
    #[cfg(test)]
    pub(crate) fn misses(&self) -> u64 {
        self.misses
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

/// Serialize hunks for transmission over the viewport socket. The
/// exact byte format is internal to the editor/client pair and may
/// change between versions; the editor and the `stoat diff` client
/// always run from the same binary so version skew does not occur.
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
