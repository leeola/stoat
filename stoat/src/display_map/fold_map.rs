use super::{
    highlights::{Chunk, HighlightEndpoint},
    inlay_map::{InlayChunks, InlayOffset, InlayPoint, InlaySnapshot},
};
use crate::multi_buffer::MultiBufferSnapshot;
use std::{
    any::TypeId,
    borrow::Cow,
    cmp::Ordering,
    collections::HashMap,
    ops::{Add, AddAssign, Deref, Range, Sub},
    sync::Arc,
};
use stoat_text::{
    patch::Patch, tree_map::TreeMap, Anchor, AnchorRangeExt, Bias, CharsAt, ContextLessSummary,
    Cursor, Dimension, Dimensions, Edit, Item, KeyedItem, Point, ReversedCharsAt, Rope, SeekTarget,
    SumTree, TextSummary,
};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FoldId(pub(crate) usize);

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FoldOffset(pub usize);

impl Add for FoldOffset {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub for FoldOffset {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for FoldOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FoldPoint(pub Point);

impl FoldPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self(Point::new(row, column))
    }

    pub fn row(&self) -> u32 {
        self.0.row
    }

    pub fn column(&self) -> u32 {
        self.0.column
    }
}

impl From<Point> for FoldPoint {
    fn from(point: Point) -> Self {
        Self(point)
    }
}

#[derive(Clone, Debug)]
pub struct FoldPlaceholder {
    pub text: Arc<str>,
    /// LSP-provided collapsed text to display instead of `text` when available.
    pub collapsed_text: Option<Arc<str>>,
    /// If true, adjacent folds with the same `type_tag` merge visually.
    pub merge_adjacent: bool,
    /// Category identifier for selective fold removal.
    pub type_tag: Option<TypeId>,
}

impl FoldPlaceholder {
    /// The string this fold renders as.
    ///
    /// Every consumer must resolve the two strings the same way. The
    /// transform's output summary is what fold-offset and fold-point
    /// conversions measure the fold by, and the char iterators behind
    /// [`FoldSnapshot::line_len`] are what a row's width is summed from, so a
    /// collapsed text of a different length than the ellipsis would otherwise
    /// paint one width and measure another, putting everything to its right on
    /// the row at the wrong column.
    fn display_text(&self) -> &Arc<str> {
        self.collapsed_text.as_ref().unwrap_or(&self.text)
    }
}

impl Default for FoldPlaceholder {
    fn default() -> Self {
        Self {
            text: Arc::from("..."),
            collapsed_text: None,
            merge_adjacent: true,
            type_tag: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FoldMetadata {
    pub range: Range<Anchor>,
    pub display_width: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Fold {
    pub id: FoldId,
    pub range: Range<InlayPoint>,
    pub placeholder: FoldPlaceholder,
}

#[derive(Clone, Debug, Default)]
pub struct FoldSummary {
    start: InlayPoint,
    end: InlayPoint,
    min_start: InlayPoint,
    max_end: InlayPoint,
    count: usize,
}

impl ContextLessSummary for FoldSummary {
    fn add_summary(&mut self, other: &Self) {
        if other.count > 0 {
            if self.count == 0 {
                self.min_start = other.min_start;
            } else {
                self.min_start = self.min_start.min(other.min_start);
            }
            self.start = other.start;
            self.end = other.end;
            self.max_end = self.max_end.max(other.max_end);
            self.count += other.count;
        }
    }
}

impl Item for Fold {
    type Summary = FoldSummary;

    fn summary(&self, _cx: ()) -> FoldSummary {
        FoldSummary {
            start: self.range.start,
            end: self.range.end,
            min_start: self.range.start,
            max_end: self.range.end,
            count: 1,
        }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FoldStart(InlayPoint);

impl<'a> Dimension<'a, FoldSummary> for FoldStart {
    fn zero(_cx: ()) -> Self {
        Self(InlayPoint::default())
    }

    fn add_summary(&mut self, s: &'a FoldSummary, _cx: ()) {
        if s.count > 0 {
            self.0 = s.start;
        }
    }
}

#[derive(Clone, Debug)]
struct Transform {
    summary: TransformSummary,
    /// The string a fold transform renders as, already resolved by
    /// [`FoldPlaceholder::display_text`]. `None` marks an isomorphic segment.
    ///
    /// Resolved once here rather than carrying the whole placeholder, so no
    /// consumer downstream can measure one string and emit another.
    placeholder_text: Option<Arc<str>>,
    fold_id: Option<FoldId>,
}

#[derive(Clone, Debug, Default)]
struct TransformSummary {
    input: TextSummary,
    output: TextSummary,
}

impl ContextLessSummary for TransformSummary {
    fn add_summary(&mut self, other: &Self) {
        ContextLessSummary::add_summary(&mut self.input, &other.input);
        ContextLessSummary::add_summary(&mut self.output, &other.output);
    }
}

impl Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self, _cx: ()) -> TransformSummary {
        self.summary.clone()
    }
}

impl<'a> Dimension<'a, TransformSummary> for InlayPoint {
    fn zero(_cx: ()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        self.0 += s.input.lines;
    }
}

impl<'a> Dimension<'a, TransformSummary> for FoldPoint {
    fn zero(_cx: ()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        self.0 += s.output.lines;
    }
}

impl<'a> SeekTarget<'a, TransformSummary, Dimensions<InlayPoint, FoldPoint>> for FoldPoint {
    fn cmp(&self, cursor_location: &Dimensions<InlayPoint, FoldPoint>, _cx: ()) -> Ordering {
        Ord::cmp(self, &cursor_location.1)
    }
}

#[derive(Clone, Debug)]
struct AnchoredFold {
    id: FoldId,
    range: Range<Anchor>,
    placeholder: FoldPlaceholder,
    resolved_start: usize,
    resolved_end: usize,
}

#[derive(Clone, Debug, Default)]
struct AnchoredFoldSummary {
    key: Option<usize>,
    max_end: usize,
}

impl ContextLessSummary for AnchoredFoldSummary {
    fn add_summary(&mut self, other: &Self) {
        if other.key.is_some() {
            self.key = other.key;
        }
        self.max_end = self.max_end.max(other.max_end);
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FoldKeyRef(Option<usize>);

impl<'a> Dimension<'a, AnchoredFoldSummary> for FoldKeyRef {
    fn zero(_cx: ()) -> Self {
        Self(None)
    }
    fn add_summary(&mut self, summary: &'a AnchoredFoldSummary, _cx: ()) {
        if summary.key.is_some() {
            self.0 = summary.key;
        }
    }
}

impl Item for AnchoredFold {
    type Summary = AnchoredFoldSummary;
    fn summary(&self, _cx: ()) -> AnchoredFoldSummary {
        AnchoredFoldSummary {
            key: Some(self.resolved_start),
            max_end: self.resolved_end,
        }
    }
}

impl KeyedItem for AnchoredFold {
    type Key = FoldKeyRef;
    fn key(&self) -> FoldKeyRef {
        FoldKeyRef(Some(self.resolved_start))
    }
}

pub struct FoldMap {
    folds: SumTree<AnchoredFold>,
    next_id: usize,
    version: usize,
    cached_snapshot: Option<Arc<FoldSnapshot>>,
    last_inlay_version: usize,
    /// Inlay *set* version the cached resolved folds were placed against. A
    /// `Fold`'s range is in inlay space, so an inlay spliced ahead of one moves
    /// it without touching its buffer offset, and only this notices.
    last_inlay_set_version: usize,
    last_self_version: usize,
    /// Buffer version the stored folds. `resolved_start` and `resolved_end`
    /// were measured against, so the next sync can carry them across exactly
    /// the edits made since.
    last_buffer_version: u64,
}

pub struct FoldSnapshot {
    inlay_snapshot: Arc<InlaySnapshot>,
    transforms: SumTree<Transform>,
    folds: SumTree<Fold>,
    fold_metadata_by_id: TreeMap<FoldId, FoldMetadata>,
    version: usize,
}

impl FoldMap {
    pub fn new(inlay_snapshot: Arc<InlaySnapshot>) -> (Self, Arc<FoldSnapshot>) {
        let empty_folds = SumTree::new(());
        let transforms = build_fold_transforms(&inlay_snapshot, &empty_folds);
        let inlay_version = inlay_snapshot.inlay_version;
        let inlay_set_version = inlay_snapshot.inlay_set_version;
        let buffer_version = inlay_snapshot.buffer_snapshot().version();
        let snapshot = Arc::new(FoldSnapshot {
            inlay_snapshot,
            transforms,
            folds: SumTree::new(()),
            fold_metadata_by_id: TreeMap::default(),
            version: 0,
        });
        let map = FoldMap {
            folds: SumTree::default(),
            next_id: 0,
            version: 0,
            cached_snapshot: Some(Arc::clone(&snapshot)),
            last_inlay_version: inlay_version,
            last_inlay_set_version: inlay_set_version,
            last_self_version: 0,
            last_buffer_version: buffer_version,
        };
        (map, snapshot)
    }

    pub fn sync(
        &mut self,
        inlay_snapshot: Arc<InlaySnapshot>,
        inlay_edits: &Patch<u32>,
    ) -> (Arc<FoldSnapshot>, Patch<u32>) {
        if inlay_snapshot.inlay_version == self.last_inlay_version
            && self.version == self.last_self_version
            && let Some(ref cached) = self.cached_snapshot
        {
            return (Arc::clone(cached), Patch::empty());
        }

        let buffer = inlay_snapshot.buffer_snapshot();
        let all_folds: Vec<AnchoredFold> = self.folds.iter().cloned().collect();
        let (all_points, all_offsets) = resolve_fold_points(
            &all_folds,
            buffer,
            // A toggle leaves the stored folds' offsets on mixed footing:
            // `fold` writes new ones at insert time and the merge pass rewrites
            // others, so none of them are reliably as of `last_buffer_version`.
            (self.version == self.last_self_version).then_some(self.last_buffer_version),
        );
        // A fold toggle moves no inlay row, so the rows it changed have to be
        // derived from the fold set itself. Without this every toggle falls to
        // the full rebuild below and invalidates the whole file downstream.
        //
        // Only when the inlay text is unchanged, though. Fold rows are placed
        // against the cached transform tree, which describes the text as it was,
        // so a fold arriving with a buffer edit keeps the full rebuild.
        //
        // Both versions have to agree. The inlay set can change without a
        // buffer edit, and the buffer can change without touching the inlay
        // set, so either one moving alone still means different text.
        let inlay_unchanged = inlay_snapshot.inlay_version == self.last_inlay_version
            && inlay_snapshot.buffer_snapshot().version() == self.last_buffer_version;

        // Nothing about where the folds sit has moved. Their buffer offsets came
        // back the same over text that itself is unchanged, so they resolve to
        // the same inlay points. The resolved tree the cache holds is therefore
        // the tree this would rebuild, and `self.folds` already carries these
        // offsets.
        //
        // Equal offsets alone would not be enough. Replacing a newline with an
        // ordinary character costs the bytes it frees, so every later offset
        // stays put while the row it ended disappears and each row after it
        // shifts up by one. The cached tree names rows, so reusing it across
        // that edit hands the sync fold positions one row stale.
        let placement_held = inlay_unchanged
            && all_offsets
                .chunks_exact(2)
                .zip(all_folds.iter())
                .all(|(pair, af)| pair[0] == af.resolved_start && pair[1] == af.resolved_end)
            && inlay_snapshot.inlay_set_version == self.last_inlay_set_version
            && self.version == self.last_self_version;

        let resolved_tree = match self.cached_snapshot.as_ref().filter(|_| placement_held) {
            Some(cached) => cached.folds.clone(),
            None => {
                let mut stored = Vec::with_capacity(all_folds.len());
                let mut resolved = Vec::with_capacity(all_folds.len());
                for (i, af) in all_folds.iter().enumerate() {
                    let start_pt = all_points[i * 2];
                    let end_pt = all_points[i * 2 + 1];
                    let start_inlay = inlay_snapshot.to_inlay_point(start_pt);
                    let end_inlay = inlay_snapshot.to_inlay_point(end_pt);

                    // The offsets these points were made from, rather than the
                    // round trip back out through inlay space, which lands on the
                    // same answer by way of four more descents per fold.
                    let mut carried = af.clone();
                    carried.resolved_start = all_offsets[i * 2];
                    carried.resolved_end = all_offsets[i * 2 + 1];
                    stored.push(carried);

                    // A fold whose endpoints have met covers nothing and renders
                    // nothing, but its record stays. Which regions the reader
                    // folded is their own state, and an edit that swallows one is
                    // undoable, so dropping the record here would lose the fold
                    // for good.
                    if start_inlay < end_inlay {
                        resolved.push(Fold {
                            id: af.id,
                            range: start_inlay..end_inlay,
                            placeholder: af.placeholder.clone(),
                        });
                    }
                }
                stored.sort_by_key(|f| f.resolved_start);
                self.folds = SumTree::from_iter(stored, ());

                resolved.sort_by_key(|f| f.range.start);
                SumTree::from_iter(resolved, ())
            },
        };

        let fold_edits = self
            .cached_snapshot
            .as_ref()
            .filter(|_| inlay_unchanged)
            .map(|cached| fold_set_edits(&cached.folds, &resolved_tree));

        let can_incremental = !inlay_edits.is_empty()
            && self.version == self.last_self_version
            && self.cached_snapshot.is_some();

        // A fold-only change and a buffer edit cannot both apply here. The
        // former is exactly the case where the inlay version held still, and
        // the latter is what moved it.
        let sync_edits = fold_edits.or_else(|| can_incremental.then(|| inlay_edits.clone()));

        let (transforms, edits) = if let Some(sync_edits) = sync_edits {
            let old_snapshot = self
                .cached_snapshot
                .as_ref()
                .expect("sync_edits is Some only with a cached snapshot");
            sync_fold_incremental(old_snapshot, &inlay_snapshot, &sync_edits, &resolved_tree)
        } else {
            let old_line_count = self
                .cached_snapshot
                .as_ref()
                .map(|s| s.line_count())
                .unwrap_or(0);
            let transforms = build_fold_transforms(&inlay_snapshot, &resolved_tree);
            let new_line_count = if transforms.is_empty() {
                1
            } else {
                let extent: FoldPoint = transforms.extent(());
                extent.row() + 1
            };
            let edits = Patch::new(vec![stoat_text::patch::Edit {
                old: 0..old_line_count,
                new: 0..new_line_count,
            }]);
            (transforms, edits)
        };

        // Every conversion out of fold space answers from this tree, so a tree
        // describing a different amount of text than the inlay layer holds is
        // wrong everywhere at once and visible nowhere. The readings stay
        // consistent with each other while drifting from the text they claim
        // to describe.
        debug_assert_eq!(
            transforms.summary().input.len,
            inlay_snapshot.total_summary().len,
            "fold transforms must account for the inlay text exactly",
        );

        // The map holds anchors and a `None`, so a buffer edit alone cannot
        // change any entry, and the fold set only ever grows or shrinks by a
        // toggle. Collapsing keeps its record, so it changes no entry either.
        let reuse_metadata = self.version == self.last_self_version;
        let fold_metadata_by_id = match self.cached_snapshot.as_ref().filter(|_| reuse_metadata) {
            Some(cached) => cached.fold_metadata_by_id.clone(),
            None => {
                let mut map = TreeMap::default();
                for fold in self.folds.iter() {
                    map.insert(
                        fold.id,
                        FoldMetadata {
                            range: fold.range.clone(),
                            display_width: None,
                        },
                    );
                }
                map
            },
        };

        let snapshot = Arc::new(FoldSnapshot {
            inlay_snapshot,
            transforms,
            folds: resolved_tree,
            fold_metadata_by_id,
            version: self.version,
        });
        self.last_inlay_version = snapshot.inlay_snapshot.inlay_version;
        self.last_inlay_set_version = snapshot.inlay_snapshot.inlay_set_version;
        self.last_self_version = self.version;
        self.last_buffer_version = snapshot.inlay_snapshot.buffer_snapshot().version();
        self.cached_snapshot = Some(Arc::clone(&snapshot));
        (snapshot, edits)
    }

    pub fn fold(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        placeholder: FoldPlaceholder,
        buffer_snapshot: &MultiBufferSnapshot,
    ) -> Vec<FoldId> {
        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        let mut new_folds: Vec<AnchoredFold> = ranges
            .into_iter()
            .map(|range| {
                let resolved_start = resolve(&range.start);
                let resolved_end = resolve(&range.end);
                let id = FoldId(self.next_id);
                self.next_id += 1;
                AnchoredFold {
                    id,
                    range,
                    placeholder: placeholder.clone(),
                    resolved_start,
                    resolved_end,
                }
            })
            .collect();
        let new_ids: Vec<FoldId> = new_folds.iter().map(|f| f.id).collect();
        new_folds.sort_by_key(|f| f.resolved_start);

        // A version that moved with the fold set unchanged reaches WrapMap::sync
        // as a change with no text edit, which is its whole-file rebuild
        // condition. Folding nothing must not cost that.
        if new_folds.is_empty() {
            return new_ids;
        }

        let edits: Vec<Edit<AnchoredFold>> = new_folds.into_iter().map(Edit::Insert).collect();
        self.folds.edit(edits, ());

        self.version += 1;
        new_ids
    }

    pub fn unfold(&mut self, ranges: Vec<Range<usize>>, buffer_snapshot: &MultiBufferSnapshot) {
        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        let mut new_folds = SumTree::default();
        let mut removed_any = false;
        for fold in self.folds.iter() {
            if ranges
                .iter()
                .any(|r| fold.range.overlaps_range(r, &resolve))
            {
                removed_any = true;
            } else {
                new_folds.push(fold.clone(), ());
            }
        }

        // Unfolding nothing has to leave the version alone, or WrapMap::sync
        // reads a change with no text edit and rebuilds the whole file for it.
        if !removed_any {
            return;
        }

        self.folds = new_folds;
        self.version += 1;
    }

    pub fn is_folded_at_offset(
        &self,
        offset: usize,
        buffer_snapshot: &MultiBufferSnapshot,
    ) -> bool {
        if self.folds.is_empty() {
            return false;
        }
        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        let mut cursor = self
            .folds
            .filter::<_, FoldKeyRef>((), |summary| summary.max_end > offset);
        cursor.next();
        while let Some(fold) = cursor.item() {
            if fold.resolved_start > offset {
                return false;
            }
            if fold.range.contains_offset(offset, &resolve) {
                return true;
            }
            cursor.next();
        }
        false
    }

    pub fn version_unchanged(&self) -> bool {
        self.version == self.last_self_version
    }
}

/// The placeholder regions `folds` paint, with overlapping and
/// mergeable-adjacent folds collapsed into one.
///
/// Folds are stored as the caller asked for them, so two can cover the same
/// text. Emitting a placeholder for each would give the transform tree more
/// input than the inlay text holds, so they are collapsed here instead of in
/// the stored set, where collapsing would destroy a record.
///
/// The first fold of a region carries its placeholder and id, which is what
/// the chunk stream and [`FoldSnapshot::fold_id_at_point`] answer with.
///
/// `folds` must be ordered by `range.start`, which is how the resolved tree is
/// built.
fn merged_fold_regions<'a>(
    folds: impl IntoIterator<Item = &'a Fold>,
) -> Vec<(&'a Fold, Range<InlayPoint>)> {
    let mut regions: Vec<(&'a Fold, Range<InlayPoint>)> = Vec::new();
    for fold in folds {
        match regions.last_mut() {
            Some((first, region))
                if fold.range.start < region.end
                    || (fold.range.start == region.end
                        && first.placeholder.merge_adjacent
                        && fold.placeholder.merge_adjacent) =>
            {
                region.end = region.end.max(fold.range.end);
            },
            _ => regions.push((fold, fold.range.clone())),
        }
    }

    // Both emission sites walk these regions carrying a cursor, so a region
    // starting before the previous one ended would emit the shared text twice
    // and leave the transform tree describing more input than exists. That is
    // silent at every layer above, which is why it is caught here rather than
    // waited on.
    if cfg!(debug_assertions) {
        for pair in regions.windows(2) {
            assert!(
                pair[0].1.end <= pair[1].1.start,
                "fold regions {:?} and {:?} overlap after merging",
                pair[0].1,
                pair[1].1,
            );
        }
    }

    regions
}

/// Text summary of `range` in the inlay layer's output space.
///
/// Accumulated over the chunk stream, which is the same source the fold chunk
/// stream reads. Taking it from a materialized string instead would mean
/// building the whole file plus its hints to read one span of it.
///
/// `TextSummary`'s own join is what makes this exact: a row crossing a chunk
/// boundary still counts once, and the longest row is still the longest.
fn inlay_text_summary(inlay_snapshot: &InlaySnapshot, range: Range<usize>) -> TextSummary {
    if !inlay_snapshot.has_inlays() {
        return inlay_snapshot.rope().text_summary_for_range(range);
    }

    let mut summary = TextSummary::default();
    let chunks = inlay_snapshot.chunks(
        InlayOffset(range.start)..InlayOffset(range.end),
        Arc::from(Vec::new()),
    );
    for chunk in chunks {
        ContextLessSummary::add_summary(&mut summary, &TextSummary::from_str(&chunk.text));
    }
    summary
}

fn build_fold_transforms(
    inlay_snapshot: &InlaySnapshot,
    folds: &SumTree<Fold>,
) -> SumTree<Transform> {
    let mut transforms = SumTree::new(());

    if folds.is_empty() {
        let summary = inlay_snapshot.total_summary();
        if summary.len > 0 {
            transforms.push(
                Transform {
                    summary: TransformSummary {
                        input: summary.clone(),
                        output: summary,
                    },
                    placeholder_text: None,
                    fold_id: None,
                },
                (),
            );
        }
        return transforms;
    }

    let rope = inlay_snapshot.rope();
    let has_inlays = inlay_snapshot.has_inlays();

    // With no inlays an inlay offset is a buffer offset, and the rope answers
    // lengths and summaries without a chunk iterator in between.
    let text_len = if has_inlays {
        inlay_snapshot.total_summary().len
    } else {
        rope.len()
    };
    let offset_of = |point: InlayPoint| -> usize {
        if has_inlays {
            inlay_snapshot.inlay_point_to_offset(point).0.min(text_len)
        } else {
            let buf_point = inlay_snapshot.to_buffer_point(point);
            rope.point_to_offset(buf_point).min(rope.len())
        }
    };
    let mut cursor = 0usize;

    for (fold, region) in merged_fold_regions(folds.iter()) {
        let fold_start = offset_of(region.start);
        let fold_end = offset_of(region.end);

        if fold_start > cursor {
            let summary = inlay_text_summary(inlay_snapshot, cursor..fold_start);
            transforms.push(
                Transform {
                    summary: TransformSummary {
                        input: summary.clone(),
                        output: summary,
                    },
                    placeholder_text: None,
                    fold_id: None,
                },
                (),
            );
        }

        let input_summary = inlay_text_summary(inlay_snapshot, fold_start..fold_end);
        let placeholder_text = fold.placeholder.display_text().clone();
        let output_summary = TextSummary::from_str(&placeholder_text);
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input: input_summary,
                    output: output_summary,
                },
                placeholder_text: Some(placeholder_text),
                fold_id: Some(fold.id),
            },
            (),
        );

        cursor = fold_end;
    }

    if cursor < text_len {
        let summary = inlay_text_summary(inlay_snapshot, cursor..text_len);
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input: summary.clone(),
                    output: summary,
                },
                placeholder_text: None,
                fold_id: None,
            },
            (),
        );
    }

    transforms
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct InputOffset(usize);

impl<'a> Dimension<'a, TransformSummary> for InputOffset {
    fn zero(_cx: ()) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        self.0 += s.input.len;
    }
}

impl<'a> Dimension<'a, TransformSummary> for FoldOffset {
    fn zero(_cx: ()) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        self.0 += s.output.len;
    }
}

fn push_fold_isomorphic(tree: &mut SumTree<Transform>, summary: TransformSummary) {
    if summary.input.len == 0 {
        return;
    }
    let mut summary = Some(summary);
    tree.update_last(
        |t| {
            if t.placeholder_text.is_none() {
                ContextLessSummary::add_summary(
                    &mut t.summary,
                    &summary.take().expect("set on entry"),
                );
            }
        },
        (),
    );
    if let Some(s) = summary {
        tree.push(
            Transform {
                summary: s,
                placeholder_text: None,
                fold_id: None,
            },
            (),
        );
    }
}

/// Carry `offsets` across `edits`, flagging the ones an edit could have moved
/// in a way the arithmetic cannot predict.
///
/// One forward pass over both ascending sequences. An offset ends up carrying
/// the summed delta of every edit it sits after.
///
/// Mirrors the inlay layer's `shift_offsets` but treats an edit's boundaries as
/// ambiguous rather than only its interior. A fold's end anchor is biased Left,
/// so an insertion landing exactly on it leaves it where it was while the carry
/// would push it along by the inserted length. Flagging it sends it back to its
/// anchor, which is the only thing that knows.
///
/// `offsets` must be ascending. The caller sorts an index permutation, since an
/// edit can leave the previous sync's fold offsets crossed.
pub(super) fn carry_offsets(
    offsets: &mut [usize],
    needs_resolve: &mut [bool],
    edits: &Patch<usize>,
) {
    let mut delta: isize = 0;
    let mut i = 0;
    for edit in edits {
        while i < offsets.len() && offsets[i] < edit.old.start {
            offsets[i] = ((offsets[i] as isize) + delta).max(0) as usize;
            i += 1;
        }
        while i < offsets.len() && offsets[i] <= edit.old.end {
            offsets[i] = ((offsets[i] as isize) + delta).max(0) as usize;
            needs_resolve[i] = true;
            i += 1;
        }
        // Assigned, not accumulated. A patch states new ranges in
        // post-all-edits coordinates, so this difference is the running total
        // already and adding them counts every earlier edit again.
        delta = (edit.new.end as isize) - (edit.old.end as isize);
    }
    for offset in &mut offsets[i..] {
        *offset = ((*offset as isize) + delta).max(0) as usize;
    }
}

/// Buffer points for every fold's start and end, interleaved as
/// `[start0, end0, start1, end1, ..]`.
///
/// With `carry_from` naming the buffer version the folds' cached
/// `resolved_start` and `resolved_end` were measured at, the offsets are
/// carried across the edits made since and only those landing inside an edit
/// are re-resolved from their anchors. Otherwise every anchor is resolved,
/// which sorts them and walks two sum trees, so a keystroke with a fold on
/// every function pays for the whole file however small the edit was.
///
/// `None` forces the full resolve, for a caller that cannot vouch for the
/// cached offsets.
fn resolve_fold_points(
    folds: &[AnchoredFold],
    buffer: &MultiBufferSnapshot,
    carry_from: Option<u64>,
) -> (Vec<Point>, Vec<usize>) {
    let anchors = || -> Vec<Anchor> {
        folds
            .iter()
            .flat_map(|af| [af.range.start, af.range.end])
            .collect()
    };

    let Some(since) = carry_from else {
        let offsets = buffer.resolve_anchors_batch(&anchors());
        return (buffer.rope().offsets_to_points_batch(&offsets), offsets);
    };

    let edits = buffer.edits_since(since);
    let mut offsets: Vec<usize> = folds
        .iter()
        .flat_map(|af| [af.resolved_start, af.resolved_end])
        .collect();
    let mut needs_resolve = vec![false; offsets.len()];

    let mut order: Vec<usize> = (0..offsets.len()).collect();
    order.sort_unstable_by_key(|&i| offsets[i]);
    let mut sorted: Vec<usize> = order.iter().map(|&i| offsets[i]).collect();
    let mut sorted_flags = vec![false; sorted.len()];
    carry_offsets(&mut sorted, &mut sorted_flags, &edits);
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
    if !affected.is_empty() {
        let all = anchors();
        let to_resolve: Vec<Anchor> = affected.iter().map(|&i| all[i]).collect();
        for (&i, offset) in affected
            .iter()
            .zip(buffer.resolve_anchors_batch(&to_resolve))
        {
            offsets[i] = offset;
        }
    }

    let len = buffer.rope().len();
    for offset in &mut offsets {
        *offset = (*offset).min(len);
    }
    (buffer.rope().offsets_to_points_batch(&offsets), offsets)
}

/// The inlay rows whose fold coverage differs between two resolved fold sets,
/// as an identity patch the incremental sync can rebuild.
///
/// A fold toggle moves no inlay row, so the caller's edit patch says nothing
/// about it and the rows have to come from the sets themselves. Reading them
/// here rather than recording them when `fold` and `unfold` run is what covers
/// the cases no caller names. A sync drops folds whose anchors have collapsed,
/// and a fold overlapping another changes the region both paint even though
/// only one of them was added.
///
/// Both sides must be in the same row space, which holds only while the inlay
/// text is unchanged. That is the sole caller's condition.
fn fold_set_edits(old_folds: &SumTree<Fold>, new_folds: &SumTree<Fold>) -> Patch<u32> {
    let by_id = |folds: &SumTree<Fold>| -> HashMap<FoldId, Range<InlayPoint>> {
        folds.iter().map(|f| (f.id, f.range.clone())).collect()
    };
    let old_by_id = by_id(old_folds);
    let new_by_id = by_id(new_folds);

    let mut changed: Vec<Range<u32>> = Vec::new();
    let mut collect = |from: &HashMap<FoldId, Range<InlayPoint>>,
                       other: &HashMap<FoldId, Range<InlayPoint>>| {
        for (id, range) in from {
            if other.get(id) != Some(range) {
                changed.push(range.start.row()..range.end.row() + 1);
            }
        }
    };
    collect(&old_by_id, &new_by_id);
    collect(&new_by_id, &old_by_id);

    changed.sort_by_key(|rows| rows.start);
    let mut patch = Patch::default();
    for rows in changed {
        patch.push(stoat_text::patch::Edit {
            old: rows.clone(),
            new: rows,
        });
    }
    patch.consolidate();
    patch
}

/// `rows` widened until no fold crosses either end of it.
///
/// A fold becomes one transform, so a rebuild starting or stopping partway
/// through one would have to emit half a placeholder. Widening the rebuilt
/// span instead keeps every fold it touches whole, at the cost of revisiting
/// the rows the fold spans.
///
/// Widening can pull in a further fold that the original span did not reach,
/// so this repeats until the span stops growing.
fn rows_covering_folds(folds: &SumTree<Fold>, rows: Range<u32>) -> Range<u32> {
    let mut rows = rows;
    loop {
        let start = InlayPoint::new(rows.start, 0);
        let end = InlayPoint::new(rows.end, 0);
        // A fold ending exactly at the span's start counts as touching it. It
        // ends where the region begins rather than overlapping it, but the
        // rebuild still has to own it whole. Left out, the prefix slice stops short
        // of its placeholder and the gap fill re-emits its bytes as plain text,
        // while the strict filters downstream decline to re-emit the fold.
        // The totals balance either way, so nothing downstream would notice.
        //
        // Both tests have to allow it. The tree filter decides which folds the
        // loop below ever sees.
        let mut cursor = folds.filter::<_, FoldStart>((), |summary| {
            summary.max_end >= start && summary.min_start < end
        });

        let mut widened = rows.clone();
        for fold in &mut cursor {
            if fold.range.start >= end {
                break;
            }
            if fold.range.end >= start {
                widened.start = widened.start.min(fold.range.start.row());
                widened.end = widened.end.max(fold.range.end.row() + 1);
            }
        }

        if widened == rows {
            return rows;
        }
        rows = widened;
    }
}

fn sync_fold_incremental(
    old_snapshot: &FoldSnapshot,
    inlay_snapshot: &InlaySnapshot,
    inlay_edits: &Patch<u32>,
    resolved_folds: &SumTree<Fold>,
) -> (SumTree<Transform>, Patch<u32>) {
    let has_inlays = inlay_snapshot.has_inlays();
    let rope = inlay_snapshot.rope();
    let text_len = if has_inlays {
        inlay_snapshot.total_summary().len
    } else {
        rope.len()
    };

    let row_to_offset = |row: u32| -> usize {
        if has_inlays {
            inlay_snapshot.inlay_offset_at_row(row).0
        } else {
            rope.point_to_offset(Point::new(row, 0))
        }
    };

    let text_summary =
        |a: usize, b: usize| -> TextSummary { inlay_text_summary(inlay_snapshot, a..b) };

    // The incoming edits carry OLD row indices, and the cursor walks the old
    // transform tree in OLD input-offset space, so old rows must resolve through
    // the OLD inlay text. Converting them through `row_to_offset` (built over the
    // new text) overshoots after a mid-buffer insert and truncates the tail.
    let old_inlay = old_snapshot.inlay_snapshot();
    let old_has_inlays = old_inlay.has_inlays();
    let old_rope = old_inlay.rope();
    let old_text_len = if old_has_inlays {
        old_inlay.total_summary().len
    } else {
        old_rope.len()
    };
    let old_row_to_offset = |row: u32| -> usize {
        if old_has_inlays {
            old_inlay.inlay_offset_at_row(row).0
        } else {
            old_rope.point_to_offset(Point::new(row, 0))
        }
    };

    let mut new_transforms = SumTree::new(());
    let mut cursor = old_snapshot.transforms.cursor::<InputOffset>(());
    let mut row_edits = Patch::empty();

    // Every edit widened to whole folds, with any that meet merged into one
    // region, before a single row of it is rebuilt.
    //
    // The walk holds two positions, one in the old tree and one in what it has
    // built, and relies on them naming the same place: the old text from the
    // cursor to a region's old start is exactly the new text from what is built
    // to that region's new start. Widening breaks that on its own, since an
    // edit can reach back over rows its predecessor already rebuilt.
    //
    // Merging is what restores it. Pulling each range forward to the last row
    // covered instead cannot, because two widened edits can meet in one
    // coordinate space and not the other, and no single amount to move by
    // satisfies both.
    let regions = {
        let mut regions: Vec<(Range<u32>, Range<u32>)> = Vec::new();
        for edit in inlay_edits {
            let new_rows = rows_covering_folds(resolved_folds, edit.new.clone());
            let grew_start = edit.new.start - new_rows.start;
            let grew_end = new_rows.end - edit.new.end;
            let old_rows = edit.old.start.saturating_sub(grew_start)..edit.old.end + grew_end;
            match regions.last_mut() {
                Some((prev_old, prev_new))
                    if old_rows.start <= prev_old.end || new_rows.start <= prev_new.end =>
                {
                    prev_old.end = prev_old.end.max(old_rows.end);
                    prev_new.end = prev_new.end.max(new_rows.end);
                },
                _ => regions.push((old_rows, new_rows)),
            }
        }
        regions
    };

    for (index, (old_rows, new_rows)) in regions.iter().enumerate() {
        let (old_rows, new_rows) = (old_rows.clone(), new_rows.clone());
        let next_region_old_start = regions.get(index + 1).map(|(old, _)| old.start);

        let old_start_offset = old_row_to_offset(old_rows.start);
        let old_end_offset = old_row_to_offset(old_rows.end).min(old_text_len);

        // Preserve unchanged prefix
        new_transforms.append(cursor.slice(&InputOffset(old_start_offset), Bias::Left), ());

        // If cursor item ends exactly at edit start, merge it with prefix
        if let Some(item) = cursor.item()
            && item.placeholder_text.is_none()
            && cursor.start().0 + item.summary.input.len == old_start_offset
        {
            push_fold_isomorphic(&mut new_transforms, item.summary.clone());
            cursor.next();
        }

        // Record old output rows
        let old_fold_start = old_snapshot
            .to_fold_point(InlayPoint::new(old_rows.start, 0), Bias::Right)
            .row();
        let old_fold_end = if old_rows.start == old_rows.end {
            old_fold_start + 1
        } else if old_rows.end >= old_inlay.line_count() {
            // An end row one past the last names the end of the tree.
            // `to_fold_point` clamps a point past the extent back into range
            // rather than extrapolating, so ask the snapshot directly.
            old_snapshot.line_count()
        } else {
            old_snapshot
                .to_fold_point(InlayPoint::new(old_rows.end, 0), Bias::Right)
                .row()
                .max(old_fold_start + 1)
        };

        // Seek past old content
        cursor.seek_forward(&InputOffset(old_end_offset), Bias::Right);

        let new_start_offset = row_to_offset(new_rows.start);
        let new_end_offset = row_to_offset(new_rows.end).min(text_len);
        let folds_in_range: Vec<&Fold> = {
            let new_start_inlay = InlayPoint::new(new_rows.start, 0);
            let new_end_inlay = InlayPoint::new(new_rows.end, 0);
            let mut fold_cursor = resolved_folds.filter::<_, FoldStart>((), |summary| {
                summary.max_end > new_start_inlay && summary.min_start < new_end_inlay
            });
            let mut result = Vec::new();
            for fold in &mut fold_cursor {
                if fold.range.start >= new_end_inlay {
                    break;
                }
                if fold.range.end > new_start_inlay {
                    result.push(fold);
                }
            }
            result
        };
        let regions = merged_fold_regions(folds_in_range.iter().copied());

        let current_pos = new_transforms.summary().input.len;
        if new_start_offset > current_pos {
            let summary = text_summary(current_pos, new_start_offset);
            push_fold_isomorphic(
                &mut new_transforms,
                TransformSummary {
                    input: summary.clone(),
                    output: summary,
                },
            );
        }
        let new_fold_start = new_transforms.summary().output.lines.row;

        if regions.is_empty() {
            let current_pos = new_transforms.summary().input.len;
            if new_end_offset > current_pos {
                let summary = text_summary(current_pos, new_end_offset);
                push_fold_isomorphic(
                    &mut new_transforms,
                    TransformSummary {
                        input: summary.clone(),
                        output: summary,
                    },
                );
            }
        } else {
            let mut region_cursor = new_transforms.summary().input.len;
            for (fold, region) in regions {
                let fold_start_offset = inlay_snapshot
                    .inlay_point_to_offset(region.start)
                    .0
                    .min(text_len);
                let fold_end_offset = inlay_snapshot
                    .inlay_point_to_offset(region.end)
                    .0
                    .min(text_len);

                if fold_start_offset > region_cursor {
                    let summary = text_summary(region_cursor, fold_start_offset);
                    push_fold_isomorphic(
                        &mut new_transforms,
                        TransformSummary {
                            input: summary.clone(),
                            output: summary,
                        },
                    );
                }

                let input_summary = text_summary(fold_start_offset, fold_end_offset);
                let placeholder_text = fold.placeholder.display_text().clone();
                let output_summary = TextSummary::from_str(&placeholder_text);
                new_transforms.push(
                    Transform {
                        summary: TransformSummary {
                            input: input_summary,
                            output: output_summary,
                        },
                        placeholder_text: Some(placeholder_text),
                        fold_id: Some(fold.id),
                    },
                    (),
                );
                region_cursor = fold_end_offset;
            }

            if new_end_offset > region_cursor {
                let summary = text_summary(region_cursor, new_end_offset);
                push_fold_isomorphic(
                    &mut new_transforms,
                    TransformSummary {
                        input: summary.clone(),
                        output: summary,
                    },
                );
            }
        }

        let new_out = new_transforms.summary().output.lines;
        let new_fold_end = if new_rows.end >= inlay_snapshot.line_count() {
            // The mirror of the old end above. A region running to the end of
            // the tree has to name the tree's line count, and the accumulated
            // position stops at the last newline rather than counting the empty
            // row after it. `line_count` is that row plus one by definition, so
            // an appended row is reported rather than read as a same-size
            // change.
            new_out.row + 1
        } else if new_out.column > 0 {
            new_out.row + 1
        } else {
            new_out.row.max(new_fold_start + 1)
        };

        row_edits.push(stoat_text::patch::Edit {
            old: old_fold_start..old_fold_end,
            new: new_fold_start..new_fold_end,
        });

        // Handle tail of current transform
        if let Some(item) = cursor.item() {
            let cursor_end = cursor.start().0 + item.summary.input.len;

            // A fold beginning exactly where the region ends lies wholly
            // outside the rebuild, and the strict filter above left it out on
            // the grounds that the old tree still carries it. Leaving the
            // cursor on it is what makes that true, since the suffix append
            // below then takes it over whole. Re-emitting its bytes as
            // ordinary text unfolds it instead, and nothing puts it back.
            //
            // A fold the region ends inside is the opposite case and keeps the
            // re-emission. Only a fold that moved or was dropped can land
            // there, because a surviving one would have widened the region past
            // itself, so what remains of it is no longer folded text.
            let carried_whole =
                item.placeholder_text.is_some() && cursor.start().0 >= old_end_offset;

            if !carried_whole
                && next_region_old_start.is_none_or(|start| old_row_to_offset(start) >= cursor_end)
            {
                let tail = cursor_end - old_end_offset;
                let tail_end_new = new_end_offset + tail;
                let current_pos = new_transforms.summary().input.len;
                if tail_end_new > current_pos {
                    let summary = text_summary(current_pos, tail_end_new);
                    push_fold_isomorphic(
                        &mut new_transforms,
                        TransformSummary {
                            input: summary.clone(),
                            output: summary,
                        },
                    );
                }
                cursor.next();
            }
        }
    }

    new_transforms.append(cursor.suffix(), ());

    if new_transforms.is_empty() && text_len != 0 {
        let summary = inlay_text_summary(inlay_snapshot, 0..text_len);
        new_transforms.push(
            Transform {
                summary: TransformSummary {
                    input: summary.clone(),
                    output: summary,
                },
                placeholder_text: None,
                fold_id: None,
            },
            (),
        );
    }

    row_edits.consolidate();
    (new_transforms, row_edits)
}

fn point_overshoot(base: Point, target: Point) -> Point {
    if target.row == base.row {
        Point::new(0, target.column - base.column)
    } else {
        Point::new(target.row - base.row, target.column)
    }
}

impl Deref for FoldSnapshot {
    type Target = InlaySnapshot;
    fn deref(&self) -> &InlaySnapshot {
        &self.inlay_snapshot
    }
}

impl FoldSnapshot {
    pub fn inlay_snapshot(&self) -> &InlaySnapshot {
        &self.inlay_snapshot
    }

    pub fn version(&self) -> usize {
        self.version
    }

    pub fn len(&self) -> FoldOffset {
        FoldOffset(self.transforms.summary().output.len)
    }

    pub fn fold_metadata(&self, id: &FoldId) -> Option<&FoldMetadata> {
        self.fold_metadata_by_id.get(id)
    }

    pub fn fold_id_at_point(&self, fold_point: FoldPoint) -> Option<FoldId> {
        let (_, _, item) = self.transforms.find::<Dimensions<FoldPoint, FoldPoint>, _>(
            (),
            &fold_point,
            Bias::Right,
        );
        item.and_then(|t| t.fold_id)
    }

    /// The fold point `inlay_point` renders at, resolving a point inside a fold
    /// to whichever side `bias` asks for.
    ///
    /// A folded span occupies one placeholder in fold space, so every inlay
    /// point within it has to collapse to one edge or the other. `Bias::Left`
    /// takes the fold's start, which is what puts leftward motion across a fold
    /// on the near side of it rather than past it.
    ///
    /// The transform lookup always seeks `Bias::Right`, so that `start` names
    /// the transform the point falls in. Seeking with the caller's bias would
    /// land on the previous transform at a fold boundary, and the branch below
    /// would then answer from the wrong fold.
    pub fn to_fold_point(&self, inlay_point: InlayPoint, bias: Bias) -> FoldPoint {
        let (start, end, item) = self
            .transforms
            .find::<Dimensions<InlayPoint, FoldPoint>, _>((), &inlay_point, Bias::Right);
        match item {
            Some(t) if t.placeholder_text.is_some() => {
                if bias == Bias::Left || inlay_point.0 == start.0 .0 {
                    start.1
                } else {
                    end.1
                }
            },
            Some(_) | None => {
                let overshoot = point_overshoot(start.0 .0, inlay_point.0);
                // A column past the end of the line the transform covers would
                // otherwise carry straight through into the next one.
                FoldPoint((start.1 .0 + overshoot).min(end.1 .0))
            },
        }
    }

    pub fn to_inlay_point(&self, fold_point: FoldPoint) -> InlayPoint {
        let (start, _end, item) = self
            .transforms
            .find::<Dimensions<InlayPoint, FoldPoint>, _>((), &fold_point, Bias::Right);
        match item {
            Some(t) if t.placeholder_text.is_some() => start.0,
            Some(_) | None => {
                let overshoot = point_overshoot(start.1 .0, fold_point.0);
                InlayPoint(start.0 .0 + overshoot)
            },
        }
    }

    /// Move `point` to the nearest position a caret can occupy, in the
    /// direction `bias` asks for.
    ///
    /// Walks the transforms rather than clipping in inlay space, because inlay
    /// space cannot express "inside a fold": every point within one resolves to
    /// the fold's start there, so a round trip would discard which side the
    /// caller was on before the bias could be applied.
    pub fn clip_point(&self, point: FoldPoint, bias: Bias) -> FoldPoint {
        let (start, end, item) = self
            .transforms
            .find::<Dimensions<InlayPoint, FoldPoint>, _>((), &point, Bias::Right);
        let Some(transform) = item else {
            return FoldPoint(self.transforms.summary().output.lines);
        };

        if transform.placeholder_text.is_some() {
            // A fold is one indivisible position, so a caret lands on an edge.
            if bias == Bias::Left || point.0 == start.1 .0 {
                start.1
            } else {
                end.1
            }
        } else {
            let overshoot = point_overshoot(start.1 .0, point.0);
            let inlay_point = InlayPoint(start.0 .0 + overshoot);
            let clipped = self.inlay_snapshot.clip_point(inlay_point, bias);
            FoldPoint(start.1 .0 + point_overshoot(start.0 .0, clipped.0))
        }
    }

    pub fn fold_count(&self) -> usize {
        self.folds.summary().count
    }

    /// Buffer row the earliest fold starts on, or `None` when nothing is
    /// folded.
    ///
    /// A fold moves everything after it, so a caller whose shortcut assumes
    /// display positions follow the buffer's can keep taking it for rows above
    /// this one. Read off the fold summary rather than by walking, so asking is
    /// as cheap as [`Self::fold_count`].
    pub fn first_fold_row(&self) -> Option<u32> {
        let summary = self.folds.summary();
        (summary.count > 0).then(|| self.inlay_snapshot.to_buffer_point(summary.min_start).row)
    }

    /// Byte offset of the start of `fold_row` in fold-offset space.
    ///
    /// Returns the snapshot's total length if `fold_row` is past the last
    /// row. Used by higher layers to translate row-based ranges into the
    /// byte-offset ranges accepted by [`FoldSnapshot::chunks`].
    pub fn row_start_offset(&self, fold_row: u32) -> FoldOffset {
        if fold_row == 0 {
            return FoldOffset(0);
        }
        let line_count = self.line_count();
        if fold_row >= line_count {
            return self.len();
        }
        // With no folds, fold space mirrors inlay space one-to-one, so the inlay
        // layer resolves the row's offset in O(log n) through its rope cursor.
        if self.fold_count() == 0 {
            return FoldOffset(self.inlay_snapshot.inlay_offset_at_row(fold_row).0);
        }

        // With folds, seek the transform tree to the row and convert the
        // isomorphic overshoot through the inlay layer, resolving the offset in
        // O(log n) rather than walking every preceding row's characters. Bias
        // right so a row start on a transform boundary lands on the transform
        // beginning there, with zero overshoot.
        let target = FoldPoint::new(fold_row, 0);
        let (start, _, item) = self
            .transforms
            .find::<Dimensions<FoldPoint, TransformSummary>, _>((), &target, Bias::Right);
        let overshoot = point_overshoot(start.0 .0, target.0);
        let mut offset = start.1.output.len;
        if overshoot != Point::zero() {
            let transform = item.expect("a fold row within range sits in a transform");
            assert!(
                transform.placeholder_text.is_none(),
                "a column-0 fold point with row overshoot cannot fall inside a placeholder",
            );
            let inlay_point = InlayPoint(start.1.input.lines + overshoot);
            offset += self.inlay_snapshot.inlay_point_to_offset(inlay_point).0 - start.1.input.len;
        }
        FoldOffset(offset)
    }

    /// Stream [`Chunk`]s covering `range` in fold-offset space.
    ///
    /// Walks the fold transform tree and interleaves chunks from the inlay
    /// layer (for isomorphic segments) with placeholder text (for folds).
    /// Fold placeholders are emitted as a single unstyled chunk with a
    /// [`ChunkRenderer`] id attached.
    ///
    /// Fast path: when the snapshot has zero folds, delegates directly to
    /// [`InlaySnapshot::chunks`] without any transform cursor work.
    pub fn chunks<'a>(
        &'a self,
        range: Range<FoldOffset>,
        endpoints: Arc<[HighlightEndpoint]>,
    ) -> FoldChunks<'a> {
        if self.fold_count() == 0 {
            // Without folds, fold offsets equal inlay offsets.
            return FoldChunks::Passthrough(Box::new(self.inlay_snapshot.chunks(
                InlayOffset(range.start.0)..InlayOffset(range.end.0),
                endpoints,
            )));
        }

        let mut cursor = self
            .transforms
            .cursor::<Dimensions<FoldOffset, InputOffset>>(());
        cursor.seek(&range.start, Bias::Right);

        FoldChunks::Transforming(Box::new(FoldChunksInner {
            snapshot: self,
            endpoints,
            cursor,
            inlay_chunks: None,
            offset: range.start,
            end: range.end,
        }))
    }

    pub fn is_line_folded(&self, inlay_row: u32) -> bool {
        let row_start = InlayPoint::new(inlay_row, 0);
        let row_end = InlayPoint::new(inlay_row, u32::MAX);
        let mut cursor = self.folds.filter::<_, FoldStart>((), |summary| {
            summary.max_end >= row_start && summary.min_start <= row_end
        });
        for fold in &mut cursor {
            if fold.range.start.row() > inlay_row {
                return false;
            }
            if fold.range.end.row() >= inlay_row
                && (fold.range.start.row() != fold.range.end.row()
                    || fold.range.start.column() != fold.range.end.column())
            {
                return true;
            }
        }
        false
    }

    pub fn max_point(&self) -> FoldPoint {
        self.transforms.extent(())
    }

    pub fn line_count(&self) -> u32 {
        let extent: FoldPoint = self.transforms.extent(());
        extent.row() + 1
    }

    pub fn fold_line_chars(&self, fold_row: u32) -> FoldLineChars<'_> {
        FoldLineChars {
            inner: self.chars_at(FoldPoint::new(fold_row, 0)),
        }
    }

    /// Inlay-expanded byte length of `fold_row`'s content, excluding the
    /// trailing newline.
    ///
    /// Answers the same width as [`Self::line_len`], which also counts inlay
    /// text. Without folds a fold offset equals an inlay offset, so the inlay
    /// layer's own length is exact and cheaper than seeking the transform tree
    /// twice.
    pub fn output_line_len(&self, fold_row: u32) -> u32 {
        if self.fold_count() == 0 {
            return self.inlay_snapshot.line_len(fold_row);
        }
        self.line_len(fold_row)
    }

    /// Byte length of `fold_row`'s painted content, excluding its newline.
    ///
    /// Measures the span between two row starts rather than decoding the row's
    /// characters, so it costs two O(log n) tree seeks however long the row is
    /// and however many folds it crosses.
    ///
    /// Fold transforms are summarized over the inlay-expanded text, so this
    /// counts hint text the way the chunk stream paints it. Walking the
    /// characters could not. They come from the buffer rope, which no inlay
    /// appears in.
    pub fn line_len(&self, fold_row: u32) -> u32 {
        let start = self.row_start_offset(fold_row).0;
        let end = if fold_row + 1 < self.line_count() {
            // The next row starts one past the newline that ends this one.
            self.row_start_offset(fold_row + 1).0.saturating_sub(1)
        } else {
            self.len().0
        };
        end.saturating_sub(start) as u32
    }

    pub fn folds_in_range(&self, range: Range<InlayPoint>) -> Vec<&Fold> {
        let mut cursor = self.folds.filter::<_, FoldStart>((), |summary| {
            summary.max_end > range.start && summary.min_start < range.end
        });
        let mut result = Vec::new();
        for fold in &mut cursor {
            if fold.range.start >= range.end {
                break;
            }
            if fold.range.end > range.start {
                result.push(fold);
            }
        }
        result
    }

    pub fn chars_at(&self, fold_point: FoldPoint) -> FoldChars<'_> {
        let inlay_point = self.to_inlay_point(fold_point);
        let buffer_point = self.inlay_snapshot.to_buffer_point(inlay_point);
        let rope = self.inlay_snapshot.rope();
        let buffer_offset = rope.point_to_offset(buffer_point);
        let chars = rope.chars_at(buffer_offset);

        // The position never lands inside a fold, so every fold ending past
        // `buffer_offset` also starts at or after it. Seek there once and read
        // folds from the cursor lazily instead of cloning the whole tail.
        let mut folds = self.folds.cursor::<FoldStart>(());
        folds.seek(&FoldStart(inlay_point), Bias::Left);
        let next_fold_start_offset = folds.item().map_or(usize::MAX, |f| {
            rope.point_to_offset(self.inlay_snapshot.to_buffer_point(f.range.start))
        });

        FoldChars {
            inlay_snapshot: &self.inlay_snapshot,
            rope,
            chars,
            buffer_offset,
            folds,
            next_fold_start_offset,
            placeholder_iter: None,
        }
    }

    pub fn reversed_chars_at(&self, fold_point: FoldPoint) -> ReversedFoldChars<'_> {
        let inlay_point = self.to_inlay_point(fold_point);
        let buffer_point = self.inlay_snapshot.to_buffer_point(inlay_point);
        let rope = self.inlay_snapshot.rope();
        let buffer_offset = rope.point_to_offset(buffer_point);
        let chars = rope.reversed_chars_at(buffer_offset);

        // Seek to the first fold at or after the position, then step back to the
        // closest preceding fold. Its predecessors are reached lazily with
        // `prev()`, sparing the up-front clone of every preceding fold.
        let mut folds = self.folds.cursor::<FoldStart>(());
        folds.seek(&FoldStart(inlay_point), Bias::Left);
        folds.prev();
        let next_fold_end_offset = folds.item().map_or(0, |f| {
            rope.point_to_offset(self.inlay_snapshot.to_buffer_point(f.range.end))
        });

        ReversedFoldChars {
            inlay_snapshot: &self.inlay_snapshot,
            rope,
            chars,
            buffer_offset,
            folds,
            next_fold_end_offset,
            placeholder_iter: None,
        }
    }

    pub fn fold_line(&self, fold_row: u32) -> String {
        self.fold_line_chars(fold_row).collect()
    }

    pub fn fold_point_cursor(&self) -> FoldPointCursor<'_> {
        FoldPointCursor {
            cursor: self
                .transforms
                .cursor::<Dimensions<InlayPoint, FoldPoint>>(()),
        }
    }
}

/// Iterator returned by [`FoldSnapshot::chunks`].
pub enum FoldChunks<'a> {
    /// Snapshot has no folds; this is a thin wrapper around [`InlayChunks`].
    Passthrough(Box<InlayChunks<'a>>),
    /// Snapshot has at least one fold; walks transforms to interleave
    /// placeholder chunks with inlay chunks.
    Transforming(Box<FoldChunksInner<'a>>),
}

#[doc(hidden)]
pub struct FoldChunksInner<'a> {
    snapshot: &'a FoldSnapshot,
    endpoints: Arc<[HighlightEndpoint]>,
    cursor: Cursor<'a, 'static, Transform, Dimensions<FoldOffset, InputOffset>>,
    inlay_chunks: Option<InlayChunks<'a>>,
    offset: FoldOffset,
    end: FoldOffset,
}

impl<'a> Iterator for FoldChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        match self {
            FoldChunks::Passthrough(inner) => inner.next(),
            FoldChunks::Transforming(inner) => inner.next(),
        }
    }
}

impl<'a> FoldChunksInner<'a> {
    fn next(&mut self) -> Option<Chunk<'a>> {
        loop {
            if self.offset >= self.end {
                return None;
            }

            if let Some(ic) = self.inlay_chunks.as_mut() {
                if let Some(chunk) = ic.next() {
                    self.offset.0 += chunk.text.len();
                    return Some(chunk);
                }
                self.inlay_chunks = None;
                self.cursor.next();
                continue;
            }

            let transform = self.cursor.item()?;
            let cursor_start = self.cursor.start();
            let cursor_end = self.cursor.end();
            let trans_start_fold = cursor_start.0;
            let trans_end_fold = cursor_end.0;
            let trans_start_inlay = cursor_start.1 .0;

            if trans_start_fold.0 >= self.end.0 {
                return None;
            }

            if let Some(placeholder_text) = transform.placeholder_text.as_ref() {
                // Emit placeholder text as a single chunk. Placeholders span the
                // entire transform in fold-offset space regardless of how many
                // inlay-side bytes they collapse.
                let text: &'a str = placeholder_text;
                let fold_id = transform.fold_id;
                let trans_end = trans_end_fold;
                self.cursor.next();
                self.offset = trans_end;
                return Some(Chunk {
                    text: Cow::Borrowed(text),
                    highlight_style: None,
                    renderer: fold_id.map(|id| super::highlights::ChunkRenderer {
                        id: super::highlights::ChunkRendererId::Fold(id.0),
                    }),
                    ..Default::default()
                });
            }

            // Isomorphic transform: compute the inlay range that corresponds
            // to the clipped fold range, then delegate to InlayChunks.
            let local_start_fold = self.offset.0.max(trans_start_fold.0);
            let local_end_fold = self.end.0.min(trans_end_fold.0);
            let local_start_inlay = trans_start_inlay + (local_start_fold - trans_start_fold.0);
            let local_end_inlay = trans_start_inlay + (local_end_fold - trans_start_fold.0);
            self.inlay_chunks = Some(self.snapshot.inlay_snapshot.chunks(
                InlayOffset(local_start_inlay)..InlayOffset(local_end_inlay),
                self.endpoints.clone(),
            ));
        }
    }
}

pub struct FoldPointCursor<'a> {
    cursor: Cursor<'a, 'static, Transform, Dimensions<InlayPoint, FoldPoint>>,
}

impl FoldPointCursor<'_> {
    /// The cursor-held equivalent of [`FoldSnapshot::to_fold_point`], for a
    /// caller mapping a run of ascending points. Same semantics.
    pub fn map(&mut self, inlay_point: InlayPoint, bias: Bias) -> FoldPoint {
        if self.cursor.did_seek() {
            self.cursor.seek_forward(&inlay_point, Bias::Right);
        } else {
            self.cursor.seek(&inlay_point, Bias::Right);
        }
        let start = *self.cursor.start();
        let end = self.cursor.end();
        match self.cursor.item() {
            Some(t) if t.placeholder_text.is_some() => {
                if bias == Bias::Left || inlay_point.0 == start.0 .0 {
                    start.1
                } else {
                    end.1
                }
            },
            Some(_) | None => {
                let overshoot = point_overshoot(start.0 .0, inlay_point.0);
                FoldPoint((start.1 .0 + overshoot).min(end.1 .0))
            },
        }
    }
}

pub struct FoldChars<'a> {
    inlay_snapshot: &'a InlaySnapshot,
    rope: &'a Rope,
    chars: CharsAt<'a>,
    buffer_offset: usize,
    folds: Cursor<'a, 'a, Fold, FoldStart>,
    next_fold_start_offset: usize,
    placeholder_iter: Option<std::vec::IntoIter<char>>,
}

impl Iterator for FoldChars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if let Some(ref mut iter) = self.placeholder_iter {
            if let Some(ch) = iter.next() {
                return Some(ch);
            }
            self.placeholder_iter = None;
        }

        if self.buffer_offset >= self.next_fold_start_offset {
            let fold = self.folds.item().expect("fold present at its start offset");
            let end = self.inlay_snapshot.to_buffer_point(fold.range.end);
            let end_off = self.rope.point_to_offset(end);
            let placeholder_chars: Vec<char> = fold.placeholder.display_text().chars().collect();
            self.folds.next();
            self.next_fold_start_offset = self.folds.item().map_or(usize::MAX, |f| {
                self.rope
                    .point_to_offset(self.inlay_snapshot.to_buffer_point(f.range.start))
            });
            self.placeholder_iter = Some(placeholder_chars.into_iter());
            self.chars = self.rope.chars_at(end_off);
            self.buffer_offset = end_off;
            return self.next();
        }

        let ch = self.chars.next()?;
        self.buffer_offset += ch.len_utf8();
        Some(ch)
    }
}

pub struct ReversedFoldChars<'a> {
    inlay_snapshot: &'a InlaySnapshot,
    rope: &'a Rope,
    chars: ReversedCharsAt<'a>,
    buffer_offset: usize,
    folds: Cursor<'a, 'a, Fold, FoldStart>,
    next_fold_end_offset: usize,
    placeholder_iter: Option<std::vec::IntoIter<char>>,
}

impl Iterator for ReversedFoldChars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if let Some(ref mut iter) = self.placeholder_iter {
            if let Some(ch) = iter.next() {
                return Some(ch);
            }
            self.placeholder_iter = None;
        }

        if self.folds.item().is_some() && self.buffer_offset <= self.next_fold_end_offset {
            let fold = self.folds.item().expect("fold present at its end offset");
            let start = self.inlay_snapshot.to_buffer_point(fold.range.start);
            let start_off = self.rope.point_to_offset(start);
            let placeholder_chars: Vec<char> =
                fold.placeholder.display_text().chars().rev().collect();
            self.folds.prev();
            self.next_fold_end_offset = self.folds.item().map_or(0, |f| {
                self.rope
                    .point_to_offset(self.inlay_snapshot.to_buffer_point(f.range.end))
            });
            self.placeholder_iter = Some(placeholder_chars.into_iter());
            self.chars = self.rope.reversed_chars_at(start_off);
            self.buffer_offset = start_off;
            return self.next();
        }

        let ch = self.chars.next()?;
        self.buffer_offset -= ch.len_utf8();
        Some(ch)
    }
}

pub struct FoldLineChars<'a> {
    inner: FoldChars<'a>,
}

impl Iterator for FoldLineChars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        match self.inner.next()? {
            '\n' => None,
            ch => Some(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FoldId, FoldMap, FoldOffset, FoldPlaceholder, FoldPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::inlay_map::{InlayKind, InlayMap, InlayPoint},
        multi_buffer::MultiBuffer,
    };
    use std::{
        ops::Range,
        sync::{Arc, RwLock},
    };
    use stoat_text::{patch::Patch, Anchor, Bias};

    fn make_snapshot(content: &str) -> Arc<super::FoldSnapshot> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        fold_snapshot
    }

    fn make_snapshot_with_folds(
        content: &str,
        fold_ranges: Vec<(InlayPoint, InlayPoint)>,
    ) -> Arc<super::FoldSnapshot> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());
        let anchor_ranges = fold_ranges
            .into_iter()
            .map(|(start, end)| {
                let s_buf = inlay_snapshot.to_buffer_point(start);
                let e_buf = inlay_snapshot.to_buffer_point(end);
                let s_off = buffer_snapshot.rope().point_to_offset(s_buf);
                let e_off = buffer_snapshot.rope().point_to_offset(e_buf);
                buffer_snapshot.anchor_at(s_off, Bias::Right)
                    ..buffer_snapshot.anchor_at(e_off, Bias::Left)
            })
            .collect();
        fold_map.fold(anchor_ranges, FoldPlaceholder::default(), &buffer_snapshot);
        let (snapshot, _) = fold_map.sync(inlay_snapshot, &Patch::empty());
        snapshot
    }

    /// Prime a fold map on `content`, apply one buffer edit, and drive the
    /// incremental sync path. Returns the resynced snapshot and the new inlay
    /// text length in bytes.
    fn fold_snapshot_after_edit(
        content: &str,
        edit: Range<usize>,
        insert: &str,
    ) -> (Arc<super::FoldSnapshot>, usize) {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let snap0 = multi_buffer.snapshot();
        let (mut inlay_map, inlay_snap0) = InlayMap::new(snap0.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snap0);

        {
            let mut buf = shared.write().unwrap();
            buf.edit(edit, insert);
        }

        let snap1 = multi_buffer.snapshot();
        let new_len = snap1.rope().len();
        let buffer_edits = snap1.edits_since(snap0.version());
        let (inlay_snap1, inlay_edits) = inlay_map.sync(snap1, &buffer_edits);
        let (fold_snap1, _) = fold_map.sync(inlay_snap1, &inlay_edits);
        (fold_snap1, new_len)
    }

    /// A fold whose end sits on the first row a rebuild touches survives it.
    ///
    /// The rebuild span is widened so no fold is cut in half, since a fold is
    /// one transform. A fold ending exactly at the span's start row is the case
    /// the widening has to reach and the one it is easiest to miss, because the
    /// fold ends where the region begins rather than overlapping it. Missing it
    /// re-emits the folded bytes as ordinary text while the fold is still in the
    /// set, and the byte totals balance either way, so nothing else notices.
    #[test]
    fn a_fold_ending_on_the_rebuilt_rows_survives_an_edit_there() {
        let content = "aaaa\nbbbb\ncccc\ndddd\n";
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let snap0 = multi_buffer.snapshot();

        let fold_range = |snap: &crate::MultiBufferSnapshot| {
            let start = snap.rope().point_to_offset(stoat_text::Point::new(0, 3));
            let end = snap.rope().point_to_offset(stoat_text::Point::new(2, 0));
            snap.anchor_at(start, Bias::Right)..snap.anchor_at(end, Bias::Left)
        };

        let (mut inlay_map, inlay_snap0) = InlayMap::new(snap0.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snap0.clone());
        fold_map.fold(vec![fold_range(&snap0)], FoldPlaceholder::default(), &snap0);
        fold_map.sync(inlay_snap0, &Patch::empty());

        // A same-size edit on row 2, the row the fold ends at, so the rebuild
        // span starts exactly where the fold stops.
        let at = snap0.rope().point_to_offset(stoat_text::Point::new(2, 1));
        shared.write().expect("poisoned").edit(at..at + 1, "X");

        let after = multi_buffer.snapshot();
        let buffer_edits = after.edits_since(snap0.version());
        let (inlay_snap1, inlay_edits) = inlay_map.sync(after.clone(), &buffer_edits);
        let (patched, _) = fold_map.sync(inlay_snap1, &inlay_edits);

        // The same fold over the same final text, with nothing to patch against.
        let (_, fresh_inlay) = InlayMap::new(after.clone());
        let (mut fresh_map, _) = FoldMap::new(fresh_inlay.clone());
        fresh_map.fold(vec![fold_range(&after)], FoldPlaceholder::default(), &after);
        let (fresh, _) = fresh_map.sync(fresh_inlay, &Patch::empty());

        let rows = |snap: &super::FoldSnapshot| -> Vec<String> {
            (0..snap.line_count()).map(|r| snap.fold_line(r)).collect()
        };
        assert_eq!(
            rows(&patched),
            rows(&fresh),
            "the fold stays folded rather than being repainted as plain text",
        );
    }

    #[test]
    fn incremental_insert_keeps_the_trailing_rows() {
        let (snap, new_len) = fold_snapshot_after_edit("aaaa\nbbbb\ncccc\ndddd\n", 6..6, "x");
        assert_eq!(
            snap.line_count(),
            5,
            "all five rows survive a mid-buffer insert"
        );
        assert_eq!(
            snap.transforms.summary().input.len,
            new_len,
            "the transform tree covers the full new text",
        );
    }

    #[test]
    fn row_start_offset_matches_char_walk_across_shared_fold_row() {
        // A fold inside the first line leaves the following isomorphic transform
        // starting mid-inlay-row, the case a prior row-count formula mishandled.
        let snap = make_snapshot_with_folds(
            "hello world foo\nsecond line\nthird line\n",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );

        let char_walk = |row: u32| -> usize {
            (0..row)
                .map(|r| snap.fold_line_chars(r).map(|c| c.len_utf8()).sum::<usize>() + 1)
                .sum()
        };

        for row in 0..snap.line_count() {
            assert_eq!(
                snap.row_start_offset(row).0,
                char_walk(row),
                "fold row {row} start offset must match the char-walk oracle",
            );
        }
    }

    #[test]
    fn incremental_delete_keeps_the_trailing_rows() {
        // Delete the newline after "bbbb", joining two rows: five rows become
        // four. The rows below the edit must all survive the rebuild.
        let (snap, new_len) = fold_snapshot_after_edit("aaaa\nbbbb\ncccc\ndddd\n", 9..10, "");
        assert_eq!(
            snap.line_count(),
            4,
            "the rows below a mid-buffer delete survive"
        );
        assert_eq!(
            snap.transforms.summary().input.len,
            new_len,
            "the transform tree covers the full new text",
        );
    }

    #[test]
    fn passthrough_no_folds() {
        let snap = make_snapshot("hello\nworld\nfoo");
        let point = InlayPoint::new(1, 3);
        let fold = snap.to_fold_point(point, Bias::Right);
        assert_eq!(fold, FoldPoint::new(1, 3));
        let back = snap.to_inlay_point(fold);
        assert_eq!(back, point);
    }

    #[test]
    fn single_line_fold() {
        // "hello world foo" with fold at columns 5..11 -> "hello... foo"
        let snap = make_snapshot_with_folds(
            "hello world foo",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(0, 0), Bias::Right),
            FoldPoint::new(0, 0)
        );
        // After fold: col 5 + 3 ("...") = 8
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(0, 11), Bias::Right),
            FoldPoint::new(0, 8)
        );
        // col 15 -> 15 - 6 + 3 = 12
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(0, 15), Bias::Right),
            FoldPoint::new(0, 12)
        );
    }

    #[test]
    fn single_line_fold_bias_left_at_boundary() {
        let snap = make_snapshot_with_folds(
            "hello world foo",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );
        // At fold end with Bias::Left → inside placeholder
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(0, 11), Bias::Left),
            FoldPoint::new(0, 8)
        );
        // At fold end with Bias::Right → after placeholder
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(0, 11), Bias::Right),
            FoldPoint::new(0, 8)
        );
    }

    /// A folded span is one position on screen, so a caret arriving from the
    /// left has to stop before it and one arriving from the right after it.
    /// Answering the same side to both walks the caret straight over the fold.
    #[test]
    fn a_point_inside_a_fold_resolves_to_the_side_its_bias_asks_for() {
        // "hello world foo" with cols 5..11 folded, painting "hello... foo".
        let snap = make_snapshot_with_folds(
            "hello world foo",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );

        let inside = InlayPoint::new(0, 8);
        assert_eq!(
            snap.to_fold_point(inside, Bias::Left),
            FoldPoint::new(0, 5),
            "leftward motion stops at the fold's near side",
        );
        assert_eq!(
            snap.to_fold_point(inside, Bias::Right),
            FoldPoint::new(0, 8),
            "rightward motion passes to its far side",
        );

        // Clipping a fold point landing mid-placeholder has to make the same
        // choice, which it cannot do by way of inlay space.
        let mid_placeholder = FoldPoint::new(0, 6);
        assert_eq!(
            snap.clip_point(mid_placeholder, Bias::Left),
            FoldPoint::new(0, 5),
        );
        assert_eq!(
            snap.clip_point(mid_placeholder, Bias::Right),
            FoldPoint::new(0, 8),
        );
    }

    /// Overshoot must not carry a point past the transform it landed in, and a
    /// clipped point must not sit past the end of its row.
    #[test]
    fn overshoot_past_a_folded_row_stays_in_range() {
        // Row 0 is "hello world foo" with cols 5..11 folded, painting
        // "hello... foo" across 12 columns. Row 1 is "second".
        let snap = make_snapshot_with_folds(
            "hello world foo\nsecond",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );
        let max = snap.max_point();
        assert_eq!(max, FoldPoint::new(1, 6));

        assert_eq!(
            snap.to_fold_point(InlayPoint::new(9, 0), Bias::Right),
            max,
            "a row past the last one stops at the end rather than extrapolating",
        );

        assert_eq!(
            snap.clip_point(FoldPoint::new(0, 400), Bias::Left),
            FoldPoint::new(0, 12),
            "a column past a folded row's end clips to it",
        );
        assert_eq!(
            snap.clip_point(FoldPoint::new(1, 400), Bias::Left),
            max,
            "and so does one past an unfolded row",
        );
    }

    #[test]
    fn chars_at_lazily_substitutes_folds() {
        // "hello world foo" folds cols 5..11 -> "hello... foo".
        let snap = make_snapshot_with_folds(
            "hello world foo",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );

        let forward: String = snap.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(
            forward, "hello... foo",
            "placeholder substituted, folded range skipped"
        );

        let after_fold: String = snap.chars_at(FoldPoint::new(0, 8)).collect();
        assert_eq!(
            after_fold, " foo",
            "a start past the fold seeks the cursor correctly"
        );

        let reversed: String = snap.reversed_chars_at(FoldPoint::new(0, 12)).collect();
        assert_eq!(
            reversed,
            forward.chars().rev().collect::<String>(),
            "the reverse walk mirrors the forward stream"
        );

        let before_fold: String = snap.reversed_chars_at(FoldPoint::new(0, 5)).collect();
        assert_eq!(
            before_fold, "olleh",
            "a reverse start before the fold yields the prefix"
        );
    }

    #[test]
    fn multi_line_fold() {
        // "line0\nline1\nline2\nline3" fold (1,0)..(2,5)
        let snap = make_snapshot_with_folds(
            "line0\nline1\nline2\nline3",
            vec![(InlayPoint::new(1, 0), InlayPoint::new(2, 5))],
        );
        assert_eq!(snap.line_count(), 3);
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(0, 3), Bias::Right),
            FoldPoint::new(0, 3)
        );
        // Point inside fold maps to fold placeholder end
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(1, 2), Bias::Right),
            FoldPoint::new(1, 3)
        );
        // line3 shifts from row 3 to row 2
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(3, 2), Bias::Right),
            FoldPoint::new(2, 2)
        );
    }

    #[test]
    fn fold_then_unfold() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let start_off = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(1, 0));
        let end_off = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(1, 5));
        let anchor_range = buffer_snapshot.anchor_at(start_off, Bias::Right)
            ..buffer_snapshot.anchor_at(end_off, Bias::Left);
        fold_map.fold(
            vec![anchor_range],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (snap, _) = fold_map.sync(inlay_snapshot.clone(), &Patch::empty());
        assert_eq!(snap.line_count(), 3);

        #[allow(clippy::single_range_in_vec_init)]
        fold_map.unfold(vec![start_off..end_off], &buffer_snapshot);
        let (snap, _) = fold_map.sync(inlay_snapshot, &Patch::empty());
        assert_eq!(snap.line_count(), 3);
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(1, 3), Bias::Right),
            FoldPoint::new(1, 3)
        );
    }

    /// Fold `line1`'s five columns behind `placeholder` and report the row's
    /// measured width, the text the chunk stream paints, and where the column
    /// just past the fold converts to and back.
    fn folded_row_geometry(placeholder: FoldPlaceholder) -> (u32, String, FoldPoint, InlayPoint) {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let start_off = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(1, 0));
        let end_off = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(1, 5));
        let anchor_range = buffer_snapshot.anchor_at(start_off, Bias::Right)
            ..buffer_snapshot.anchor_at(end_off, Bias::Left);
        fold_map.fold(vec![anchor_range], placeholder, &buffer_snapshot);
        let (snap, _) = fold_map.sync(inlay_snapshot, &Patch::empty());

        let painted: String = snap
            .chunks(
                snap.row_start_offset(1)..snap.row_start_offset(2),
                Arc::from(Vec::new()),
            )
            .map(|chunk| chunk.text.to_string())
            .collect();

        // The inlay point just past the folded span, which is where the
        // transform's measured width decides the fold row's column.
        let past_fold = InlayPoint::new(1, 5);
        let fold_point = snap.to_fold_point(past_fold, Bias::Right);
        (
            snap.line_len(1),
            painted,
            fold_point,
            snap.to_inlay_point(fold_point),
        )
    }

    /// A deterministic pseudo-random walk, so the edits and fold sets below
    /// land in shapes nobody hand-picked while staying reproducible.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }

    /// Carrying the cached resolved offsets across a buffer edit has to land
    /// exactly where re-resolving every anchor would. It is an arithmetic
    /// shortcut past the anchor trees, so a discrepancy would misplace folds
    /// with nothing else to catch it.
    #[test]
    fn carrying_offsets_across_a_multi_edit_patch() {
        // 5..8 shrinks by one, so everything past it stands at -1. The insert's
        // new range, 19..24, is stated with that -1 already applied, so its own
        // ends give +4 as the running total rather than as a further step.
        let edits = Patch::new(vec![
            stoat_text::patch::Edit {
                old: 5..8,
                new: 5..7,
            },
            stoat_text::patch::Edit {
                old: 20..20,
                new: 19..24,
            },
        ]);
        let mut offsets = [0, 4, 5, 12, 20, 30];
        let mut needs_resolve = [false; 6];
        super::carry_offsets(&mut offsets, &mut needs_resolve, &edits);

        assert_eq!(
            offsets,
            [0, 4, 5, 11, 19, 34],
            "the last one takes +4, which summing the two would make +3",
        );
        assert_eq!(
            needs_resolve,
            [false, false, true, false, true, false],
            "an offset an edit reaches, or an insertion lands exactly on, is \
             sent back to its anchor rather than trusted to the carry",
        );
    }

    #[test]
    fn carried_fold_offsets_match_a_full_resolve() {
        for seed in 0..40u64 {
            let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);

            let text: String = (0..30)
                .map(|i| format!("line{i} with some trailing words\n"))
                .collect();
            let buffer = TextBuffer::with_text(BufferId::new(0), &text);
            let shared = Arc::new(RwLock::new(buffer));
            let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

            // A handful of folds at pseudo-random rows, kept disjoint so the
            // merge pass does not rewrite them out from under the comparison.
            let snap0 = multi_buffer.snapshot();
            let mut ranges = Vec::new();
            let mut row = 0u32;
            while row < 26 {
                let span = 1 + (lcg(&mut state) % 3) as u32;
                let start = snap0.rope().point_to_offset(stoat_text::Point::new(row, 0));
                let end = snap0
                    .rope()
                    .point_to_offset(stoat_text::Point::new(row + span, 0));
                ranges.push(snap0.anchor_at(start, Bias::Right)..snap0.anchor_at(end, Bias::Left));
                row += span + 1 + (lcg(&mut state) % 2) as u32;
            }

            let (mut inlay_map, inlay_snapshot) = InlayMap::new(snap0.clone());
            let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());
            fold_map.fold(ranges.clone(), FoldPlaceholder::default(), &snap0);
            fold_map.sync(inlay_snapshot, &Patch::empty());

            // Several edits, some landing inside a fold and some between them,
            // each driven through the carrying sync. One to three per sync, so
            // some patches hold several and the carrying is exercised against
            // the post-all-edits coordinates a multi-edit patch states its new
            // ranges in.
            for _ in 0..4 {
                let before = multi_buffer.snapshot();
                for _ in 0..1 + lcg(&mut state) % 3 {
                    let len = multi_buffer.snapshot().rope().len();
                    let at = (lcg(&mut state) as usize) % (len + 1);
                    if lcg(&mut state).is_multiple_of(3) && at + 4 <= len {
                        shared.write().expect("poisoned").edit(at..at + 4, "");
                    } else {
                        shared.write().expect("poisoned").edit(at..at, "zz\n");
                    }
                }

                let after = multi_buffer.snapshot();
                let buffer_edits = after.edits_since(before.version());
                let (inlay_snapshot, inlay_edits) = inlay_map.sync(after, &buffer_edits);
                fold_map.sync(inlay_snapshot, &inlay_edits);
            }

            let carried: Vec<(FoldId, Range<usize>)> = fold_map
                .folds
                .iter()
                .map(|f| (f.id, f.resolved_start..f.resolved_end))
                .collect();

            // The same folds, resolved from their anchors with no carrying.
            let final_snapshot = multi_buffer.snapshot();
            let all: Vec<super::AnchoredFold> = fold_map.folds.iter().cloned().collect();
            let (points, _) = super::resolve_fold_points(&all, &final_snapshot, None);
            let full: Vec<(FoldId, Range<usize>)> = all
                .iter()
                .enumerate()
                .map(|(i, af)| {
                    let start = final_snapshot.rope().point_to_offset(points[i * 2]);
                    let end = final_snapshot.rope().point_to_offset(points[i * 2 + 1]);
                    (af.id, start..end)
                })
                .collect();

            assert_eq!(carried, full, "seed {seed}");
        }
    }

    /// A fold toggle moves no inlay row, so the sync has to work out which
    /// rows it changed from the fold set. Emitting the whole file instead
    /// cascades an O(file) re-wrap and re-block for a few rows of change.
    #[test]
    fn folding_one_range_emits_only_its_rows() {
        let text: String = (0..200)
            .map(|i| format!("line{i}\n"))
            .collect::<Vec<_>>()
            .join("");
        let buffer = TextBuffer::with_text(BufferId::new(0), &text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());
        fold_map.sync(inlay_snapshot.clone(), &Patch::empty());

        let anchor_at = |row: u32, col: u32, bias| {
            let off = buffer_snapshot
                .rope()
                .point_to_offset(stoat_text::Point::new(row, col));
            (off, buffer_snapshot.anchor_at(off, bias))
        };
        let (start_off, start) = anchor_at(100, 0, Bias::Right);
        let (end_off, end) = anchor_at(102, 0, Bias::Left);

        fold_map.fold(
            vec![start..end],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (_, edits) = fold_map.sync(inlay_snapshot.clone(), &Patch::empty());
        let covered: Vec<Range<u32>> = edits.edits().iter().map(|e| e.old.clone()).collect();
        assert_eq!(
            covered,
            vec![100..103],
            "a fold over rows 100..102 must not invalidate all 200 rows",
        );

        #[allow(clippy::single_range_in_vec_init)]
        fold_map.unfold(vec![start_off..end_off], &buffer_snapshot);
        let (_, edits) = fold_map.sync(inlay_snapshot, &Patch::empty());
        let covered: Vec<Range<u32>> = edits.edits().iter().map(|e| e.old.clone()).collect();
        assert_eq!(
            covered.len(),
            1,
            "unfolding names one region too, got {covered:?}",
        );
        assert!(
            covered[0].start >= 100 && covered[0].end <= 103,
            "and it is the unfolded rows, got {covered:?}",
        );
    }

    /// An LSP folding-range install can land in the same sync as a keystroke.
    /// The fold rows cannot be placed in the old transform tree once an edit
    /// has moved them, so that sync falls back to a full rebuild. This pins the
    /// result it has to produce either way.
    #[test]
    fn a_fold_landing_with_a_buffer_edit_renders_both() {
        let text: String = (0..40)
            .map(|i| format!("line{i}\n"))
            .collect::<Vec<_>>()
            .join("");
        let buffer = TextBuffer::with_text(BufferId::new(0), &text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(multi_buffer.snapshot());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());
        fold_map.sync(inlay_snapshot, &Patch::empty());

        // Edit row 5 and fold rows 20..22 before the same sync runs.
        let before = multi_buffer.snapshot();
        let edit_at = before.rope().point_to_offset(stoat_text::Point::new(5, 0));
        shared
            .write()
            .expect("poisoned")
            .edit(edit_at..edit_at, "inserted\n");

        let after = multi_buffer.snapshot();
        let buffer_edits = after.edits_since(before.version());
        let start = after.rope().point_to_offset(stoat_text::Point::new(21, 0));
        let end = after.rope().point_to_offset(stoat_text::Point::new(23, 0));
        fold_map.fold(
            vec![after.anchor_at(start, Bias::Right)..after.anchor_at(end, Bias::Left)],
            FoldPlaceholder::default(),
            &after,
        );

        let (inlay_snapshot, inlay_edits) = inlay_map.sync(after.clone(), &buffer_edits);
        let (snap, _) = fold_map.sync(inlay_snapshot, &inlay_edits);

        assert_eq!(snap.line_count(), 40);
        let rendered: Vec<String> = (0..snap.line_count())
            .map(|row| {
                snap.chunks(
                    snap.row_start_offset(row)..snap.row_start_offset(row + 1),
                    Arc::from(Vec::new()),
                )
                .map(|chunk| chunk.text.to_string())
                .collect::<String>()
                .trim_end_matches('\n')
                .to_string()
            })
            .collect();

        let mut expected: Vec<String> = (0..40).map(|i| format!("line{i}")).collect();
        // The buffer's trailing newline leaves an empty last row.
        expected.push(String::new());
        expected.insert(5, "inserted".to_string());
        // The fold swallows rows 21 and 22 and both their newlines, so row 23's
        // text carries on after the placeholder rather than starting a row.
        let after_fold = expected[23].clone();
        expected.splice(21..24, [format!("...{after_fold}")]);

        assert_eq!(
            rendered, expected,
            "a fold arriving with a buffer edit still renders both",
        );
    }

    /// An LSP folding range can name its own collapsed text, and the fold's
    /// measured width has to agree with the width painted. When they disagree
    /// every column right of the fold on that row lands in the wrong place.
    #[test]
    fn a_collapsed_text_fold_measures_the_width_it_paints() {
        let (width, painted, fold_point, back) = folded_row_geometry(FoldPlaceholder {
            collapsed_text: Some(Arc::from("{ 3 lines }")),
            ..FoldPlaceholder::default()
        });

        assert_eq!(painted.trim_end_matches('\n'), "{ 3 lines }");
        assert_eq!(
            width,
            "{ 3 lines }".len() as u32,
            "the row's width is the collapsed text, not the ellipsis it replaced",
        );
        assert_eq!(
            fold_point,
            FoldPoint::new(1, "{ 3 lines }".len() as u32),
            "the column past the fold sits at the collapsed text's end",
        );
        assert_eq!(back, InlayPoint::new(1, 5), "and converts back to the fold");
    }

    /// The default ellipsis path must be untouched by resolving the two
    /// strings in one place.
    #[test]
    fn an_ellipsis_fold_keeps_its_geometry() {
        let (width, painted, fold_point, back) = folded_row_geometry(FoldPlaceholder::default());

        assert_eq!(painted.trim_end_matches('\n'), "...");
        assert_eq!(width, 3);
        assert_eq!(fold_point, FoldPoint::new(1, 3));
        assert_eq!(back, InlayPoint::new(1, 5));
    }

    #[test]
    fn overlapping_folds_merge() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let to_anchor = |row: u32, col: u32, bias: Bias| {
            let off = buffer_snapshot
                .rope()
                .point_to_offset(stoat_text::Point::new(row, col));
            buffer_snapshot.anchor_at(off, bias)
        };

        fold_map.fold(
            vec![to_anchor(1, 0, Bias::Right)..to_anchor(2, 0, Bias::Left)],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        fold_map.fold(
            vec![to_anchor(1, 5, Bias::Right)..to_anchor(3, 0, Bias::Left)],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (snap, _) = fold_map.sync(inlay_snapshot, &Patch::empty());
        // 4 rows - 2 rows folded = 2 rows
        assert_eq!(snap.line_count(), 2);
    }

    /// Folding over an existing fold must not consume the one already there.
    /// Both are records a caller can still address, and the outer fold's own
    /// anchors have to keep describing the range that was asked for.
    #[test]
    fn a_fold_over_another_leaves_both_records_intact() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let to_anchor = |row: u32, col: u32, bias: Bias| {
            let off = buffer_snapshot
                .rope()
                .point_to_offset(stoat_text::Point::new(row, col));
            buffer_snapshot.anchor_at(off, bias)
        };
        let offset = |row: u32, col: u32| {
            buffer_snapshot
                .rope()
                .point_to_offset(stoat_text::Point::new(row, col))
        };

        // An inner fold inside row 1, then an outer one starting before it and
        // running through row 2.
        let inner = fold_map.fold(
            vec![to_anchor(1, 2, Bias::Right)..to_anchor(1, 5, Bias::Left)],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let outer = fold_map.fold(
            vec![to_anchor(1, 0, Bias::Right)..to_anchor(2, 5, Bias::Left)],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (snap, _) = fold_map.sync(inlay_snapshot.clone(), &Patch::empty());

        for id in inner.iter().chain(&outer) {
            assert!(
                snap.fold_metadata(id).is_some(),
                "both folds stay addressable, {id:?} did not",
            );
        }
        // Rows 1 and 2 collapse together, leaving row 0, the placeholder row,
        // and row 3.
        assert_eq!(snap.line_count(), 3);

        // Unfolding a range touching only the outer fold's tail leaves the
        // inner one in place, which it cannot do if the two share one record.
        #[allow(clippy::single_range_in_vec_init)]
        fold_map.unfold(vec![offset(2, 0)..offset(2, 5)], &buffer_snapshot);
        let (snap, _) = fold_map.sync(inlay_snapshot, &Patch::empty());
        assert!(
            snap.is_line_folded(1),
            "the inner fold survives unfolding the outer",
        );
        assert!(!snap.is_line_folded(2), "and the outer one is gone");
    }

    /// Overlap arises from ordinary use, not just from folds written to overlap.
    /// Edits drag fold anchors toward each other, so two folds made disjoint can
    /// come to share text later. Every sync below runs the region merge and, on
    /// a full rebuild, the input-length check.
    #[test]
    fn folding_unfolding_and_editing_keeps_every_fold_addressable() {
        for seed in 0..40u64 {
            let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);

            let text: String = (0..24).map(|i| format!("line{i} with words\n")).collect();
            let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), &text)));
            let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
            let (mut inlay_map, mut inlay_snapshot) = InlayMap::new(multi_buffer.snapshot());
            let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

            // Each fold's own anchors, held outside the map so liveness is
            // decided without consulting the set under test. Asking the map
            // which folds it still holds would excuse the very loss this checks
            // for.
            let mut live: Vec<(FoldId, Range<Anchor>)> = Vec::new();

            for _ in 0..12 {
                let snapshot = multi_buffer.snapshot();
                let len = snapshot.rope().len();

                match lcg(&mut state) % 3 {
                    0 => {
                        let start = (lcg(&mut state) as usize) % len;
                        let end = start + 1 + (lcg(&mut state) as usize) % (len - start);
                        let range = snapshot.anchor_at(start, Bias::Right)
                            ..snapshot.anchor_at(end, Bias::Left);
                        let ids = fold_map.fold(
                            vec![range.clone()],
                            FoldPlaceholder::default(),
                            &snapshot,
                        );
                        live.extend(ids.into_iter().map(|id| (id, range.clone())));
                    },
                    1 => {
                        let start = (lcg(&mut state) as usize) % len;
                        let end = start + 1 + (lcg(&mut state) as usize) % (len - start);
                        #[allow(clippy::single_range_in_vec_init)]
                        fold_map.unfold(vec![start..end], &snapshot);

                        // Unfolding drops folds by design, and no merge has run
                        // yet, so the stored set is authoritative here alone.
                        live.retain(|(id, _)| fold_map.folds.iter().any(|f| f.id == *id));
                    },
                    _ => {
                        let at = (lcg(&mut state) as usize) % (len + 1);
                        if lcg(&mut state).is_multiple_of(3) && at + 6 <= len {
                            shared.write().expect("poisoned").edit(at..at + 6, "");
                        } else {
                            shared.write().expect("poisoned").edit(at..at, "inserted\n");
                        }
                    },
                }

                let after = multi_buffer.snapshot();
                let buffer_edits = after.edits_since(snapshot.version());
                let inlay_edits;
                (inlay_snapshot, inlay_edits) = inlay_map.sync(after, &buffer_edits);
                let (snap, _) = fold_map.sync(inlay_snapshot.clone(), &inlay_edits);

                // An edit can delete everything a fold covered, which drops it
                // for real. Every fold still spanning text keeps a record of its
                // own, which is what a merge rewriting the stored set destroys.
                let current = multi_buffer.snapshot();
                live.retain(|(_, range)| {
                    current.resolve_anchor(&range.start) < current.resolve_anchor(&range.end)
                });
                for (id, _) in &live {
                    assert!(
                        snap.fold_metadata(id).is_some(),
                        "seed {seed}: {id:?} lost its record",
                    );
                }
            }
        }
    }

    /// Inserting a whole line above everything is the edit that pushes every
    /// row below it down, so it is the one a row patch reporting no shift
    /// strands. The transforms would then describe only the inserted line and
    /// drop the text it displaced.
    #[test]
    fn a_line_inserted_above_leaves_the_text_below_accounted_for() {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "fn a() {}\n",
        )));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(multi_buffer.snapshot());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot);

        for text in ["//1\n", "//2\n"] {
            let before = multi_buffer.snapshot();
            shared.write().expect("poisoned").edit(0..0, text);

            let after = multi_buffer.snapshot();
            let buffer_edits = after.edits_since(before.version());
            let (inlay_snapshot, inlay_edits) = inlay_map.sync(after, &buffer_edits);
            let (snap, _) = fold_map.sync(inlay_snapshot, &inlay_edits);

            assert_eq!(
                snap.transforms.summary().input.len,
                snap.inlay_snapshot.total_summary().len,
                "inserting {text:?} left the transforms describing the wrong amount of text",
            );
        }

        assert_eq!(
            multi_buffer.snapshot().rope().to_string(),
            "//2\n//1\nfn a() {}\n",
        );
    }

    #[test]
    fn is_line_folded_checks() {
        let snap = make_snapshot_with_folds(
            "line0\nline1\nline2\nline3",
            vec![(InlayPoint::new(1, 0), InlayPoint::new(2, 5))],
        );
        assert!(!snap.is_line_folded(0));
        assert!(snap.is_line_folded(1));
        assert!(snap.is_line_folded(2));
        assert!(!snap.is_line_folded(3));
    }

    #[test]
    fn is_line_folded_empty_fold() {
        let snap = make_snapshot_with_folds(
            "hello",
            vec![(InlayPoint::new(0, 3), InlayPoint::new(0, 3))],
        );
        assert!(!snap.is_line_folded(0));
    }

    #[test]
    fn max_point_no_folds() {
        let snap = make_snapshot("hello\nworld");
        assert_eq!(snap.max_point(), FoldPoint::new(1, 5));
    }

    #[test]
    fn max_point_with_folds() {
        let snap = make_snapshot_with_folds(
            "line0\nline1\nline2",
            vec![(InlayPoint::new(1, 0), InlayPoint::new(1, 5))],
        );
        let mp = snap.max_point();
        assert_eq!(mp.row(), 2);
    }

    #[test]
    fn folds_in_range_overlapping() {
        let snap = make_snapshot_with_folds(
            "line0\nline1\nline2\nline3",
            vec![
                (InlayPoint::new(1, 0), InlayPoint::new(1, 5)),
                (InlayPoint::new(2, 0), InlayPoint::new(2, 5)),
            ],
        );
        let folds = snap.folds_in_range(InlayPoint::new(0, 0)..InlayPoint::new(2, 0));
        assert_eq!(folds.len(), 1);
        assert_eq!(folds[0].range.start, InlayPoint::new(1, 0));
    }

    #[test]
    fn folds_in_range_non_overlapping() {
        let snap = make_snapshot_with_folds(
            "line0\nline1\nline2\nline3",
            vec![(InlayPoint::new(2, 0), InlayPoint::new(2, 5))],
        );
        let folds = snap.folds_in_range(InlayPoint::new(0, 0)..InlayPoint::new(1, 0));
        assert!(folds.is_empty());
    }

    #[test]
    fn folds_in_range_empty_range() {
        let snap = make_snapshot_with_folds(
            "line0\nline1",
            vec![(InlayPoint::new(0, 0), InlayPoint::new(0, 5))],
        );
        let folds = snap.folds_in_range(InlayPoint::new(1, 0)..InlayPoint::new(1, 0));
        assert!(folds.is_empty());
    }

    #[test]
    fn fold_map_folds_in_range() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());
        let start_off = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(0, 3));
        let end_off = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(0, 5));
        fold_map.fold(
            vec![
                buffer_snapshot.anchor_at(start_off, Bias::Right)
                    ..buffer_snapshot.anchor_at(end_off, Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (snap, _) = fold_map.sync(inlay_snapshot, &Patch::empty());
        let folds = snap.folds_in_range(InlayPoint::new(0, 0)..InlayPoint::new(1, 0));
        assert_eq!(folds.len(), 1);
    }

    #[test]
    fn fold_line_content() {
        let snap = make_snapshot_with_folds(
            "fn main() {\n    body;\n}",
            vec![(InlayPoint::new(0, 11), InlayPoint::new(2, 0))],
        );
        assert_eq!(snap.fold_line(0), "fn main() {...}");
    }

    #[test]
    fn chars_at_no_folds() {
        let snap = make_snapshot("hello");
        let chars: Vec<char> = snap.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(chars, vec!['h', 'e', 'l', 'l', 'o']);
    }

    #[test]
    fn chars_at_with_fold() {
        let snap = make_snapshot_with_folds(
            "hello world foo",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );
        let s: String = snap.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(s, "hello... foo");
    }

    #[test]
    fn chars_at_multi_fold() {
        let snap = make_snapshot_with_folds(
            "aaa bbb ccc ddd",
            vec![
                (InlayPoint::new(0, 3), InlayPoint::new(0, 7)),
                (InlayPoint::new(0, 11), InlayPoint::new(0, 15)),
            ],
        );
        let s: String = snap.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(s, "aaa... ccc...");
    }

    #[test]
    fn reversed_chars_at_no_folds() {
        let snap = make_snapshot("hello");
        let chars: Vec<char> = snap.reversed_chars_at(FoldPoint::new(0, 5)).collect();
        assert_eq!(chars, vec!['o', 'l', 'l', 'e', 'h']);
    }

    #[test]
    fn reversed_chars_at_with_fold() {
        let snap = make_snapshot_with_folds(
            "hello world foo",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );
        let s: String = snap.reversed_chars_at(snap.max_point()).collect();
        assert_eq!(s, "oof ...olleh");
    }

    #[test]
    fn fold_stops_rendering_after_its_region_is_deleted() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let start_off = snap.rope().point_to_offset(stoat_text::Point::new(1, 0));
        let end_off = snap.rope().point_to_offset(stoat_text::Point::new(1, 5));
        fold_map.fold(
            vec![snap.anchor_at(start_off, Bias::Right)..snap.anchor_at(end_off, Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );

        {
            let mut buf = shared.write().unwrap();
            buf.edit(start_off..end_off, "");
        }

        let snap2 = multi_buffer.snapshot();
        let inlay2 = InlayMap::new(snap2).1;
        let (fold_snap, _) = fold_map.sync(inlay2, &Patch::empty());
        assert_eq!(fold_snap.fold_count(), 0, "nothing is left to render");
        assert_eq!(
            fold_map.folds.iter().count(),
            1,
            "the record stays, so undoing the delete restores the fold",
        );
    }

    #[test]
    fn fold_preserved_after_adjacent_edit() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "aaabbbccc");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        fold_map.fold(
            vec![snap.anchor_at(3, Bias::Right)..snap.anchor_at(6, Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );

        {
            let mut buf = shared.write().unwrap();
            buf.edit(0..0, "XX");
        }

        let snap2 = multi_buffer.snapshot();
        let inlay2 = InlayMap::new(snap2).1;
        let (fold_snap, _) = fold_map.sync(inlay2, &Patch::empty());
        assert_eq!(fold_snap.fold_count(), 1);
    }

    #[test]
    fn fold_stops_rendering_when_its_endpoints_merge() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "abcXYZdef");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        fold_map.fold(
            vec![snap.anchor_at(3, Bias::Right)..snap.anchor_at(6, Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );

        {
            let mut buf = shared.write().unwrap();
            buf.edit(3..6, "");
        }

        let snap2 = multi_buffer.snapshot();
        let inlay2 = InlayMap::new(snap2).1;
        let (fold_snap, _) = fold_map.sync(inlay2, &Patch::empty());
        assert_eq!(fold_snap.fold_count(), 0, "nothing is left to render");
        assert_eq!(
            fold_map.folds.iter().count(),
            1,
            "the record stays, so undoing the delete restores the fold",
        );
    }

    /// Which regions the reader folded is their own state, so an edit that
    /// swallows a fold has to be undoable like any other.
    #[test]
    fn undoing_the_delete_that_collapsed_a_fold_restores_it() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "abcXYZdef");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (mut inlay_map, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot);

        fold_map.fold(
            vec![snap.anchor_at(3, Bias::Right)..snap.anchor_at(6, Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );

        // One inlay map across the whole test, as the display map keeps. A fresh
        // one per sync would hand the fold map an unmoved inlay version and be
        // served the cached snapshot.
        let resync = |inlay_map: &mut InlayMap, fold_map: &mut FoldMap| {
            let (inlay, inlay_edits) = inlay_map.sync(multi_buffer.snapshot(), &Patch::empty());
            fold_map.sync(inlay, &inlay_edits).0.fold_line(0)
        };

        assert_eq!(resync(&mut inlay_map, &mut fold_map), "abc...def");

        shared.write().unwrap().edit(3..6, "");
        assert_eq!(
            resync(&mut inlay_map, &mut fold_map),
            "abcdef",
            "the fold has nothing left to hide",
        );

        assert!(
            shared.write().unwrap().undo().is_some(),
            "the delete undoes"
        );
        assert_eq!(
            resync(&mut inlay_map, &mut fold_map),
            "abc...def",
            "and the fold comes back with the text",
        );
    }

    #[test]
    fn a_fold_operation_that_changes_nothing_leaves_the_version_alone() {
        // The version is what WrapMap::sync reads, and a change it sees with no
        // text edit alongside is its whole-file rebuild condition. Moving it for
        // a fold that folded nothing costs that rebuild for no reason.
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot);

        let at_start = fold_map.version;
        fold_map.fold(Vec::new(), FoldPlaceholder::default(), &snap);
        assert_eq!(
            fold_map.version, at_start,
            "folding no ranges folds nothing"
        );

        #[allow(clippy::single_range_in_vec_init)]
        fold_map.unfold(vec![0..snap.rope().len()], &snap);
        assert_eq!(
            fold_map.version, at_start,
            "and unfolding a buffer that holds no folds removes nothing",
        );

        // A real fold moves it, so the checks above are not passing on a version
        // that simply never moves.
        let start_off = snap.rope().point_to_offset(stoat_text::Point::new(2, 0));
        let end_off = snap.rope().point_to_offset(stoat_text::Point::new(2, 5));
        fold_map.fold(
            vec![snap.anchor_at(start_off, Bias::Right)..snap.anchor_at(end_off, Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );
        let after_fold = fold_map.version;
        assert_ne!(after_fold, at_start, "a fold that lands is a change");

        #[allow(clippy::single_range_in_vec_init)]
        fold_map.unfold(vec![start_off..end_off], &snap);
        assert_ne!(fold_map.version, after_fold, "and so is removing it");
    }

    #[test]
    fn an_inlay_spliced_ahead_of_a_fold_still_moves_it() {
        // A Fold's range is in inlay space, so an inlay added before one shifts
        // it while leaving its buffer offsets exactly where they were. Reusing
        // the cached resolved tree on unchanged offsets alone would keep the
        // fold at the row it used to occupy.
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "line0\nline1\nline2\n",
        )));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let buffer_snapshot = multi.snapshot();
        let at = |row: u32, col: u32| {
            buffer_snapshot
                .rope()
                .point_to_offset(stoat_text::Point::new(row, col))
        };

        let (mut inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot);
        fold_map.fold(
            vec![
                buffer_snapshot.anchor_at(at(2, 0), Bias::Right)
                    ..buffer_snapshot.anchor_at(at(2, 5), Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (inlay_snap, _) = inlay_map.sync(buffer_snapshot.clone(), &Patch::empty());
        let (before, _) = fold_map.sync(inlay_snap, &Patch::empty());
        let placed_before = before.folds.iter().next().expect("the fold").range.clone();

        // A multi-row hint on row 0, which pushes every inlay row below it down
        // without moving a single buffer offset.
        inlay_map.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![(
                buffer_snapshot.anchor_at(at(0, 5), Bias::Right),
                "\nhint\n".to_string(),
                InlayKind::Hint,
            )],
        );
        let (inlay_snap, inlay_edits) = inlay_map.sync(buffer_snapshot, &Patch::empty());
        let (after, _) = fold_map.sync(inlay_snap, &inlay_edits);
        let placed_after = after.folds.iter().next().expect("the fold").range.clone();

        assert_ne!(
            placed_before, placed_after,
            "the hint's rows sit above the fold, so its inlay range moved",
        );
    }

    #[test]
    fn an_edit_past_every_fold_keeps_them_where_they_were() {
        // The case the reuse is for. Typing at the end of a file with folds
        // above moves none of them, so the resolved tree the cache holds is the
        // one a rebuild would produce.
        let text = "line0\nline1\nline2\n";
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), text)));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let snap = multi.snapshot();
        let at = |row: u32, col: u32| {
            snap.rope()
                .point_to_offset(stoat_text::Point::new(row, col))
        };

        let (mut inlay_map, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot);
        fold_map.fold(
            vec![snap.anchor_at(at(0, 0), Bias::Right)..snap.anchor_at(at(0, 5), Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );
        let (inlay_snap, _) = inlay_map.sync(snap, &Patch::empty());
        let (before, _) = fold_map.sync(inlay_snap, &Patch::empty());
        let placed_before: Vec<_> = before.folds.iter().map(|f| f.range.clone()).collect();

        let end = text.len();
        shared.write().expect("poisoned").edit(end..end, "tail\n");
        let edited = multi.snapshot();
        let (inlay_snap, inlay_edits) = inlay_map.sync(edited, &Patch::empty());
        let (after, _) = fold_map.sync(inlay_snap, &inlay_edits);

        assert_eq!(
            after
                .folds
                .iter()
                .map(|f| f.range.clone())
                .collect::<Vec<_>>(),
            placed_before,
            "an edit past every fold leaves all of them where they were",
        );
    }

    #[test]
    fn an_unfold_that_misses_every_fold_leaves_the_version_alone() {
        // Distinct from unfolding an empty set. Folds exist here, the ranges
        // just do not reach them, so the filter retains all of them.
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot);

        let start_off = snap.rope().point_to_offset(stoat_text::Point::new(2, 0));
        let end_off = snap.rope().point_to_offset(stoat_text::Point::new(2, 5));
        fold_map.fold(
            vec![snap.anchor_at(start_off, Bias::Right)..snap.anchor_at(end_off, Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );

        let after_fold = fold_map.version;
        let row0 = snap.rope().point_to_offset(stoat_text::Point::new(0, 0));
        let row0_end = snap.rope().point_to_offset(stoat_text::Point::new(0, 5));
        #[allow(clippy::single_range_in_vec_init)]
        fold_map.unfold(vec![row0..row0_end], &snap);

        assert_eq!(
            fold_map.version, after_fold,
            "row 0 holds no fold, so the one on row 2 is retained and nothing changed",
        );
    }

    #[test]
    fn fold_survives_edit_before() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let start_off = snap.rope().point_to_offset(stoat_text::Point::new(2, 0));
        let end_off = snap.rope().point_to_offset(stoat_text::Point::new(2, 5));
        fold_map.fold(
            vec![snap.anchor_at(start_off, Bias::Right)..snap.anchor_at(end_off, Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );

        {
            let mut buf = shared.write().unwrap();
            buf.edit(0..0, "XX");
        }

        let snap2 = multi_buffer.snapshot();
        let inlay2 = InlayMap::new(snap2).1;
        let (fold_snap, _) = fold_map.sync(inlay2, &Patch::empty());
        assert_eq!(fold_snap.fold_line(2), "...");
    }

    /// The transforms describe the inlay-expanded text, and their summaries
    /// carry byte lengths, row counts, and longest-row bookkeeping that a chunk
    /// boundary landing mid-row could disturb. Multi-byte text is where
    /// counting bytes and counting characters come apart, so the fixture holds
    /// wide characters on both sides of a hint and inside a fold.
    #[test]
    fn transforms_over_folds_and_hints_describe_the_painted_text() {
        let buffer = TextBuffer::with_text(
            BufferId::new(0),
            "let 名前 = 1\nfn 関数() {}\nlet x = 2\n終わり",
        );
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut inlay_map, inlay_snap) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snap);

        let at = |row: u32, column: u32| {
            buffer_snapshot
                .rope()
                .point_to_offset(stoat_text::Point::new(row, column))
        };
        let hint = |offset: usize, text: &str| {
            (
                buffer_snapshot.anchor_at(offset, Bias::Right),
                text.to_string(),
                InlayKind::Hint,
            )
        };

        // Column 10 of row 0 sits just past the wide name, column 5 of row 2
        // just past `let x`.
        inlay_map.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![hint(at(0, 10), ": 数値型"), hint(at(2, 5), ": i64")],
        );
        fold_map.fold(
            vec![
                buffer_snapshot.anchor_at(at(1, 0), Bias::Right)
                    ..buffer_snapshot.anchor_at(at(1, 14), Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );

        let (inlay_snap, _) = inlay_map.sync(buffer_snapshot, &Patch::empty());
        let (snap, _) = fold_map.sync(inlay_snap, &Patch::empty());

        let painted: String = snap
            .chunks(
                FoldOffset(0)..FoldOffset(snap.len().0),
                Arc::from(Vec::new()),
            )
            .map(|chunk| chunk.text.to_string())
            .collect();
        assert_eq!(painted, "let 名前: 数値型 = 1\n...\nlet x: i64 = 2\n終わり");

        // 49 buffer bytes, plus 11 and 5 for the hints, is the 65 the fold layer
        // reads. It paints 14 of them as a 3-byte placeholder.
        let summary = snap.transforms.summary();
        assert_eq!((summary.input.len, summary.output.len), (65, 54));
        assert_eq!(
            (summary.output.lines.row, summary.output.longest_row),
            (3, 0),
            "row 0 carries the wider hint, at 15 characters against row 2's 14",
        );

        let rows: Vec<u32> = (0..snap.line_count())
            .map(|row| snap.line_len(row))
            .collect();
        assert_eq!(rows, vec![25, 3, 14, 9]);
    }

    /// A row's measured width has to be the width the chunk stream paints,
    /// including any inlay text on it. A caller bounding a rendered row by a
    /// short measurement clips the buffer content sitting after the hint.
    #[test]
    fn line_len_counts_inlay_text_on_a_folded_buffer() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "hello world\nsecond line");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut inlay_map, inlay_snap) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snap);

        // A hint on row 0, and a fold on row 1 so `output_line_len` cannot take
        // its no-fold shortcut past the fold layer.
        let hint_at = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(0, 5));
        inlay_map.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![(
                buffer_snapshot.anchor_at(hint_at, Bias::Right),
                ": str".to_string(),
                InlayKind::Hint,
            )],
        );
        let fold_start = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(1, 0));
        let fold_end = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(1, 6));
        fold_map.fold(
            vec![
                buffer_snapshot.anchor_at(fold_start, Bias::Right)
                    ..buffer_snapshot.anchor_at(fold_end, Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );

        let (inlay_snap, _) = inlay_map.sync(buffer_snapshot, &Patch::empty());
        let (snap, _) = fold_map.sync(inlay_snap, &Patch::empty());

        for row in 0..snap.line_count() {
            let painted: usize = snap
                .chunks(
                    snap.row_start_offset(row)..snap.row_start_offset(row + 1),
                    Arc::from(Vec::new()),
                )
                .map(|chunk| chunk.text.trim_end_matches('\n').len())
                .sum();
            assert_eq!(
                snap.line_len(row) as usize,
                painted,
                "row {row} measures what it paints",
            );
        }
    }

    /// Measuring a byte span rather than counting characters has to hold for
    /// multi-byte text too, on both sides of a fold.
    #[test]
    fn line_len_matches_the_painted_width_across_wide_characters() {
        // Row 0 mixes ASCII with three-byte CJK. Row 1 carries a fold, so the
        // measurement goes through the transform tree.
        let snap = make_snapshot_with_folds(
            "ab \u{4f60}\u{597d} cd\nhello world",
            vec![(InlayPoint::new(1, 5), InlayPoint::new(1, 11))],
        );

        for row in 0..snap.line_count() {
            let painted: usize = snap
                .chunks(
                    snap.row_start_offset(row)..snap.row_start_offset(row + 1),
                    Arc::from(Vec::new()),
                )
                .map(|chunk| chunk.text.trim_end_matches('\n').len())
                .sum();
            assert_eq!(
                snap.line_len(row) as usize,
                painted,
                "row {row} measures what it paints",
            );
        }

        // Spelled out, so a change to either side of the comparison above is
        // still measured against something fixed. Row 0 is twelve bytes across
        // eight characters, and row 1 paints "hello..." with " world" folded.
        assert_eq!(snap.line_len(0), 12);
        assert_eq!(snap.line_len(1), 8);
    }

    #[test]
    fn fold_map_invalidates_on_inlay_splice() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut inlay_map, inlay_snap) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snap);

        let off = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(0, 5));
        let anchor = buffer_snapshot.anchor_at(off, Bias::Right);
        inlay_map.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![(anchor, ": str".to_string(), InlayKind::Hint)],
        );
        let (inlay_snap2, _) = inlay_map.sync(buffer_snapshot, &Patch::empty());
        assert!(inlay_snap2.has_inlays());

        let (fold_snap2, _) = fold_map.sync(inlay_snap2, &Patch::empty());
        assert!(fold_snap2.inlay_snapshot().has_inlays());
    }

    #[test]
    fn the_first_fold_row_is_the_earliest_one_folded() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, unfolded) = FoldMap::new(inlay_snapshot.clone());

        assert_eq!(
            unfolded.first_fold_row(),
            None,
            "an unfolded buffer names no row"
        );

        let to_anchor = |row: u32, col: u32, bias: Bias| {
            let off = buffer_snapshot
                .rope()
                .point_to_offset(stoat_text::Point::new(row, col));
            buffer_snapshot.anchor_at(off, bias)
        };

        // Folded later-first, so a snapshot reading the tree in insertion order
        // rather than by position would answer 3.
        fold_map.fold(
            vec![
                to_anchor(3, 0, Bias::Right)..to_anchor(3, 3, Bias::Left),
                to_anchor(1, 0, Bias::Right)..to_anchor(1, 3, Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (snap, _) = fold_map.sync(inlay_snapshot, &Patch::empty());

        assert_eq!(
            snap.first_fold_row(),
            Some(1),
            "the earlier of the two folds names the row"
        );
    }

    #[test]
    fn non_overlapping_folds_no_merge() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let to_anchor = |row: u32, col: u32, bias: Bias| {
            let off = buffer_snapshot
                .rope()
                .point_to_offset(stoat_text::Point::new(row, col));
            buffer_snapshot.anchor_at(off, bias)
        };

        fold_map.fold(
            vec![
                to_anchor(0, 2, Bias::Right)..to_anchor(0, 4, Bias::Left),
                to_anchor(2, 0, Bias::Right)..to_anchor(2, 3, Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let (snap, _) = fold_map.sync(inlay_snapshot, &Patch::empty());
        assert_eq!(snap.fold_count(), 2);
    }

    #[test]
    fn chunks_no_folds_passthrough() {
        let snap = make_snapshot("hello\nworld");
        let end = snap.len();
        let text: String = snap
            .chunks(FoldOffset(0)..end, Arc::from(Vec::new()))
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn chunks_with_fold_emits_placeholder() {
        // "hello world foo" with fold at columns 5..11 -> "hello... foo"
        let snap = make_snapshot_with_folds(
            "hello world foo",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(0, 11))],
        );
        let end = snap.len();
        let chunks: Vec<_> = snap
            .chunks(FoldOffset(0)..end, Arc::from(Vec::new()))
            .collect();
        let text: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(text, "hello... foo");

        // Exactly one chunk must carry a fold renderer (the placeholder).
        let fold_chunks: Vec<_> = chunks.iter().filter(|c| c.renderer.is_some()).collect();
        assert_eq!(fold_chunks.len(), 1);
        assert_eq!(fold_chunks[0].text.as_ref(), "...");
    }
}
