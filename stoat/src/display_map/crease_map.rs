use crate::display_map::{BlockStyle, FoldPlaceholder};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};
use stoat_text::{Anchor, ContextLessSummary, Dimension, Item, Point, SumTree};

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
    resolved_start: usize,
    resolved_end: usize,
}

#[derive(Clone, Debug, Default)]
struct CreaseItemSummary {
    resolved_start: usize,
    count: usize,
    min_start: usize,
    max_end: usize,
}

impl ContextLessSummary for CreaseItemSummary {
    fn add_summary(&mut self, other: &Self) {
        if other.count > 0 {
            if self.count == 0 {
                self.min_start = other.min_start;
            } else {
                self.min_start = self.min_start.min(other.min_start);
            }
            self.resolved_start = other.resolved_start;
            self.max_end = self.max_end.max(other.max_end);
            self.count += other.count;
        }
    }
}

impl Item for CreaseItem {
    type Summary = CreaseItemSummary;

    fn summary(&self, _cx: ()) -> CreaseItemSummary {
        CreaseItemSummary {
            resolved_start: self.resolved_start,
            count: 1,
            min_start: self.resolved_start,
            max_end: self.resolved_end,
        }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CreaseStartOffset(usize);

impl<'a> Dimension<'a, CreaseItemSummary> for CreaseStartOffset {
    fn zero(_cx: ()) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, s: &'a CreaseItemSummary, _cx: ()) {
        if s.count > 0 {
            self.0 = s.resolved_start;
        }
    }
}

pub struct CreaseMap {
    creases: SumTree<CreaseItem>,
    next_id: usize,
    id_to_range: HashMap<CreaseId, Range<Anchor>>,
}

impl Default for CreaseMap {
    fn default() -> Self {
        Self {
            creases: SumTree::new(()),
            next_id: 0,
            id_to_range: HashMap::new(),
        }
    }
}

impl CreaseMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> CreaseSnapshot {
        CreaseSnapshot {
            items: Arc::new(self.creases.clone()),
        }
    }

    pub fn insert(
        &mut self,
        creases: impl IntoIterator<Item = Crease<Anchor>>,
        resolve: &impl Fn(&Anchor) -> usize,
    ) -> Vec<CreaseId> {
        let mut new_items: Vec<CreaseItem> = creases
            .into_iter()
            .map(|crease| {
                let resolved_start = resolve(&crease.range().start);
                let resolved_end = resolve(&crease.range().end);
                let id = CreaseId(self.next_id);
                self.next_id += 1;
                self.id_to_range.insert(id, crease.range().clone());
                CreaseItem {
                    id,
                    crease,
                    resolved_start,
                    resolved_end,
                }
            })
            .collect();
        new_items.sort_by_key(|item| item.resolved_start);

        let new_ids: Vec<CreaseId> = new_items.iter().map(|item| item.id).collect();

        let new_tree = {
            let mut tree = SumTree::new(());
            let mut cursor = self.creases.cursor::<CreaseStartOffset>(());

            for item in new_items {
                tree.append(
                    cursor.slice(
                        &CreaseStartOffset(item.resolved_start),
                        stoat_text::Bias::Left,
                    ),
                    (),
                );
                tree.push(item, ());
            }
            tree.append(cursor.suffix(), ());
            tree
        };
        self.creases = new_tree;

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

        let items: Vec<CreaseItem> = self
            .creases
            .iter()
            .filter(|item| !ids_to_remove.contains(&item.id))
            .cloned()
            .collect();
        self.creases = SumTree::from_iter(items, ());

        removed
    }

    pub fn sync(&mut self, resolve: &impl Fn(&Anchor) -> usize) {
        let mut items: Vec<CreaseItem> = self
            .creases
            .iter()
            .cloned()
            .map(|mut item| {
                item.resolved_start = resolve(&item.crease.range().start);
                item.resolved_end = resolve(&item.crease.range().end);
                item
            })
            .collect();
        items.sort_by_key(|c| c.resolved_start);
        self.creases = SumTree::from_iter(items, ());
    }
}

#[derive(Clone)]
pub struct CreaseSnapshot {
    items: Arc<SumTree<CreaseItem>>,
}

impl CreaseSnapshot {
    pub fn empty() -> Self {
        Self {
            items: Arc::new(SumTree::new(())),
        }
    }

    pub fn query_row(
        &self,
        row: u32,
        row_start_offset: usize,
        resolve_to_point: &impl Fn(&Anchor) -> Point,
    ) -> Option<&Crease<Anchor>> {
        let mut cursor = self.items.cursor::<CreaseStartOffset>(());
        cursor.seek(&CreaseStartOffset(row_start_offset), stoat_text::Bias::Left);
        while let Some(item) = cursor.item() {
            let start_row = resolve_to_point(&item.crease.range().start).row;
            match start_row.cmp(&row) {
                Ordering::Less => {
                    cursor.next();
                },
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
        let mut cursor = self.items.cursor::<CreaseStartOffset>(());
        cursor.seek(&CreaseStartOffset(0), stoat_text::Bias::Left);
        std::iter::from_fn(move || {
            while let Some(item) = cursor.item() {
                cursor.next();
                let start_row = resolve_to_point(&item.crease.range().start).row;
                let end_row = resolve_to_point(&item.crease.range().end).row;
                if start_row >= range.start && end_row < range.end {
                    return Some(&item.crease);
                }
            }
            None
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
    use super::{Crease, CreaseMap, FoldPlaceholder};
    use stoat_text::{Anchor, Bias, Point};

    fn anchor(_timestamp: u64, offset: usize) -> Anchor {
        Anchor {
            timestamp: _timestamp,
            offset: offset as u32,
            bias: Bias::Left,
            buffer_id: None,
        }
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
        assert!(snap.query_row(5, 5, &resolve_to_point).is_some());
        assert!(snap.query_row(0, 0, &resolve_to_point).is_none());
        assert!(snap.query_row(6, 6, &resolve_to_point).is_none());
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
    fn sync_reorders() {
        let mut map = CreaseMap::new();
        let resolve_initial = |a: &Anchor| a.offset as usize;

        map.insert(
            [
                Crease::inline(anchor(0, 5)..anchor(0, 10), FoldPlaceholder::default()),
                Crease::inline(anchor(0, 20)..anchor(0, 25), FoldPlaceholder::default()),
            ],
            &resolve_initial,
        );

        let resolve_after = |a: &Anchor| {
            if a.offset == 20 {
                2
            } else if a.offset == 25 {
                7
            } else {
                a.offset as usize
            }
        };
        map.sync(&resolve_after);

        let snap = map.snapshot();
        let offsets: Vec<usize> = snap.items.iter().map(|i| i.resolved_start).collect();
        assert_eq!(offsets, vec![2, 5]);
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
