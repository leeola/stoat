use super::{
    highlights::{Chunk, HighlightEndpoint},
    inlay_map::{InlayChunks, InlayOffset, InlayPoint, InlaySnapshot},
};
use crate::multi_buffer::MultiBufferSnapshot;
use std::{
    any::TypeId,
    borrow::Cow,
    cmp::Ordering,
    ops::{Add, AddAssign, Deref, Range, Sub},
    sync::Arc,
};
use stoat_text::{
    patch::Patch, tree_map::TreeMap, Anchor, AnchorRangeExt, Bias, CharsAt, ContextLessSummary,
    Cursor, Dimension, Dimensions, Item, Point, ReversedCharsAt, Rope, SeekTarget, SumTree,
    Summary, TextSummary,
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
    #[allow(dead_code)]
    fn display_text(&self) -> &str {
        self.collapsed_text.as_deref().unwrap_or(self.text.as_ref())
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
    placeholder: Option<FoldPlaceholder>,
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

/// Accumulates the fold-space (`output`) text summary across transforms, so a
/// cursor can sum the interior of a fold-point range in O(log n) for
/// [`FoldSnapshot::text_summary_for_range`].
#[derive(Clone, Default)]
struct FoldOutputSummary(TextSummary);

impl<'a> Dimension<'a, TransformSummary> for FoldOutputSummary {
    fn zero(_cx: ()) -> Self {
        Self(TextSummary::default())
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        ContextLessSummary::add_summary(&mut self.0, &s.output);
    }
}

#[derive(Clone, Debug)]
struct AnchoredFold {
    id: FoldId,
    range: Range<Anchor>,
    placeholder: FoldPlaceholder,
}

/// Summary of a span of [`AnchoredFold`]s, keyed by [`Anchor`] and resolved
/// lazily against the buffer passed as context. `min_start`/`max_end` bound the
/// span's offset extent so [`FoldMap::is_folded_at_offset`] can prune;
/// `start`/`end` carry the last fold's range for the [`FoldRange`] dimension.
/// Because comparison is deferred to query time, buffer edits never rebuild the
/// tree -- only `fold`/`unfold` do.
#[derive(Clone, Debug)]
struct AnchoredFoldSummary {
    start: Anchor,
    end: Anchor,
    min_start: Anchor,
    max_end: Anchor,
    count: usize,
}

impl Default for AnchoredFoldSummary {
    fn default() -> Self {
        Self {
            start: Anchor::min(),
            end: Anchor::max(),
            min_start: Anchor::max(),
            max_end: Anchor::min(),
            count: 0,
        }
    }
}

impl Summary for AnchoredFoldSummary {
    type Context<'a> = &'a MultiBufferSnapshot;

    fn zero(_cx: Self::Context<'_>) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, other: &Self, cx: Self::Context<'_>) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 {
            self.min_start = other.min_start;
            self.max_end = other.max_end;
        } else {
            let resolve = |a: &Anchor| cx.resolve_anchor(a);
            if other.min_start.cmp(&self.min_start, &resolve) == Ordering::Less {
                self.min_start = other.min_start;
            }
            if other.max_end.cmp(&self.max_end, &resolve) == Ordering::Greater {
                self.max_end = other.max_end;
            }
        }
        self.start = other.start;
        self.end = other.end;
        self.count += other.count;
    }
}

impl Item for AnchoredFold {
    type Summary = AnchoredFoldSummary;
    fn summary(&self, _cx: &MultiBufferSnapshot) -> AnchoredFoldSummary {
        AnchoredFoldSummary {
            start: self.range.start,
            end: self.range.end,
            min_start: self.range.start,
            max_end: self.range.end,
            count: 1,
        }
    }
}

/// Anchor-range dimension and seek target over [`AnchoredFoldSummary`]. Seeks
/// resolve anchors against the buffer carried as the cursor's context, so the
/// storage tree stays correctly ordered across edits without being rebuilt.
#[derive(Clone, Debug)]
struct FoldRange(Range<Anchor>);

impl Default for FoldRange {
    fn default() -> Self {
        Self(Anchor::min()..Anchor::max())
    }
}

impl<'a> Dimension<'a, AnchoredFoldSummary> for FoldRange {
    fn zero(_cx: &MultiBufferSnapshot) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, summary: &'a AnchoredFoldSummary, _cx: &MultiBufferSnapshot) {
        self.0.start = summary.start;
        self.0.end = summary.end;
    }
}

impl SeekTarget<'_, AnchoredFoldSummary, FoldRange> for FoldRange {
    fn cmp(&self, other: &Self, cx: &MultiBufferSnapshot) -> Ordering {
        let resolve = |a: &Anchor| cx.resolve_anchor(a);
        AnchorRangeExt::cmp(&self.0, &other.0, &resolve)
    }
}

pub struct FoldMap {
    folds: SumTree<AnchoredFold>,
    next_id: usize,
    version: usize,
    cached_snapshot: Option<Arc<FoldSnapshot>>,
    last_inlay_version: usize,
    last_self_version: usize,
    /// Inlay-row regions a pending `fold`/`unfold` touched, resolved against
    /// [`Self::cached_snapshot`] (the "old" space the next sync's inlay edits
    /// map forward from). The next [`Self::sync`] composes them with the
    /// buffer's inlay edits so a fold toggle rebuilds only its rows instead of
    /// taking the whole-file `0..line_count` path. A mutation that fills this
    /// always bumps `version`, so the early-return and the coordinator cache
    /// both re-sync and drain it.
    deferred_edits: Patch<u32>,
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
        let snapshot = Arc::new(FoldSnapshot {
            inlay_snapshot,
            transforms,
            folds: SumTree::new(()),
            fold_metadata_by_id: TreeMap::default(),
            version: 0,
        });
        let map = FoldMap {
            folds: SumTree::new(snapshot.inlay_snapshot.buffer_snapshot()),
            next_id: 0,
            version: 0,
            cached_snapshot: Some(Arc::clone(&snapshot)),
            last_inlay_version: inlay_version,
            last_self_version: 0,
            deferred_edits: Patch::empty(),
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
        {
            if let Some(ref cached) = self.cached_snapshot {
                return (Arc::clone(cached), Patch::empty());
            }
        }

        // Resolve the anchor storage tree to inlay points for this snapshot's
        // transforms. The tree itself is left untouched -- buffer edits never
        // rebuild it; only `fold`/`unfold` mutate it.
        let buffer = inlay_snapshot.buffer_snapshot();
        let all_anchors: Vec<Anchor> = self
            .folds
            .iter()
            .flat_map(|af| [af.range.start, af.range.end])
            .collect();
        let all_points = buffer.points_for_anchors_batch(&all_anchors);
        let mut resolved = Vec::new();
        for (i, af) in self.folds.iter().enumerate() {
            let start_inlay = inlay_snapshot.to_inlay_point(all_points[i * 2], Bias::Right);
            let end_inlay = inlay_snapshot.to_inlay_point(all_points[i * 2 + 1], Bias::Right);
            if start_inlay >= end_inlay {
                continue;
            }
            resolved.push(Fold {
                id: af.id,
                range: start_inlay..end_inlay,
                placeholder: af.placeholder.clone(),
            });
        }

        // Fold mutations recorded their touched rows as identity edits in
        // `deferred_edits`; compose them with the buffer's inlay edits so the
        // incremental rebuild covers both the toggled folds and any concurrent
        // edit. A pure fold toggle arrives here with empty `inlay_edits`, so the
        // composed set is the deferred regions alone.
        let composed_edits = if self.deferred_edits.is_empty() {
            inlay_edits.clone()
        } else {
            let deferred = std::mem::replace(&mut self.deferred_edits, Patch::empty());
            deferred.compose(inlay_edits.edits().iter().cloned())
        };

        let can_incremental = !composed_edits.is_empty() && self.cached_snapshot.is_some();

        resolved.sort_by_key(|f| f.range.start);
        // Folds are stored individually; coalesce overlapping (and
        // adjacent-mergeable) folds only here, so the transforms render an
        // outer fold as a single placeholder without discarding the inner
        // fold from storage. The merged region keeps the first fold's id
        // and placeholder.
        let mut merged_resolved: Vec<Fold> = Vec::with_capacity(resolved.len());
        for fold in resolved {
            if let Some(last) = merged_resolved.last_mut() {
                let overlaps = fold.range.start < last.range.end;
                let adjacent = fold.range.start == last.range.end
                    && last.placeholder.merge_adjacent
                    && fold.placeholder.merge_adjacent;
                if overlaps || adjacent {
                    if fold.range.end > last.range.end {
                        last.range.end = fold.range.end;
                    }
                    continue;
                }
            }
            merged_resolved.push(fold);
        }
        let resolved_tree = SumTree::from_iter(merged_resolved, ());

        let (transforms, edits) = if can_incremental {
            let old_snapshot = self
                .cached_snapshot
                .as_ref()
                .expect("guarded by can_incremental");
            sync_fold_incremental(
                old_snapshot,
                &inlay_snapshot,
                &composed_edits,
                &resolved_tree,
            )
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

        let mut fold_metadata_by_id = TreeMap::default();
        for fold in self.folds.iter() {
            fold_metadata_by_id.insert(
                fold.id,
                FoldMetadata {
                    range: fold.range.clone(),
                    display_width: None,
                },
            );
        }
        let snapshot = Arc::new(FoldSnapshot {
            inlay_snapshot,
            transforms,
            folds: resolved_tree,
            fold_metadata_by_id,
            version: self.version,
        });

        #[cfg(test)]
        snapshot.check_invariants();

        self.last_inlay_version = snapshot.inlay_snapshot.inlay_version;
        self.last_self_version = self.version;
        self.cached_snapshot = Some(Arc::clone(&snapshot));
        (snapshot, edits)
    }

    pub fn fold(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        placeholder: FoldPlaceholder,
        buffer_snapshot: &MultiBufferSnapshot,
    ) -> Vec<FoldId> {
        let mut new_folds: Vec<AnchoredFold> = ranges
            .into_iter()
            .map(|range| {
                let id = FoldId(self.next_id);
                self.next_id += 1;
                AnchoredFold {
                    id,
                    range,
                    placeholder: placeholder.clone(),
                }
            })
            .collect();
        let new_ids: Vec<FoldId> = new_folds.iter().map(|f| f.id).collect();

        if let Some(snapshot) = self.cached_snapshot.clone() {
            let regions: Vec<(u32, u32)> = new_folds
                .iter()
                .map(|fold| fold_inlay_region(&fold.range, &snapshot.inlay_snapshot))
                .collect();
            // A new fold can coalesce with an existing fold it overlaps or
            // abuts, changing that fold's rendered span. Defer those rows too so
            // the incremental rebuild replaces the neighbor's now-stale
            // placeholder rather than leaving it beside the coalesced one with
            // its collapsed input counted twice.
            let neighbors = neighbor_fold_regions(&self.folds, &regions, &snapshot.inlay_snapshot);
            self.merge_deferred_regions(regions.into_iter().chain(neighbors));
        }

        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        new_folds.sort_unstable_by(|a, b| AnchorRangeExt::cmp(&a.range, &b.range, &resolve));

        self.folds = {
            let mut new_tree = SumTree::new(buffer_snapshot);
            let mut cursor = self.folds.cursor::<FoldRange>(buffer_snapshot);
            for fold in new_folds {
                new_tree.append(
                    cursor.slice(&FoldRange(fold.range.clone()), Bias::Right),
                    buffer_snapshot,
                );
                new_tree.push(fold, buffer_snapshot);
            }
            new_tree.append(cursor.suffix(), buffer_snapshot);
            new_tree
        };

        self.version += 1;
        new_ids
    }

    pub fn unfold(&mut self, ranges: Vec<Range<usize>>, buffer_snapshot: &MultiBufferSnapshot) {
        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        let mut removed: Vec<AnchoredFold> = Vec::new();
        let mut new_folds = SumTree::new(buffer_snapshot);
        for fold in self.folds.iter() {
            if ranges
                .iter()
                .any(|r| fold.range.overlaps_range(r, &resolve))
            {
                removed.push(fold.clone());
            } else {
                new_folds.push(fold.clone(), buffer_snapshot);
            }
        }

        if removed.is_empty() {
            return;
        }

        self.folds = new_folds;

        if let Some(snapshot) = self.cached_snapshot.clone() {
            let regions: Vec<(u32, u32)> = removed
                .iter()
                .map(|fold| fold_inlay_region(&fold.range, &snapshot.inlay_snapshot))
                .collect();
            // A removed fold may have been coalesced with a surviving fold; that
            // survivor now renders on its own, so defer its rows too and let the
            // rebuild re-emit it as a separate placeholder.
            let neighbors = neighbor_fold_regions(&self.folds, &regions, &snapshot.inlay_snapshot);
            self.merge_deferred_regions(regions.into_iter().chain(neighbors));
        }

        self.version += 1;
    }

    /// Coalesce inlay-row `regions` into [`Self::deferred_edits`] as identity
    /// (`old == new`) edits, kept sorted and overlap-merged. The next
    /// [`Self::sync`] composes them with the buffer's inlay edits, so the
    /// affected rows are reconstructed incrementally rather than via a full
    /// rebuild.
    fn merge_deferred_regions(&mut self, regions: impl IntoIterator<Item = (u32, u32)>) {
        let mut ranges: Vec<(u32, u32)> = self
            .deferred_edits
            .edits()
            .iter()
            .map(|e| (e.old.start, e.old.end))
            .chain(regions)
            .filter(|&(start, end)| end > start)
            .collect();
        ranges.sort_unstable();

        let mut merged: Vec<stoat_text::patch::Edit<u32>> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut() {
                if start <= last.old.end {
                    last.old.end = last.old.end.max(end);
                    last.new.end = last.old.end;
                    continue;
                }
            }
            merged.push(stoat_text::patch::Edit {
                old: start..end,
                new: start..end,
            });
        }
        self.deferred_edits = Patch::new(merged);
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
        let mut cursor = self.folds.filter::<_, ()>(buffer_snapshot, |summary| {
            buffer_snapshot.resolve_anchor(&summary.max_end) > offset
        });
        cursor.next();
        while let Some(fold) = cursor.item() {
            if buffer_snapshot.resolve_anchor(&fold.range.start) > offset {
                return false;
            }
            if fold.range.contains_offset(offset, &resolve) {
                return true;
            }
            cursor.next();
        }
        false
    }

    /// The anchor range of every active fold, in storage order.
    /// Resolve against a buffer snapshot to recover offsets or points;
    /// the display map uses this to enumerate folds for persistence.
    pub fn fold_anchor_ranges(&self) -> Vec<Range<Anchor>> {
        self.folds.iter().map(|fold| fold.range.clone()).collect()
    }

    pub fn version_unchanged(&self) -> bool {
        self.version == self.last_self_version
    }
}

/// Inlay-row span `[start_row, end_row + 1)` a fold occupies, resolving its
/// anchors against `inlay_snapshot`'s buffer. Used to turn a `fold`/`unfold`
/// into a `deferred_edits` region; a fold toggle leaves the inlay text
/// unchanged, so the span is identical on the old and new sides.
fn fold_inlay_region(range: &Range<Anchor>, inlay_snapshot: &InlaySnapshot) -> (u32, u32) {
    let buffer = inlay_snapshot.buffer_snapshot();
    let start_point = buffer
        .rope()
        .offset_to_point(buffer.resolve_anchor(&range.start));
    let end_point = buffer
        .rope()
        .offset_to_point(buffer.resolve_anchor(&range.end));
    let start_row = inlay_snapshot
        .to_inlay_point(start_point, Bias::Right)
        .row();
    let end_row = inlay_snapshot.to_inlay_point(end_point, Bias::Right).row();
    (start_row, end_row + 1)
}

/// Inlay-row regions of `folds` whose rows overlap any of `regions`.
///
/// Used to extend a fold mutation's deferred rebuild regions to its coalescing
/// neighbors: a fold sharing or abutting a mutated fold's rows renders
/// differently once they merge or split, so those rows must be rebuilt too.
/// Because [`fold_inlay_region`] ends a fold one row past its last, abutting
/// folds already have overlapping regions, so this catches adjacency as well as
/// overlap.
fn neighbor_fold_regions(
    folds: &SumTree<AnchoredFold>,
    regions: &[(u32, u32)],
    inlay_snapshot: &InlaySnapshot,
) -> Vec<(u32, u32)> {
    folds
        .iter()
        .map(|fold| fold_inlay_region(&fold.range, inlay_snapshot))
        .filter(|region| {
            regions
                .iter()
                .any(|&(start, end)| region.0 < end && start < region.1)
        })
        .collect()
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
                    placeholder: None,
                    fold_id: None,
                },
                (),
            );
        }
        return transforms;
    }

    let total_len = inlay_snapshot.total_summary().len;
    let mut cursor = 0usize;

    for fold in folds.iter() {
        let fold_start = inlay_snapshot
            .inlay_point_to_offset(fold.range.start)
            .0
            .min(total_len);
        let fold_end = inlay_snapshot
            .inlay_point_to_offset(fold.range.end)
            .0
            .min(total_len);

        if fold_start > cursor {
            let summary =
                inlay_snapshot.text_summary_for_range(InlayOffset(cursor)..InlayOffset(fold_start));
            transforms.push(
                Transform {
                    summary: TransformSummary {
                        input: summary.clone(),
                        output: summary,
                    },
                    placeholder: None,
                    fold_id: None,
                },
                (),
            );
        }

        let input_summary =
            inlay_snapshot.text_summary_for_range(InlayOffset(fold_start)..InlayOffset(fold_end));
        let output_summary = TextSummary::from_str(&fold.placeholder.text);
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input: input_summary,
                    output: output_summary,
                },
                placeholder: Some(fold.placeholder.clone()),
                fold_id: Some(fold.id),
            },
            (),
        );

        cursor = fold_end;
    }

    if cursor < total_len {
        let summary =
            inlay_snapshot.text_summary_for_range(InlayOffset(cursor)..InlayOffset(total_len));
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input: summary.clone(),
                    output: summary,
                },
                placeholder: None,
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
            if t.placeholder.is_none() {
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
                placeholder: None,
                fold_id: None,
            },
            (),
        );
    }
}

fn sync_fold_incremental(
    old_snapshot: &FoldSnapshot,
    inlay_snapshot: &InlaySnapshot,
    inlay_edits: &Patch<u32>,
    resolved_folds: &SumTree<Fold>,
) -> (SumTree<Transform>, Patch<u32>) {
    let total_len = inlay_snapshot.total_summary().len;

    let row_to_offset = |row: u32| -> usize { inlay_snapshot.inlay_offset_at_row(row).0 };

    let text_summary = |a: usize, b: usize| -> TextSummary {
        inlay_snapshot.text_summary_for_range(InlayOffset(a)..InlayOffset(b))
    };

    // The cursor walks the old transforms, whose input space is the old
    // inlay snapshot's output offsets, so `edit.old.*` rows must resolve
    // against the old snapshot; `edit.new.*` use the new `row_to_offset`.
    let old_row_to_offset =
        |row: u32| -> usize { old_snapshot.inlay_snapshot.inlay_offset_at_row(row).0 };
    let old_text_len = old_snapshot.transforms.summary().input.len;

    let mut new_transforms = SumTree::new(());
    let mut cursor = old_snapshot.transforms.cursor::<InputOffset>(());
    let mut row_edits = Patch::empty();

    let mut edits_iter = inlay_edits.into_iter().peekable();
    while let Some(edit) = edits_iter.next() {
        let mut old_start_offset = old_row_to_offset(edit.old.start);
        let old_end_offset = old_row_to_offset(edit.old.end).min(old_text_len);

        // Preserve unchanged prefix
        new_transforms.append(cursor.slice(&InputOffset(old_start_offset), Bias::Left), ());

        // If the edit begins inside a placeholder, snap its start back to that
        // placeholder's boundary so the rebuild replaces the whole fold instead
        // of emitting the fold's leading input as an isomorphic gap (which would
        // also be re-counted under the rebuilt placeholder). The skipped bytes
        // are unchanged prefix, so the new start moves back by the same amount.
        let mut start_delta = 0;
        if let Some(item) = cursor.item() {
            if item.placeholder.is_some() && cursor.start().0 < old_start_offset {
                start_delta = old_start_offset - cursor.start().0;
                old_start_offset = cursor.start().0;
            }
        }

        // If cursor item ends exactly at edit start, merge it with prefix
        if let Some(item) = cursor.item() {
            if item.placeholder.is_none()
                && cursor.start().0 + item.summary.input.len == old_start_offset
            {
                push_fold_isomorphic(&mut new_transforms, item.summary.clone());
                cursor.next();
            }
        }

        // Record old output rows
        let old_fold_start = old_snapshot
            .to_fold_point(InlayPoint::new(edit.old.start, 0), Bias::Right)
            .row();
        let old_fold_end = if edit.old.start == edit.old.end {
            old_fold_start + 1
        } else {
            old_snapshot
                .to_fold_point(InlayPoint::new(edit.old.end, 0), Bias::Right)
                .row()
                .max(old_fold_start + 1)
        };

        // Seek past old content
        cursor.seek_forward(&InputOffset(old_end_offset), Bias::Right);

        // Push gap from current position to edit.new.start
        let new_start_offset = row_to_offset(edit.new.start) - start_delta;
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

        // Rebuild transforms for the edit region [new_start, new_end)
        let new_end_offset = row_to_offset(edit.new.end).min(total_len);
        let folds_in_range: Vec<&Fold> = {
            let new_start_inlay = InlayPoint::new(edit.new.start, 0);
            let new_end_inlay = InlayPoint::new(edit.new.end, 0);
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

        if folds_in_range.is_empty() {
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
            for fold in &folds_in_range {
                let fold_start_offset = inlay_snapshot
                    .inlay_point_to_offset(fold.range.start)
                    .0
                    .min(total_len);
                let fold_end_offset = inlay_snapshot
                    .inlay_point_to_offset(fold.range.end)
                    .0
                    .min(total_len);

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
                let output_summary = TextSummary::from_str(&fold.placeholder.text);
                new_transforms.push(
                    Transform {
                        summary: TransformSummary {
                            input: input_summary,
                            output: output_summary,
                        },
                        placeholder: Some(fold.placeholder.clone()),
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
        let new_fold_end = if new_out.column > 0 {
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
            if edits_iter
                .peek()
                .is_none_or(|next| old_row_to_offset(next.old.start) >= cursor_end)
            {
                // A placeholder beginning at or after the edit end lies wholly
                // outside the edit. Re-emitting its tail as isomorphic text would
                // drop the fold and count its folded input as output, so leave it
                // for `cursor.suffix()` to carry over intact. Only an isomorphic
                // tail (or a placeholder the edit reaches into) is re-emitted here.
                let preserved_placeholder =
                    item.placeholder.is_some() && cursor.start().0 >= old_end_offset;
                if !preserved_placeholder {
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
    }

    new_transforms.append(cursor.suffix(), ());

    if new_transforms.is_empty() && total_len > 0 {
        let summary = inlay_snapshot.total_summary();
        new_transforms.push(
            Transform {
                summary: TransformSummary {
                    input: summary.clone(),
                    output: summary,
                },
                placeholder: None,
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

    /// Asserts the fold transform tree's total input length equals the
    /// inlay snapshot's output length. Called from `sync` under
    /// `cfg(test)` so every incremental sync is checked, catching
    /// coordinate-space and patch-span corruption in the transform
    /// rebuild.
    #[cfg(test)]
    fn check_invariants(&self) {
        assert_eq!(
            self.transforms.summary().input.len,
            self.inlay_snapshot.total_summary().len,
            "fold transform input length must equal inlay output length",
        );
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

    pub fn to_fold_point(&self, inlay_point: InlayPoint, bias: Bias) -> FoldPoint {
        let (start, end, item) = self
            .transforms
            .find::<Dimensions<InlayPoint, FoldPoint>, _>((), &inlay_point, Bias::Right);
        match item {
            Some(t) if t.placeholder.is_some() => {
                if inlay_point.0 == start.0 .0 || bias == Bias::Left {
                    start.1
                } else {
                    end.1
                }
            },
            Some(_) | None => {
                let overshoot = point_overshoot(start.0 .0, inlay_point.0);
                FoldPoint(start.1 .0 + overshoot)
            },
        }
    }

    pub fn to_inlay_point(&self, fold_point: FoldPoint) -> InlayPoint {
        let (start, _end, item) = self
            .transforms
            .find::<Dimensions<InlayPoint, FoldPoint>, _>((), &fold_point, Bias::Right);
        match item {
            Some(t) if t.placeholder.is_some() => start.0,
            Some(_) | None => {
                let overshoot = point_overshoot(start.1 .0, fold_point.0);
                InlayPoint(start.0 .0 + overshoot)
            },
        }
    }

    pub fn clip_point(&self, point: FoldPoint, bias: Bias) -> FoldPoint {
        let (start, end, item) = self
            .transforms
            .find::<Dimensions<FoldPoint, InlayPoint>, _>((), &point, Bias::Right);
        match item {
            Some(transform) if transform.placeholder.is_some() => {
                if point.0 == start.0 .0 || bias == Bias::Left {
                    start.0
                } else {
                    end.0
                }
            },
            Some(_) => {
                let overshoot = point_overshoot(start.0 .0, point.0);
                let inlay_point = InlayPoint(start.1 .0 + overshoot);
                let clipped = self.inlay_snapshot.clip_point(inlay_point, bias);
                let back = point_overshoot(start.1 .0, clipped.0);
                FoldPoint(start.0 .0 + back)
            },
            None => FoldPoint(self.transforms.summary().output.lines),
        }
    }

    pub fn fold_count(&self) -> usize {
        self.folds.summary().count
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
        let target = FoldPoint::new(fold_row, 0);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<FoldPoint, FoldOffset, InlayPoint>>(());
        cursor.seek(&target, Bias::Left);
        let Dimensions(transform_start_point, transform_start_offset, transform_start_inlay) =
            *cursor.start();

        let overshoot_rows = fold_row - transform_start_point.row();
        if overshoot_rows == 0 {
            return transform_start_offset;
        }

        // Only isomorphic transforms span multiple output rows -- placeholders
        // collapse to a single row -- and within one, output bytes mirror input
        // bytes. The target row's offset is thus the output base plus the input
        // span from the transform start, resolved via two O(log n) inlay lookups.
        match cursor.item() {
            Some(transform) if transform.placeholder.is_none() => {
                let target_inlay = InlayPoint::new(transform_start_inlay.row() + overshoot_rows, 0);
                let target_offset = self.inlay_snapshot.inlay_point_to_offset(target_inlay);
                let start_offset = self
                    .inlay_snapshot
                    .inlay_point_to_offset(transform_start_inlay);
                FoldOffset(transform_start_offset.0 + (target_offset.0 - start_offset.0))
            },
            _ => transform_start_offset,
        }
    }

    /// Fold-offset of the end of `fold_row`'s rendered content, excluding
    /// the trailing newline. Unlike [`FoldSnapshot::line_len`], which sums
    /// only buffer chars and fold placeholders, this derives from the
    /// transform tree (via [`FoldSnapshot::row_start_offset`]) and so counts
    /// inlay bytes -- the inclusive end that pairs with the inclusive start
    /// when slicing a row's chunk range.
    pub fn row_content_end_offset(&self, fold_row: u32) -> FoldOffset {
        if fold_row + 1 >= self.line_count() {
            self.len()
        } else {
            FoldOffset(self.row_start_offset(fold_row + 1).0 - 1)
        }
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
            // A fold ending at column 0 of a row collapses only up to that
            // row's start, leaving the row itself rendered, so the bound is
            // strict: `> row_start` excludes a column-0 end row that an
            // end-row `>=` check would wrongly mark folded.
            if fold.range.end > row_start
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

    pub fn line_len(&self, fold_row: u32) -> u32 {
        self.fold_line_chars(fold_row)
            .map(|ch| ch.len_utf8() as u32)
            .sum()
    }

    /// Text summary of a fold-point range without materializing the text.
    ///
    /// Sums the fold-space (`output`) summary across the range: a partial
    /// summary at each boundary transform -- placeholder text for a fold, or
    /// an [`InlaySnapshot::text_summary_for_range`] for an isomorphic span --
    /// and an O(log n) interior via `cursor.summary`. `longest_row` /
    /// `longest_row_chars` are char counts (a tab counts as one char), not
    /// tab-expanded display columns.
    pub fn text_summary_for_range(&self, range: Range<FoldPoint>) -> TextSummary {
        let mut summary = TextSummary::default();

        let mut cursor = self
            .transforms
            .cursor::<Dimensions<FoldPoint, InlayPoint>>(());
        cursor.seek(&range.start, Bias::Right);
        if let Some(transform) = cursor.item() {
            let start_in_transform = range.start.0 - cursor.start().0 .0;
            let end_in_transform = range.end.min(cursor.end().0).0 - cursor.start().0 .0;
            if let Some(placeholder) = transform.placeholder.as_ref() {
                summary = TextSummary::from_str(
                    &placeholder.text
                        [start_in_transform.column as usize..end_in_transform.column as usize],
                );
            } else {
                let inlay_start = self
                    .inlay_snapshot
                    .inlay_point_to_offset(InlayPoint(cursor.start().1 .0 + start_in_transform));
                let inlay_end = self
                    .inlay_snapshot
                    .inlay_point_to_offset(InlayPoint(cursor.start().1 .0 + end_in_transform));
                summary = self
                    .inlay_snapshot
                    .text_summary_for_range(inlay_start..inlay_end);
            }
        }

        if range.end > cursor.end().0 {
            cursor.next();
            let interior: FoldOutputSummary = cursor.summary(&range.end, Bias::Right);
            ContextLessSummary::add_summary(&mut summary, &interior.0);

            if let Some(transform) = cursor.item() {
                let end_in_transform = range.end.0 - cursor.start().0 .0;
                if let Some(placeholder) = transform.placeholder.as_ref() {
                    ContextLessSummary::add_summary(
                        &mut summary,
                        &TextSummary::from_str(
                            &placeholder.text[..end_in_transform.column as usize],
                        ),
                    );
                } else {
                    let inlay_start = self.inlay_snapshot.inlay_point_to_offset(cursor.start().1);
                    let inlay_end = self
                        .inlay_snapshot
                        .inlay_point_to_offset(InlayPoint(cursor.start().1 .0 + end_in_transform));
                    ContextLessSummary::add_summary(
                        &mut summary,
                        &self
                            .inlay_snapshot
                            .text_summary_for_range(inlay_start..inlay_end),
                    );
                }
            }
        }

        summary
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

        // Seek straight to the first fold at or after the position (O(log)) and
        // advance the cursor lazily during iteration, rather than walking and
        // cloning every following fold up front.
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

        // Seek straight to the fold immediately preceding the position (O(log))
        // and walk the cursor backward lazily, rather than collecting every
        // earlier fold up front.
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

            if let Some(placeholder) = transform.placeholder.as_ref() {
                // Emit placeholder text as a single chunk. Placeholders span the
                // entire transform in fold-offset space regardless of how many
                // inlay-side bytes they collapse.
                let text: &'a str = placeholder
                    .collapsed_text
                    .as_deref()
                    .unwrap_or(placeholder.text.as_ref());
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
    pub fn map(&mut self, inlay_point: InlayPoint, bias: Bias) -> FoldPoint {
        if self.cursor.did_seek() {
            self.cursor.seek_forward(&inlay_point, Bias::Right);
        } else {
            self.cursor.seek(&inlay_point, Bias::Right);
        }
        let start = self.cursor.start();
        match self.cursor.item() {
            Some(t) if t.placeholder.is_some() => {
                if inlay_point.0 == start.0 .0 || bias == Bias::Left {
                    start.1
                } else {
                    self.cursor.end().1
                }
            },
            Some(_) | None => {
                let overshoot = point_overshoot(start.0 .0, inlay_point.0);
                FoldPoint(start.1 .0 + overshoot)
            },
        }
    }
}

pub struct FoldChars<'a> {
    inlay_snapshot: &'a InlaySnapshot,
    rope: &'a Rope,
    chars: CharsAt<'a>,
    buffer_offset: usize,
    folds: Cursor<'a, 'static, Fold, FoldStart>,
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
            let fold = self
                .folds
                .item()
                .expect("next_fold_start_offset below MAX implies a fold");
            let end_off = self
                .rope
                .point_to_offset(self.inlay_snapshot.to_buffer_point(fold.range.end));
            let placeholder_chars: Vec<char> = fold.placeholder.text.chars().collect();
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
    folds: Cursor<'a, 'static, Fold, FoldStart>,
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

        if self.buffer_offset <= self.next_fold_end_offset {
            if let Some(fold) = self.folds.item() {
                let start_off = self
                    .rope
                    .point_to_offset(self.inlay_snapshot.to_buffer_point(fold.range.start));
                let placeholder_chars: Vec<char> = fold.placeholder.text.chars().rev().collect();
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
    use super::{FoldMap, FoldOffset, FoldPlaceholder, FoldPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::inlay_map::{InlayKind, InlayMap, InlayPoint},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::{patch::Patch, Bias};

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

    #[test]
    fn row_start_offset_unfolded_matches_rope() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "line0\nlong line one\n\nlast");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (_, snap) = FoldMap::new(inlay_snapshot);

        let rope = buffer_snapshot.rope();
        for row in 0..snap.line_count() {
            assert_eq!(
                snap.row_start_offset(row).0,
                rope.point_to_offset(stoat_text::Point::new(row, 0)),
                "row {row} start offset"
            );
        }
    }

    #[test]
    fn row_start_offset_folded_pins_post_fold_rows() {
        // "line0\nline1\nline2\nline3" with (1,0)..(2,5) folded renders as
        // "line0\n...\nline3": row 1 is the "..." placeholder, row 2 is "line3".
        let snap = make_snapshot_with_folds(
            "line0\nline1\nline2\nline3",
            vec![(InlayPoint::new(1, 0), InlayPoint::new(2, 5))],
        );
        assert_eq!(snap.line_count(), 3);
        assert_eq!(snap.row_start_offset(0).0, 0);
        assert_eq!(snap.row_start_offset(1).0, 6, "row 1 after line0");
        assert_eq!(
            snap.row_start_offset(2).0,
            10,
            "row 2 after placeholder line"
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

    #[test]
    fn overlapping_folds_stored_individually() {
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

        // Storage keeps both folds individually; only rendering coalesces.
        assert_eq!(fold_map.fold_anchor_ranges().len(), 2);
        let (snap, _) = fold_map.sync(inlay_snapshot, &Patch::empty());
        assert_eq!(snap.line_count(), 2);
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
    fn is_line_folded_excludes_end_row_at_column_zero() {
        let snap = make_snapshot_with_folds(
            "line0\nline1\nline2\nline3",
            vec![(InlayPoint::new(0, 5), InlayPoint::new(2, 0))],
        );
        assert!(snap.is_line_folded(0), "start row is folded");
        assert!(snap.is_line_folded(1), "interior row is folded");
        assert!(
            !snap.is_line_folded(2),
            "fold ending at column 0 does not fold the end row"
        );
        assert!(!snap.is_line_folded(3));
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
    fn fold_removed_after_region_deleted() {
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
        assert_eq!(fold_snap.fold_count(), 0);
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
    fn fold_collapses_when_endpoints_merge() {
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
        assert_eq!(fold_snap.fold_count(), 0);
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
            Vec::new(),
            vec![(anchor, ": str".to_string(), InlayKind::Hint)],
        );
        let (inlay_snap2, _) = inlay_map.sync(buffer_snapshot, &Patch::empty());
        assert!(inlay_snap2.has_inlays());

        let (fold_snap2, _) = fold_map.sync(inlay_snap2, &Patch::empty());
        assert!(fold_snap2.inlay_snapshot().has_inlays());
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

    #[test]
    fn incremental_sync_preserves_fold_after_length_changing_edit() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "abc\ndef\nghi\njkl");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap1 = multi_buffer.snapshot();
        let version1 = snap1.version();
        let (mut inlay_map, inlay1) = InlayMap::new(snap1.clone());
        let (mut fold_map, _) = FoldMap::new(inlay1.clone());

        let fold_start = snap1.rope().point_to_offset(stoat_text::Point::new(2, 0));
        let fold_end = snap1.rope().point_to_offset(stoat_text::Point::new(2, 3));
        fold_map.fold(
            vec![snap1.anchor_at(fold_start, Bias::Right)..snap1.anchor_at(fold_end, Bias::Left)],
            FoldPlaceholder::default(),
            &snap1,
        );

        // Settle the fold so the next sync takes the incremental path:
        // version == last_self_version and a cached snapshot is present.
        fold_map.sync(inlay1, &Patch::empty());

        {
            let mut buf = shared.write().unwrap();
            buf.edit(0..0, "XX");
        }

        let snap2 = multi_buffer.snapshot();
        let buffer_edits = snap2.edits_since(version1);
        let (inlay2, inlay_edits) = inlay_map.sync(snap2, &buffer_edits);
        assert!(
            !inlay_edits.is_empty(),
            "edit must drive the incremental path"
        );

        let (fold_snap, _) = fold_map.sync(inlay2, &inlay_edits);
        fold_snap.check_invariants();
        let text: String = fold_snap.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(text, "XXabc\ndef\n...\njkl");
    }

    #[test]
    fn fold_toggle_emits_localized_patch() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "aaa\nbbb\nccc\nddd\neee");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let snap = multi_buffer.snapshot();
        let (_, inlay) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay.clone());

        let fold_start = snap.rope().point_to_offset(stoat_text::Point::new(1, 1));
        let fold_end = snap.rope().point_to_offset(stoat_text::Point::new(3, 1));
        fold_map.fold(
            vec![snap.anchor_at(fold_start, Bias::Right)..snap.anchor_at(fold_end, Bias::Left)],
            FoldPlaceholder::default(),
            &snap,
        );

        // Pure fold toggle: no buffer edit, so the sync's only input is the
        // fold's synthesized region. The row patch must cover just the fold's
        // rows, not 0..line_count as a full rebuild would emit.
        let (fold_snap, patch) = fold_map.sync(inlay, &Patch::empty());

        let edits = patch.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].old.start, 1,
            "patch starts at the fold row, not a full rebuild from row 0"
        );
        assert_eq!(
            fold_snap.line_count(),
            3,
            "rows 1..3 collapse to one display row"
        );
    }

    #[test]
    fn noop_unfold_does_not_bump_version() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "aaa\nbbb\nccc\nddd");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let f_start = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(0, 1));
        let f_end = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(0, 3));
        fold_map.fold(
            vec![
                buffer_snapshot.anchor_at(f_start, Bias::Right)
                    ..buffer_snapshot.anchor_at(f_end, Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        fold_map.sync(inlay_snapshot, &Patch::empty());
        assert!(fold_map.version_unchanged());

        // Unfolding a range that overlaps no fold (the last line) must not
        // invalidate the snapshot cache by bumping the version.
        let u_start = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(3, 0));
        let u_end = buffer_snapshot
            .rope()
            .point_to_offset(stoat_text::Point::new(3, 3));
        #[allow(clippy::single_range_in_vec_init)]
        fold_map.unfold(vec![u_start..u_end], &buffer_snapshot);
        assert!(
            fold_map.version_unchanged(),
            "no-op unfold must not bump version"
        );
    }

    #[test]
    fn fold_query_resolves_lazily_after_edit() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "aaa\nbbb\nccc");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap1 = multi_buffer.snapshot();
        let (_, inlay1) = InlayMap::new(snap1.clone());
        let (mut fold_map, _) = FoldMap::new(inlay1.clone());

        let fs = snap1.rope().point_to_offset(stoat_text::Point::new(1, 0));
        let fe = snap1.rope().point_to_offset(stoat_text::Point::new(1, 3));
        fold_map.fold(
            vec![snap1.anchor_at(fs, Bias::Right)..snap1.anchor_at(fe, Bias::Left)],
            FoldPlaceholder::default(),
            &snap1,
        );
        fold_map.sync(inlay1, &Patch::empty());

        // Insert on row 0, shifting row 1's offsets without touching the fold or
        // re-syncing. The anchor storage tree is never rebuilt, yet the query
        // resolves the fold's anchors against the live buffer.
        {
            shared.write().unwrap().edit(0..0, "ZZ");
        }
        let snap2 = multi_buffer.snapshot();

        let bbb_mid = snap2.rope().point_to_offset(stoat_text::Point::new(1, 1));
        assert!(
            fold_map.is_folded_at_offset(bbb_mid, &snap2),
            "fold tracks the edit via lazy anchor resolution"
        );
        assert!(
            !fold_map.is_folded_at_offset(0, &snap2),
            "text before the fold is not folded"
        );
    }

    #[test]
    fn chars_at_seeks_past_earlier_fold() {
        let snap = make_snapshot_with_folds(
            "aaa bbb ccc ddd",
            vec![
                (InlayPoint::new(0, 3), InlayPoint::new(0, 7)),
                (InlayPoint::new(0, 11), InlayPoint::new(0, 15)),
            ],
        );
        // Start past the first fold's placeholder: the cursor must seek over the
        // earlier fold rather than re-emit it.
        let s: String = snap.chars_at(FoldPoint::new(0, 7)).collect();
        assert_eq!(s, "ccc...");
    }

    #[test]
    fn reversed_chars_at_multi_fold() {
        let snap = make_snapshot_with_folds(
            "aaa bbb ccc ddd",
            vec![
                (InlayPoint::new(0, 3), InlayPoint::new(0, 7)),
                (InlayPoint::new(0, 11), InlayPoint::new(0, 15)),
            ],
        );
        let s: String = snap.reversed_chars_at(snap.max_point()).collect();
        assert_eq!(s, "...ccc ...aaa");
    }

    fn one_char_placeholder(merge_adjacent: bool) -> FoldPlaceholder {
        FoldPlaceholder {
            text: Arc::from("*"),
            collapsed_text: None,
            merge_adjacent,
            type_tag: None,
        }
    }

    /// Folding an early row incrementally must not drop an existing fold's
    /// placeholder on a later row from the transform tree.
    ///
    /// `chars_at` reads the fold tree directly while `len` and `chunks` derive
    /// from the transforms, so a placeholder dropped from the transforms alone
    /// leaves the transform-derived length over-reporting by the folded
    /// region's collapsed bytes and `chunks` reading past the lost fold.
    #[test]
    fn incremental_fold_keeps_later_placeholder() {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "c\nccad\na",
        )));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let fold = |fold_map: &mut FoldMap, start: usize, end: usize| {
            fold_map.fold(
                vec![
                    buffer_snapshot.anchor_at(start, Bias::Right)
                        ..buffer_snapshot.anchor_at(end, Bias::Left),
                ],
                one_char_placeholder(false),
                &buffer_snapshot,
            );
        };

        // Fold "cc" on row 1 and "a" on row 2, settling each so the next sync
        // takes the incremental path.
        fold(&mut fold_map, 2, 4);
        fold_map.sync(inlay_snapshot.clone(), &Patch::empty());
        fold(&mut fold_map, 7, 8);
        fold_map.sync(inlay_snapshot.clone(), &Patch::empty());

        // Folding "c" on row 0 incrementally must keep the row-1 placeholder.
        fold(&mut fold_map, 0, 1);
        let (snapshot, _) = fold_map.sync(inlay_snapshot, &Patch::empty());

        snapshot.check_invariants();

        let text: String = snapshot.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(text, "*\n*ad\n*");
        assert_eq!(snapshot.len().0, 7, "transform output length");

        let chunks: String = snapshot
            .chunks(FoldOffset(0)..snapshot.len(), Arc::from(Vec::new()))
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(chunks, "*\n*ad\n*");

        let after_placeholder: String = snapshot
            .chunks(FoldOffset(3)..FoldOffset(4), Arc::from(Vec::new()))
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(after_placeholder, "a", "chunk after the row-1 placeholder");
    }

    /// Folding a row that abuts an existing fold coalesces the two (with
    /// merge_adjacent placeholders). The incremental rebuild must cover the
    /// existing fold's rows so its placeholder is replaced by the coalesced
    /// one, not left beside it with its collapsed input counted twice.
    #[test]
    fn incremental_fold_coalesces_with_neighbor() {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "\ndaa",
        )));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let fold = |fold_map: &mut FoldMap, start: usize, end: usize| {
            fold_map.fold(
                vec![
                    buffer_snapshot.anchor_at(start, Bias::Right)
                        ..buffer_snapshot.anchor_at(end, Bias::Left),
                ],
                one_char_placeholder(true),
                &buffer_snapshot,
            );
        };

        // Fold the leading newline, settle, then fold the rest. The two folds
        // abut and coalesce into one placeholder spanning the whole buffer.
        fold(&mut fold_map, 0, 1);
        fold_map.sync(inlay_snapshot.clone(), &Patch::empty());
        fold(&mut fold_map, 1, 4);
        let (snapshot, _) = fold_map.sync(inlay_snapshot, &Patch::empty());

        snapshot.check_invariants();

        let text: String = snapshot.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(text, "*");
        assert_eq!(snapshot.len().0, 1, "transform output length");

        let chunks: String = snapshot
            .chunks(FoldOffset(0)..snapshot.len(), Arc::from(Vec::new()))
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(chunks, "*");
    }

    /// An edit whose rows fall inside a multi-row fold must rebuild the whole
    /// fold transform, not emit the fold's pre-edit rows as isomorphic text
    /// beside the rebuilt placeholder (which double-counts that input).
    #[test]
    fn incremental_edit_inside_fold() {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "a\nbb\nc",
        )));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let snap1 = multi_buffer.snapshot();
        let version1 = snap1.version();
        let (mut inlay_map, inlay1) = InlayMap::new(snap1.clone());
        let (mut fold_map, _) = FoldMap::new(inlay1.clone());

        // Fold "a\nbb\n" (offsets [0,5)), spanning rows 0-1, and settle so the
        // edit's sync takes the incremental path.
        fold_map.fold(
            vec![snap1.anchor_at(0, Bias::Right)..snap1.anchor_at(5, Bias::Left)],
            one_char_placeholder(false),
            &snap1,
        );
        fold_map.sync(inlay1, &Patch::empty());

        // Delete a 'b' inside the fold's span.
        shared.write().unwrap().edit(2..3, "");
        let snap2 = multi_buffer.snapshot();
        let buffer_edits = snap2.edits_since(version1);
        let (inlay2, inlay_edits) = inlay_map.sync(snap2, &buffer_edits);
        let (snapshot, _) = fold_map.sync(inlay2, &inlay_edits);

        snapshot.check_invariants();

        let text: String = snapshot.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(text, "*c");
        assert_eq!(snapshot.len().0, 2, "transform output length");

        let chunks: String = snapshot
            .chunks(FoldOffset(0)..snapshot.len(), Arc::from(Vec::new()))
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(chunks, "*c");
    }

    /// As above, but the fold continues past the edited row, so the rebuild
    /// must also carry the fold's trailing rows correctly.
    #[test]
    fn incremental_edit_inside_fold_extending_past_edit() {
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(
            BufferId::new(0),
            "a\nbb\ncc\nd",
        )));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let snap1 = multi_buffer.snapshot();
        let version1 = snap1.version();
        let (mut inlay_map, inlay1) = InlayMap::new(snap1.clone());
        let (mut fold_map, _) = FoldMap::new(inlay1.clone());

        // Fold "a\nbb\ncc\n" (offsets [0,8)), spanning rows 0-2, and settle.
        fold_map.fold(
            vec![snap1.anchor_at(0, Bias::Right)..snap1.anchor_at(8, Bias::Left)],
            one_char_placeholder(false),
            &snap1,
        );
        fold_map.sync(inlay1, &Patch::empty());

        // Delete a 'b' on row 1, inside the fold but before its last rows.
        shared.write().unwrap().edit(2..3, "");
        let snap2 = multi_buffer.snapshot();
        let buffer_edits = snap2.edits_since(version1);
        let (inlay2, inlay_edits) = inlay_map.sync(snap2, &buffer_edits);
        let (snapshot, _) = fold_map.sync(inlay2, &inlay_edits);

        snapshot.check_invariants();

        let text: String = snapshot.chars_at(FoldPoint::new(0, 0)).collect();
        assert_eq!(text, "*d");
        assert_eq!(snapshot.len().0, 2, "transform output length");

        let chunks: String = snapshot
            .chunks(FoldOffset(0)..snapshot.len(), Arc::from(Vec::new()))
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(chunks, "*d");
    }
}
