use crate::{
    display_map::{BlockStyle, FoldPlaceholder},
    multi_buffer::MultiBufferSnapshot,
};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};
use stoat_text::{
    Anchor, AnchorRangeExt, Bias, Dimension, Item, Point, SeekTarget, SumTree, Summary,
};

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

/// Summary of a span of [`CreaseItem`]s, keyed by [`Anchor`] and resolved lazily
/// against the buffer passed as context. `start`/`end` carry the last crease's
/// range for the [`CreaseRange`] dimension. Because comparison is deferred to
/// query time, buffer edits never rebuild the tree -- only insert/remove do.
#[derive(Clone, Debug)]
struct CreaseSummary {
    start: Anchor,
    end: Anchor,
    count: usize,
}

impl Default for CreaseSummary {
    fn default() -> Self {
        Self {
            start: Anchor::min(),
            end: Anchor::min(),
            count: 0,
        }
    }
}

impl Summary for CreaseSummary {
    type Context<'a> = &'a MultiBufferSnapshot;

    fn zero(_cx: Self::Context<'_>) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self, _cx: Self::Context<'_>) {
        if other.count == 0 {
            return;
        }
        self.start = other.start;
        self.end = other.end;
        self.count += other.count;
    }
}

impl Item for CreaseItem {
    type Summary = CreaseSummary;

    fn summary(&self, _cx: &MultiBufferSnapshot) -> CreaseSummary {
        CreaseSummary {
            start: self.crease.range().start,
            end: self.crease.range().end,
            count: 1,
        }
    }
}

/// Anchor-range dimension and seek target over [`CreaseSummary`]. Seeks resolve
/// anchors against the buffer carried as the cursor's context, so the storage
/// tree stays correctly ordered across edits without being rebuilt.
#[derive(Clone, Debug)]
struct CreaseRange(Range<Anchor>);

impl Default for CreaseRange {
    fn default() -> Self {
        Self(Anchor::min()..Anchor::min())
    }
}

impl<'a> Dimension<'a, CreaseSummary> for CreaseRange {
    fn zero(_cx: &MultiBufferSnapshot) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, summary: &'a CreaseSummary, _cx: &MultiBufferSnapshot) {
        self.0.start = summary.start;
        self.0.end = summary.end;
    }
}

impl SeekTarget<'_, CreaseSummary, CreaseRange> for CreaseRange {
    fn cmp(&self, other: &Self, cx: &MultiBufferSnapshot) -> Ordering {
        let resolve = |a: &Anchor| cx.resolve_anchor(a);
        AnchorRangeExt::cmp(&self.0, &other.0, &resolve)
    }
}

impl SeekTarget<'_, CreaseSummary, CreaseRange> for Anchor {
    fn cmp(&self, other: &CreaseRange, cx: &MultiBufferSnapshot) -> Ordering {
        let resolve = |a: &Anchor| cx.resolve_anchor(a);
        Anchor::cmp(self, &other.0.start, &resolve)
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
            creases: SumTree::from_summary(CreaseSummary::default()),
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
        snapshot: &MultiBufferSnapshot,
    ) -> Vec<CreaseId> {
        let resolve = |a: &Anchor| snapshot.resolve_anchor(a);

        let mut new_items: Vec<CreaseItem> = creases
            .into_iter()
            .map(|crease| {
                let id = CreaseId(self.next_id);
                self.next_id += 1;
                self.id_to_range.insert(id, crease.range().clone());
                CreaseItem { id, crease }
            })
            .collect();
        new_items.sort_by(|a, b| AnchorRangeExt::cmp(a.crease.range(), b.crease.range(), &resolve));

        let new_ids: Vec<CreaseId> = new_items.iter().map(|item| item.id).collect();

        self.creases = {
            let mut tree = SumTree::new(snapshot);
            let mut cursor = self.creases.cursor::<CreaseRange>(snapshot);

            for item in new_items {
                tree.append(
                    cursor.slice(&CreaseRange(item.crease.range().clone()), Bias::Left),
                    snapshot,
                );
                tree.push(item, snapshot);
            }
            tree.append(cursor.suffix(), snapshot);
            tree
        };

        new_ids
    }

    pub fn remove(
        &mut self,
        ids: impl IntoIterator<Item = CreaseId>,
        snapshot: &MultiBufferSnapshot,
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
        self.creases = SumTree::from_iter(items, snapshot);

        removed
    }
}

#[derive(Clone)]
pub struct CreaseSnapshot {
    items: Arc<SumTree<CreaseItem>>,
}

impl CreaseSnapshot {
    pub fn empty() -> Self {
        Self {
            items: Arc::new(SumTree::from_summary(CreaseSummary::default())),
        }
    }

    pub fn query_row<'a>(
        &'a self,
        row: u32,
        snapshot: &'a MultiBufferSnapshot,
    ) -> Option<&'a Crease<Anchor>> {
        let start = row_start_anchor(row, snapshot);
        let mut cursor = self.items.cursor::<CreaseRange>(snapshot);
        cursor.seek(&start, Bias::Left);
        while let Some(item) = cursor.item() {
            let start_row = snapshot.point_for_anchor(&item.crease.range().start).row;
            match start_row.cmp(&row) {
                Ordering::Less => cursor.next(),
                Ordering::Equal => return Some(&item.crease),
                Ordering::Greater => break,
            }
        }
        None
    }

    pub fn creases_in_range<'a>(
        &'a self,
        range: Range<u32>,
        snapshot: &'a MultiBufferSnapshot,
    ) -> impl Iterator<Item = &'a Crease<Anchor>> {
        let start = row_start_anchor(range.start, snapshot);
        let mut cursor = self.items.cursor::<CreaseRange>(snapshot);
        cursor.seek(&start, Bias::Left);
        std::iter::from_fn(move || {
            while let Some(item) = cursor.item() {
                cursor.next();
                let start_row = snapshot.point_for_anchor(&item.crease.range().start).row;
                let end_row = snapshot.point_for_anchor(&item.crease.range().end).row;
                if start_row >= range.start && end_row < range.end {
                    return Some(&item.crease);
                }
            }
            None
        })
    }

    pub fn crease_items_with_offsets(
        &self,
        snapshot: &MultiBufferSnapshot,
    ) -> Vec<(CreaseId, Range<Point>)> {
        self.items
            .iter()
            .map(|item| {
                let start = snapshot.point_for_anchor(&item.crease.range().start);
                let end = snapshot.point_for_anchor(&item.crease.range().end);
                (item.id, start..end)
            })
            .collect()
    }

    pub fn creases(&self) -> impl Iterator<Item = (CreaseId, &Crease<Anchor>)> {
        self.items.iter().map(|item| (item.id, &item.crease))
    }
}

fn row_start_anchor(row: u32, snapshot: &MultiBufferSnapshot) -> Anchor {
    let offset = snapshot.rope().point_to_offset(Point::new(row, 0));
    snapshot.anchor_at(offset, Bias::Left)
}

#[cfg(test)]
mod tests {
    use super::{Crease, CreaseMap, FoldPlaceholder};
    use crate::{
        buffer::{BufferId, TextBuffer},
        multi_buffer::{MultiBuffer, MultiBufferSnapshot},
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::{Anchor, Bias, Point};

    fn buffer(content: &str) -> (Arc<RwLock<TextBuffer>>, MultiBuffer) {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            content,
        )));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        (shared, multi_buffer)
    }

    fn crease(snap: &MultiBufferSnapshot, start: (u32, u32), end: (u32, u32)) -> Crease<Anchor> {
        let anchor = |row, col| {
            snap.anchor_at(
                snap.rope().point_to_offset(Point::new(row, col)),
                Bias::Left,
            )
        };
        Crease::inline(
            anchor(start.0, start.1)..anchor(end.0, end.1),
            FoldPlaceholder::default(),
        )
    }

    #[test]
    fn insert_and_query() {
        let (_buffer, multi_buffer) = buffer("l0\nl1\nl2\nl3");
        let snap = multi_buffer.snapshot();
        let mut map = CreaseMap::new();

        let ids = map.insert([crease(&snap, (1, 0), (1, 2))], &snap);
        assert_eq!(ids.len(), 1);

        let cs = map.snapshot();
        assert!(cs.query_row(1, &snap).is_some());
        assert!(cs.query_row(0, &snap).is_none());
        assert!(cs.query_row(2, &snap).is_none());
    }

    #[test]
    fn remove() {
        let (_buffer, multi_buffer) = buffer("l0\nl1\nl2\nl3");
        let snap = multi_buffer.snapshot();
        let mut map = CreaseMap::new();

        let ids = map.insert(
            [crease(&snap, (0, 0), (0, 2)), crease(&snap, (2, 0), (2, 2))],
            &snap,
        );
        assert_eq!(map.snapshot().creases().count(), 2);

        let removed = map.remove([ids[0]], &snap);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].0, ids[0]);
        assert_eq!(map.snapshot().creases().count(), 1);
    }

    #[test]
    fn query_resolves_lazily_after_edit() {
        let (shared, multi_buffer) = buffer("aaa\nbbb\nccc");
        let snap = multi_buffer.snapshot();
        let mut map = CreaseMap::new();
        map.insert([crease(&snap, (1, 0), (1, 3))], &snap);
        assert!(map.snapshot().query_row(1, &snap).is_some());

        shared.write().unwrap().edit(0..0, "XXX\n");
        let snap2 = multi_buffer.snapshot();

        let cs = map.snapshot();
        assert!(cs.query_row(2, &snap2).is_some());
        assert!(cs.query_row(1, &snap2).is_none());
    }

    #[test]
    fn creases_in_range_seeks_to_start() {
        let (_buffer, multi_buffer) = buffer("l0\nl1\nl2\nl3\nl4\nl5");
        let snap = multi_buffer.snapshot();
        let mut map = CreaseMap::new();
        map.insert(
            [
                crease(&snap, (1, 0), (1, 2)),
                crease(&snap, (3, 0), (3, 2)),
                crease(&snap, (5, 0), (5, 2)),
            ],
            &snap,
        );

        let cs = map.snapshot();
        let in_range: Vec<_> = cs.creases_in_range(2..5, &snap).collect();
        assert_eq!(in_range.len(), 1);
    }

    #[test]
    fn crease_items_with_offsets() {
        let (_buffer, multi_buffer) = buffer("l0\nl1\nl2\nl3");
        let snap = multi_buffer.snapshot();
        let mut map = CreaseMap::new();
        map.insert(
            [crease(&snap, (1, 0), (1, 2)), crease(&snap, (2, 0), (2, 2))],
            &snap,
        );

        let items = map.snapshot().crease_items_with_offsets(&snap);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].1, Point::new(1, 0)..Point::new(1, 2));
        assert_eq!(items[1].1, Point::new(2, 0)..Point::new(2, 2));
    }
}
