//! CreaseMap: Metadata layer for foldable regions.
//!
//! CreaseMap is NOT a transformation layer - it provides metadata about foldable
//! regions without modifying coordinates. It tracks two types of creases:
//!
//! - **Inline creases**: Foldable regions that can be collapsed with a placeholder
//! - **Block creases**: Regions with custom block rendering (code lens, diagnostics)
//!
//! # Coordinate Stability
//!
//! Creases use [`Anchor`]-based positioning to remain stable across buffer edits.
//! When queried, anchors are resolved to the current coordinate system (e.g., FoldPoint).
//!
//! # Architecture
//!
//! ```text
//! CreaseMap (mutable)
//!   - next_id: CreaseId counter
//!   - id_to_range: HashMap<CreaseId, Range<Anchor>>
//!   - snapshot: CreaseSnapshot
//!
//! CreaseSnapshot (immutable)
//!   - creases: SumTree<CreaseItem>
//!     - sorted by buffer position
//! ```
//!
//! # Usage
//!
//! ```ignore
//! let mut crease_map = CreaseMap::new(buffer_snapshot);
//!
//! // Insert inline crease (e.g., folded function)
//! let id = crease_map.insert(
//!     anchor_range,
//!     Crease::Inline { placeholder: "...".into() }
//! );
//!
//! // Query creases in a range
//! let snapshot = crease_map.snapshot();
//! for crease in snapshot.creases_in_range(range) {
//!     // Process crease metadata
//! }
//! ```

use std::{collections::HashMap, ops::Range};
use sum_tree::{Item, SumTree};
use text::{Anchor, BufferSnapshot, ToOffset};

/// Unique identifier for a crease.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct CreaseId(usize);

/// Metadata about a foldable or block region.
///
/// Creases are parameterized by coordinate type `T` to support queries
/// in different coordinate systems (Anchor, FoldPoint, etc.).
#[derive(Clone, Debug)]
pub enum Crease<T> {
    /// Inline crease: foldable region with optional placeholder text.
    ///
    /// When folded, the range is replaced with the placeholder (e.g., "...").
    Inline {
        range: Range<T>,
        placeholder: Option<String>,
    },

    /// Block crease: region with custom block rendering.
    ///
    /// Examples: code lens, git blame, diagnostics.
    Block {
        range: Range<T>,
        height: u32,
        priority: usize,
    },
}

impl<T> Crease<T> {
    /// Get the range of this crease.
    pub fn range(&self) -> &Range<T> {
        match self {
            Crease::Inline { range, .. } => range,
            Crease::Block { range, .. } => range,
        }
    }

    /// Map the coordinate type using a conversion function.
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> Crease<U> {
        match self {
            Crease::Inline { range, placeholder } => Crease::Inline {
                range: f(range.start)..f(range.end),
                placeholder,
            },
            Crease::Block {
                range,
                height,
                priority,
            } => Crease::Block {
                range: f(range.start)..f(range.end),
                height,
                priority,
            },
        }
    }
}

/// Internal item stored in the SumTree.
///
/// Each item represents one crease, positioned by its buffer anchor.
#[derive(Clone, Debug)]
struct CreaseItem {
    id: CreaseId,
    crease: Crease<Anchor>,
}

/// Summary for aggregating CreaseItem trees.
///
/// Tracks the buffer extent covered by the subtree.
#[derive(Clone, Debug, Default)]
struct ItemSummary {
    /// Count of creases in this subtree.
    count: usize,
}

impl sum_tree::ContextLessSummary for ItemSummary {
    fn zero() -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self) {
        self.count += other.count;
    }
}

impl Item for CreaseItem {
    type Summary = ItemSummary;

    fn summary(&self, _cx: ()) -> Self::Summary {
        ItemSummary { count: 1 }
    }
}

/// Mutable crease map.
///
/// Manages crease insertion, removal, and snapshot creation.
pub struct CreaseMap {
    snapshot: CreaseSnapshot,
    next_id: CreaseId,
    id_to_range: HashMap<CreaseId, Range<Anchor>>,
}

impl CreaseMap {
    /// Create a new crease map with no creases.
    pub fn new(buffer: BufferSnapshot) -> Self {
        Self {
            snapshot: CreaseSnapshot::new(buffer),
            next_id: CreaseId(0),
            id_to_range: HashMap::new(),
        }
    }

    /// Insert a new crease and return its ID.
    ///
    /// The crease is inserted into the snapshot's SumTree, maintaining sorted order.
    pub fn insert(&mut self, range: Range<Anchor>, crease: Crease<Anchor>) -> CreaseId {
        let id = self.next_id;
        self.next_id = CreaseId(id.0 + 1);

        self.id_to_range.insert(id, range.clone());

        // Build new SumTree with inserted crease
        let new_item = CreaseItem {
            id,
            crease: match crease {
                Crease::Inline { placeholder, .. } => Crease::Inline {
                    range: range.clone(),
                    placeholder,
                },
                Crease::Block {
                    height, priority, ..
                } => Crease::Block {
                    range: range.clone(),
                    height,
                    priority,
                },
            },
        };

        let mut new_creases = Vec::new();
        let insert_offset = range.start.to_offset(&self.snapshot.buffer);
        let mut inserted = false;

        for item in self.snapshot.creases.iter() {
            let item_start_offset = item.crease.range().start.to_offset(&self.snapshot.buffer);

            if !inserted && item_start_offset >= insert_offset {
                new_creases.push(new_item.clone());
                inserted = true;
            }

            new_creases.push(item.clone());
        }

        // If we haven't inserted yet, add at the end
        if !inserted {
            new_creases.push(new_item);
        }

        self.snapshot.creases = SumTree::from_iter(new_creases, ());

        id
    }

    /// Remove a crease by ID.
    ///
    /// Returns true if the crease was found and removed.
    pub fn remove(&mut self, id: CreaseId) -> bool {
        if self.id_to_range.remove(&id).is_none() {
            return false;
        }

        // Rebuild tree without the removed item
        let new_creases: Vec<_> = self
            .snapshot
            .creases
            .iter()
            .filter(|item| item.id != id)
            .cloned()
            .collect();

        self.snapshot.creases = SumTree::from_iter(new_creases, ());
        true
    }

    /// Get an immutable snapshot of the current crease state.
    pub fn snapshot(&self) -> CreaseSnapshot {
        self.snapshot.clone()
    }

    /// Update the buffer snapshot for anchor resolution.
    pub fn set_buffer(&mut self, buffer: BufferSnapshot) {
        self.snapshot.buffer = buffer;
    }
}

/// Immutable snapshot of crease state.
///
/// Cheap to clone (Arc-based buffer snapshot, persistent SumTree).
#[derive(Clone)]
pub struct CreaseSnapshot {
    buffer: BufferSnapshot,
    creases: SumTree<CreaseItem>,
}

impl CreaseSnapshot {
    /// Create a new empty snapshot.
    fn new(buffer: BufferSnapshot) -> Self {
        Self {
            buffer,
            creases: SumTree::new(()),
        }
    }

    /// Query creases intersecting a range of buffer points.
    ///
    /// Returns creases in sorted buffer order.
    pub fn creases_in_range(
        &self,
        range: Range<text::Point>,
    ) -> impl Iterator<Item = Crease<text::Point>> + '_ {
        let start_offset = self.buffer.point_to_offset(range.start);
        let end_offset = self.buffer.point_to_offset(range.end);

        self.creases.iter().filter_map(move |item| {
            let crease_start_offset = item.crease.range().start.to_offset(&self.buffer);
            let crease_end_offset = item.crease.range().end.to_offset(&self.buffer);

            // Check if crease intersects the range
            if crease_start_offset < end_offset && crease_end_offset > start_offset {
                let crease = item.crease.clone();
                // Convert anchor range to Point range
                let point_crease = crease
                    .map(|anchor| self.buffer.offset_to_point(anchor.to_offset(&self.buffer)));
                Some(point_crease)
            } else {
                None
            }
        })
    }

    /// Get all creases in buffer order.
    pub fn all_creases(&self) -> impl Iterator<Item = Crease<Anchor>> + '_ {
        self.creases.iter().map(|item| item.crease.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> BufferSnapshot {
        let buffer = Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text);
        buffer.snapshot()
    }

    #[test]
    fn empty_crease_map() {
        let buffer = create_buffer("hello world");
        let crease_map = CreaseMap::new(buffer);

        assert_eq!(crease_map.snapshot().all_creases().count(), 0);
    }

    #[test]
    fn insert_inline_crease() {
        let buffer = create_buffer("fn main() {\n    println!(\"hello\");\n}");
        let mut crease_map = CreaseMap::new(buffer.clone());

        let start = buffer.anchor_before(0);
        let end = buffer.anchor_after(buffer.len());

        let id = crease_map.insert(
            start..end,
            Crease::Inline {
                range: start..end,
                placeholder: Some("...".to_string()),
            },
        );

        assert_eq!(id, CreaseId(0));
        assert_eq!(crease_map.snapshot().all_creases().count(), 1);
    }

    #[test]
    fn insert_multiple_creases() {
        let buffer = create_buffer("line 1\nline 2\nline 3");
        let mut crease_map = CreaseMap::new(buffer.clone());

        let range1 = buffer.anchor_before(0)..buffer.anchor_after(6);
        let range2 = buffer.anchor_before(7)..buffer.anchor_after(13);

        let id1 = crease_map.insert(
            range1.clone(),
            Crease::Inline {
                range: range1,
                placeholder: None,
            },
        );
        let id2 = crease_map.insert(
            range2.clone(),
            Crease::Block {
                range: range2,
                height: 2,
                priority: 0,
            },
        );

        assert_eq!(id1, CreaseId(0));
        assert_eq!(id2, CreaseId(1));
        assert_eq!(crease_map.snapshot().all_creases().count(), 2);
    }

    #[test]
    fn remove_crease() {
        let buffer = create_buffer("hello world");
        let mut crease_map = CreaseMap::new(buffer.clone());

        let range = buffer.anchor_before(0)..buffer.anchor_after(5);
        let id = crease_map.insert(
            range.clone(),
            Crease::Inline {
                range,
                placeholder: None,
            },
        );

        assert_eq!(crease_map.snapshot().all_creases().count(), 1);

        assert!(crease_map.remove(id));
        assert_eq!(crease_map.snapshot().all_creases().count(), 0);
    }

    #[test]
    fn remove_nonexistent_crease() {
        let buffer = create_buffer("hello world");
        let mut crease_map = CreaseMap::new(buffer);

        assert!(!crease_map.remove(CreaseId(999)));
    }

    #[test]
    fn crease_map_conversion() {
        let crease = Crease::Inline {
            range: 0..10,
            placeholder: Some("...".to_string()),
        };

        let converted = crease.map(|x| x * 2);

        match converted {
            Crease::Inline { range, placeholder } => {
                assert_eq!(range.start, 0);
                assert_eq!(range.end, 20);
                assert_eq!(placeholder, Some("...".to_string()));
            },
            _ => panic!("Expected Inline crease"),
        }
    }
}
