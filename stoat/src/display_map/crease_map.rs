use crate::display_map::{BlockStyle, FoldPlaceholder};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};
use stoat_text::{Anchor, Point};

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct CreaseId(usize);

pub type RenderToggleFn = Arc<dyn Send + Sync + Fn(u32, bool) -> Option<String>>;
pub type RenderTrailerFn = Arc<dyn Send + Sync + Fn(u32, bool) -> Option<String>>;

#[derive(Clone, Debug)]
pub struct CreaseMetadata {
    pub icon_path: Arc<str>,
    pub label: Arc<str>,
}

#[derive(Clone)]
pub enum Crease<T> {
    Inline {
        range: Range<T>,
        placeholder: FoldPlaceholder,
        render_toggle: Option<RenderToggleFn>,
        render_trailer: Option<RenderTrailerFn>,
        metadata: Option<CreaseMetadata>,
    },
    Block {
        range: Range<T>,
        block_height: u32,
        block_style: BlockStyle,
        block_priority: usize,
        render_toggle: Option<RenderToggleFn>,
    },
}

impl<T: std::fmt::Debug> std::fmt::Debug for Crease<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Crease::Inline {
                range,
                placeholder,
                metadata,
                ..
            } => f
                .debug_struct("Inline")
                .field("range", range)
                .field("placeholder", placeholder)
                .field("metadata", metadata)
                .finish(),
            Crease::Block {
                range,
                block_height,
                block_style,
                block_priority,
                ..
            } => f
                .debug_struct("Block")
                .field("range", range)
                .field("block_height", block_height)
                .field("block_style", block_style)
                .field("block_priority", block_priority)
                .finish(),
        }
    }
}

impl Crease<Anchor> {
    pub fn inline(range: Range<Anchor>, placeholder: FoldPlaceholder) -> Self {
        Crease::Inline {
            range,
            placeholder,
            render_toggle: None,
            render_trailer: None,
            metadata: None,
        }
    }

    pub fn inline_with_metadata(
        range: Range<Anchor>,
        placeholder: FoldPlaceholder,
        metadata: CreaseMetadata,
    ) -> Self {
        Crease::Inline {
            range,
            placeholder,
            render_toggle: None,
            render_trailer: None,
            metadata: Some(metadata),
        }
    }

    pub fn block(range: Range<Anchor>, height: u32, style: BlockStyle, priority: usize) -> Self {
        Crease::Block {
            range,
            block_height: height,
            block_style: style,
            block_priority: priority,
            render_toggle: None,
        }
    }
}

impl<T> Crease<T> {
    pub fn range(&self) -> &Range<T> {
        match self {
            Crease::Inline { range, .. } | Crease::Block { range, .. } => range,
        }
    }

    pub fn placeholder(&self) -> Option<&FoldPlaceholder> {
        match self {
            Crease::Inline { placeholder, .. } => Some(placeholder),
            Crease::Block { .. } => None,
        }
    }

    pub fn metadata(&self) -> Option<&CreaseMetadata> {
        match self {
            Crease::Inline { metadata, .. } => metadata.as_ref(),
            Crease::Block { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
struct CreaseItem {
    id: CreaseId,
    crease: Crease<Anchor>,
}

/// The creases an editor holds, such as a language server's folding ranges.
///
/// The map stores anchors and resolves none of them on a buffer edit, so an
/// edit costs it nothing. A query resolves each crease it reads through the
/// closure its caller hands it, which suits readers that ask on demand. A
/// reader that queries every frame needs an anchor-keyed tree instead.
#[derive(Default)]
pub struct CreaseMap {
    /// The creases in the order of their starts as the last insert found them.
    ///
    /// Anchors keep their relative order across edits, so that order stays
    /// valid with nothing resolved after the insert. Each [`CreaseSnapshot`]
    /// shares the slice, so a snapshot costs a refcount bump.
    items: Arc<[CreaseItem]>,
    next_id: usize,
    id_to_range: HashMap<CreaseId, Range<Anchor>>,
}

impl CreaseMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> CreaseSnapshot {
        CreaseSnapshot {
            items: self.items.clone(),
        }
    }

    /// Add `creases` and return their ids in the order of their starts.
    ///
    /// `resolve` places the starts of the new creases and the held ones, so
    /// the new creases merge into start order. A held crease stays ahead of a
    /// new one with the same start.
    pub fn insert(
        &mut self,
        creases: impl IntoIterator<Item = Crease<Anchor>>,
        resolve: &impl Fn(&Anchor) -> usize,
    ) -> Vec<CreaseId> {
        let mut new_items: Vec<(usize, CreaseItem)> = creases
            .into_iter()
            .map(|crease| {
                let id = CreaseId(self.next_id);
                self.next_id += 1;
                self.id_to_range.insert(id, crease.range().clone());
                (resolve(&crease.range().start), CreaseItem { id, crease })
            })
            .collect();
        new_items.sort_by_key(|(start, _)| *start);

        let new_ids: Vec<CreaseId> = new_items.iter().map(|(_, item)| item.id).collect();

        // Merged rather than appended and re-sorted, since both sides are
        // already in order and the held side is the long one.
        let mut merged = Vec::with_capacity(self.items.len() + new_items.len());
        let mut held = self
            .items
            .iter()
            .map(|item| (resolve(&item.crease.range().start), item))
            .peekable();
        for (start, item) in new_items {
            while let Some((_, existing)) = held.next_if(|(held_start, _)| *held_start <= start) {
                merged.push(existing.clone());
            }
            merged.push(item);
        }
        merged.extend(held.map(|(_, item)| item.clone()));
        self.items = merged.into();

        new_ids
    }

    pub fn remove(
        &mut self,
        ids: impl IntoIterator<Item = CreaseId>,
    ) -> Vec<(CreaseId, Range<Anchor>)> {
        let ids_to_remove: HashSet<CreaseId> = ids.into_iter().collect();
        if ids_to_remove.is_empty() {
            return Vec::new();
        }

        let mut removed = Vec::new();
        for &id in &ids_to_remove {
            if let Some(range) = self.id_to_range.remove(&id) {
                removed.push((id, range));
            }
        }

        self.items = self
            .items
            .iter()
            .filter(|item| !ids_to_remove.contains(&item.id))
            .cloned()
            .collect();

        removed
    }
}

#[derive(Clone)]
pub struct CreaseSnapshot {
    items: Arc<[CreaseItem]>,
}

impl CreaseSnapshot {
    pub fn empty() -> Self {
        Self {
            items: Arc::default(),
        }
    }

    pub fn query_row(
        &self,
        row: u32,
        resolve_to_point: &impl Fn(&Anchor) -> Point,
    ) -> Option<&Crease<Anchor>> {
        for item in self.items.iter() {
            match resolve_to_point(&item.crease.range().start).row.cmp(&row) {
                Ordering::Less => {},
                Ordering::Equal => return Some(&item.crease),
                Ordering::Greater => break,
            }
        }
        None
    }

    pub fn creases_in_range<'a>(
        &'a self,
        range: Range<u32>,
        resolve_to_point: &'a impl Fn(&Anchor) -> Point,
    ) -> impl Iterator<Item = &'a Crease<Anchor>> {
        self.items.iter().filter_map(move |item| {
            let start_row = resolve_to_point(&item.crease.range().start).row;
            let end_row = resolve_to_point(&item.crease.range().end).row;
            (start_row >= range.start && end_row < range.end).then_some(&item.crease)
        })
    }

    pub fn crease_items_with_offsets(
        &self,
        resolve_to_point: &impl Fn(&Anchor) -> Point,
    ) -> Vec<(CreaseId, Range<Point>)> {
        self.items
            .iter()
            .map(|item| {
                let start = resolve_to_point(&item.crease.range().start);
                let end = resolve_to_point(&item.crease.range().end);
                (item.id, start..end)
            })
            .collect()
    }

    pub fn creases(&self) -> impl Iterator<Item = (CreaseId, &Crease<Anchor>)> {
        self.items.iter().map(|item| (item.id, &item.crease))
    }
}

#[cfg(test)]
mod tests {
    use super::{Crease, CreaseMap, FoldPlaceholder, Range};
    use crate::{
        buffer::{BufferId, SharedBuffer, TextBuffer},
        multi_buffer::{MultiBuffer, MultiBufferSnapshot},
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::{Anchor, Bias, Point};

    fn anchor(_timestamp: u64, offset: usize) -> Anchor {
        Anchor {
            timestamp: _timestamp,
            offset: offset as u32,
            bias: Bias::Left,
            buffer_id: None,
        }
    }

    /// A crease map over a real buffer, holding one crease per line body, with
    /// the shared buffer returned so a test can edit it.
    fn map_over(text: &str, ranges: &[Range<usize>]) -> (CreaseMap, SharedBuffer, MultiBuffer) {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), text)));
        let multi = MultiBuffer::singleton(shared.clone());
        let snap = multi.snapshot();

        let mut map = CreaseMap::new();
        map.insert(
            ranges.iter().map(|r| {
                Crease::inline(
                    snap.anchor_at(r.start, Bias::Right)..snap.anchor_at(r.end, Bias::Left),
                    FoldPlaceholder::default(),
                )
            }),
            &|a: &Anchor| snap.resolve_anchor(a),
        );

        (map, shared, multi)
    }

    /// Where each crease of `map` starts and ends in `buffer`, in map order.
    fn starts_and_ends(map: &CreaseMap, buffer: &MultiBufferSnapshot) -> Vec<(usize, usize)> {
        map.items
            .iter()
            .map(|i| {
                let range = i.crease.range();
                (
                    buffer.resolve_anchor(&range.start),
                    buffer.resolve_anchor(&range.end),
                )
            })
            .collect()
    }

    /// The map resolves nothing on an edit, so a query after one reads where the
    /// edit moved a crease through the closure it is handed.
    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn a_query_reads_the_rows_an_insertion_before_a_crease_moved() {
        let text = "line0 body\nline1 body\n";
        let (map, shared, multi) = map_over(text, &[17..21]);

        shared.write().expect("poisoned").edit(0..0, "XY\n");
        let after = multi.snapshot();
        let to_point = |a: &Anchor| after.rope().offset_to_point(after.resolve_anchor(a));
        let snap = map.snapshot();
        let rows_in = |rows: Range<u32>| -> Vec<Range<u32>> {
            snap.creases_in_range(rows, &to_point)
                .map(|crease| {
                    to_point(&crease.range().start).row..to_point(&crease.range().end).row
                })
                .collect()
        };

        assert_eq!(
            (rows_in(1..2), rows_in(2..3), starts_and_ends(&map, &after)),
            (vec![], vec![2..2], vec![(20, 24)]),
            "the crease on row 1 reads as row 2 once a line lands above it",
        );
    }

    #[test]
    fn insert_and_query() {
        let mut map = CreaseMap::new();
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_to_point = |a: &Anchor| Point::new(a.offset, 0);

        let ids = map.insert(
            [Crease::inline(
                anchor(0, 5)..anchor(0, 10),
                FoldPlaceholder::default(),
            )],
            &resolve,
        );
        assert_eq!(ids.len(), 1);

        let snap = map.snapshot();
        assert!(snap.query_row(5, &resolve_to_point).is_some());
        assert!(snap.query_row(0, &resolve_to_point).is_none());
        assert!(snap.query_row(6, &resolve_to_point).is_none());
    }

    #[test]
    fn remove() {
        let mut map = CreaseMap::new();
        let resolve = |a: &Anchor| a.offset as usize;

        let ids = map.insert(
            [
                Crease::inline(anchor(0, 0)..anchor(0, 5), FoldPlaceholder::default()),
                Crease::inline(anchor(0, 10)..anchor(0, 15), FoldPlaceholder::default()),
            ],
            &resolve,
        );

        let snap = map.snapshot();
        assert_eq!(snap.creases().count(), 2);

        let removed = map.remove([ids[0]]);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].0, ids[0]);
        let snap = map.snapshot();
        assert_eq!(snap.creases().count(), 1);
    }

    #[test]
    fn creases_in_range() {
        let mut map = CreaseMap::new();
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_to_point = |a: &Anchor| Point::new(a.offset, 0);

        map.insert(
            [
                Crease::inline(anchor(0, 0)..anchor(0, 3), FoldPlaceholder::default()),
                Crease::inline(anchor(0, 5)..anchor(0, 8), FoldPlaceholder::default()),
                Crease::inline(anchor(0, 10)..anchor(0, 15), FoldPlaceholder::default()),
            ],
            &resolve,
        );

        let snap = map.snapshot();
        let in_range: Vec<_> = snap.creases_in_range(4..12, &resolve_to_point).collect();
        assert_eq!(in_range.len(), 1);
    }

    #[test]
    fn crease_items_with_offsets() {
        let mut map = CreaseMap::new();
        let resolve = |a: &Anchor| a.offset as usize;
        let resolve_to_point = |a: &Anchor| Point::new(a.offset, 0);

        map.insert(
            [
                Crease::inline(anchor(0, 5)..anchor(0, 10), FoldPlaceholder::default()),
                Crease::inline(anchor(0, 15)..anchor(0, 20), FoldPlaceholder::default()),
            ],
            &resolve,
        );

        let snap = map.snapshot();
        let items = snap.crease_items_with_offsets(&resolve_to_point);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].1, Point::new(5, 0)..Point::new(10, 0));
        assert_eq!(items[1].1, Point::new(15, 0)..Point::new(20, 0));
    }
}
