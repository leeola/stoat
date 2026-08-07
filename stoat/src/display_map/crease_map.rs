use crate::{
    display_map::{fold_map, BlockStyle, FoldPlaceholder},
    multi_buffer::MultiBufferSnapshot,
};
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
    /// Buffer version the stored offsets were last known good against, which is
    /// what lets a sync carry them across an edit rather than re-resolve them.
    ///
    /// `None` whenever the crease set itself changed, since `insert` resolves
    /// through a caller's closure and `remove` drops items, and neither names a
    /// version the carry could start from.
    last_synced_version: Option<u64>,
}

impl Default for CreaseMap {
    fn default() -> Self {
        Self {
            creases: SumTree::new(()),
            next_id: 0,
            id_to_range: HashMap::new(),
            last_synced_version: None,
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
        self.last_synced_version = None;

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
        self.last_synced_version = None;

        removed
    }

    /// Bring every crease's resolved offsets up to date with `buffer`.
    ///
    /// Creases come from LSP folding ranges, one per foldable region, so a large
    /// file holds thousands and this runs on every buffer version change. When
    /// the offsets can be carried across the edits since the last sync, only the
    /// anchors the carry could not vouch for are resolved, and an edit that
    /// moved nothing leaves the tree alone entirely.
    pub fn sync(&mut self, buffer: &MultiBufferSnapshot) {
        let version = buffer.version();
        if self.creases.is_empty() {
            self.last_synced_version = Some(version);
            return;
        }

        let items: Vec<CreaseItem> = self.creases.iter().cloned().collect();
        let stored: Vec<usize> = items
            .iter()
            .flat_map(|item| [item.resolved_start, item.resolved_end])
            .collect();

        let resolved = match self.last_synced_version {
            Some(since) => carry_resolved(&items, &stored, buffer, since),
            None => {
                let anchors: Vec<Anchor> = items
                    .iter()
                    .flat_map(|item| [item.crease.range().start, item.crease.range().end])
                    .collect();
                buffer.resolve_anchors_batch(&anchors)
            },
        };

        self.last_synced_version = Some(version);
        if resolved == stored {
            return;
        }
        self.install_resolved(items, &resolved);
    }

    /// Write `resolved` onto `items` and rebuild the tree in offset order.
    ///
    /// `resolved` interleaves each crease's start and end, matching `items`. A
    /// carry is monotone so the order usually survives it, but an edit that
    /// swallows a range collapses its start onto another's, and the tree is
    /// keyed on that order.
    fn install_resolved(&mut self, mut items: Vec<CreaseItem>, resolved: &[usize]) {
        for (item, pair) in items.iter_mut().zip(resolved.chunks_exact(2)) {
            item.resolved_start = pair[0];
            item.resolved_end = pair[1];
        }
        items.sort_by_key(|c| c.resolved_start);
        self.creases = SumTree::from_iter(items, ());
    }
}

/// Carry `stored` across the edits `buffer` has taken since `since`, resolving
/// only the anchors the carry could not place itself.
///
/// The carry cannot vouch for an offset an edit touched the boundary of, since a
/// crease's anchor bias decides whether it moved and only the anchor knows.
/// Those go back to the buffer. The rest are arithmetic.
///
/// `stored` interleaves each crease's start and end, matching `items`.
fn carry_resolved(
    items: &[CreaseItem],
    stored: &[usize],
    buffer: &MultiBufferSnapshot,
    since: u64,
) -> Vec<usize> {
    let edits = buffer.edits_since(since);
    let mut offsets = stored.to_vec();
    let mut needs_resolve = vec![false; offsets.len()];

    // carry_offsets walks one ascending sequence, and an earlier edit can leave
    // the stored offsets crossed, so an index permutation is sorted instead.
    let mut order: Vec<usize> = (0..offsets.len()).collect();
    order.sort_unstable_by_key(|&i| offsets[i]);
    let mut sorted: Vec<usize> = order.iter().map(|&i| offsets[i]).collect();
    let mut sorted_flags = vec![false; sorted.len()];
    fold_map::carry_offsets(&mut sorted, &mut sorted_flags, &edits);
    for (slot, &i) in order.iter().enumerate() {
        offsets[i] = sorted[slot];
        needs_resolve[i] = sorted_flags[slot];
    }

    let affected: Vec<usize> = needs_resolve
        .iter()
        .enumerate()
        .filter(|&(_, &needs)| needs)
        .map(|(i, _)| i)
        .collect();
    if affected.is_empty() {
        return offsets;
    }

    let anchor_at = |i: usize| {
        let range = items[i / 2].crease.range();
        match i % 2 {
            0 => range.start,
            _ => range.end,
        }
    };
    let to_resolve: Vec<Anchor> = affected.iter().map(|&i| anchor_at(i)).collect();
    for (&i, offset) in affected
        .iter()
        .zip(buffer.resolve_anchors_batch(&to_resolve))
    {
        offsets[i] = offset;
    }

    offsets
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
    use super::{Crease, CreaseItem, CreaseMap, FoldPlaceholder, Range};
    use crate::{
        buffer::{BufferId, SharedBuffer, TextBuffer},
        multi_buffer::MultiBuffer,
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
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
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
        map.sync(&snap);

        (map, shared, multi)
    }

    fn starts_and_ends(map: &CreaseMap) -> Vec<(usize, usize)> {
        map.creases
            .iter()
            .map(|i| (i.resolved_start, i.resolved_end))
            .collect()
    }

    #[test]
    fn carrying_offsets_lands_where_resolving_them_all_does() {
        // The carry is arithmetic over a patch rather than a walk per anchor, so
        // the only thing making it safe is that it agrees with the walk.
        let text = "line0 body\nline1 body\nline2 body\nline3 body\n";
        for edit_at in [0usize, 6, 11, 22, 33, 43] {
            let ranges = [6..10, 17..21, 28..32];
            let (mut carried, shared, multi) = map_over(text, &ranges);
            let (mut resolved, _, _) = map_over(text, &ranges);

            shared
                .write()
                .expect("poisoned")
                .edit(edit_at..edit_at, "XY");
            let after = multi.snapshot();

            carried.sync(&after);
            // Forgetting the version is what sends a sync back to resolving
            // every anchor, which is the answer being compared against.
            resolved.last_synced_version = None;
            resolved.sync(&after);

            assert_eq!(
                starts_and_ends(&carried),
                starts_and_ends(&resolved),
                "carried and fully resolved disagree for an edit at {edit_at}",
            );
        }
    }

    #[test]
    fn an_edit_past_every_crease_moves_no_offset() {
        // What the carry buys. Typing at the end of a file with a crease on
        // every foldable region used to re-resolve all of them.
        let text = "line0 body\nline1 body\nline2 body\n";
        let (mut map, shared, multi) = map_over(text, &[6..10, 17..21]);
        let before = starts_and_ends(&map);

        let end = text.len();
        shared.write().expect("poisoned").edit(end..end, "tail\n");
        map.sync(&multi.snapshot());

        assert_eq!(
            starts_and_ends(&map),
            before,
            "nothing before the edit moved"
        );
    }

    #[test]
    fn a_crease_inserted_after_an_unsynced_edit_is_not_carried_twice() {
        // insert resolves against whatever snapshot the caller holds, which can
        // already be past the version the carry would start from. Carrying the
        // new crease from there would apply an edit it had already accounted
        // for, so a crease-set change has to send the next sync back to
        // resolving everything.
        let text = "line0 body\nline1 body\n";
        #[allow(clippy::single_range_in_vec_init)]
        let (mut map, shared, multi) = map_over(text, &[6..10]);

        shared.write().expect("poisoned").edit(0..0, "XY");
        let after = multi.snapshot();

        // Resolved against the edited buffer, while the map's carry point still
        // names the version before it.
        map.insert(
            [Crease::inline(
                after.anchor_at(19, Bias::Right)..after.anchor_at(23, Bias::Left),
                FoldPlaceholder::default(),
            )],
            &|a: &Anchor| after.resolve_anchor(a),
        );
        map.sync(&after);

        assert_eq!(
            starts_and_ends(&map),
            vec![(8, 12), (19, 23)],
            "the first crease shifts by the edit, the second is already past it",
        );
    }

    #[test]
    fn an_edit_before_a_crease_shifts_it() {
        let text = "line0 body\nline1 body\n";
        #[allow(clippy::single_range_in_vec_init)]
        let (mut map, shared, multi) = map_over(text, &[17..21]);
        assert_eq!(starts_and_ends(&map), vec![(17, 21)]);

        shared.write().expect("poisoned").edit(0..0, "XY");
        map.sync(&multi.snapshot());

        assert_eq!(
            starts_and_ends(&map),
            vec![(19, 23)],
            "two inserted bytes ahead of it carry it two along",
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

        // Resolution that moves the second crease in front of the first, which
        // a real edit reaches only by collapsing the text between them.
        let items: Vec<CreaseItem> = map.creases.iter().cloned().collect();
        let resolved: Vec<usize> = items
            .iter()
            .flat_map(|item| [item.crease.range().start, item.crease.range().end])
            .map(|a| match a.offset {
                20 => 2,
                25 => 7,
                other => other as usize,
            })
            .collect();
        map.install_resolved(items, &resolved);

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
