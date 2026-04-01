use super::inlay_map::{InlayPoint, InlaySnapshot};
use crate::multi_buffer::MultiBufferSnapshot;
use std::{
    any::TypeId,
    cmp::Ordering,
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
    last_self_version: usize,
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
            folds: SumTree::default(),
            next_id: 0,
            version: 0,
            cached_snapshot: Some(Arc::clone(&snapshot)),
            last_inlay_version: inlay_version,
            last_self_version: 0,
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

        let buffer = inlay_snapshot.buffer_snapshot();
        let all_folds: Vec<AnchoredFold> = self.folds.iter().cloned().collect();
        let all_anchors: Vec<Anchor> = all_folds
            .iter()
            .flat_map(|af| [af.range.start, af.range.end])
            .collect();
        let all_points = buffer.points_for_anchors_batch(&all_anchors);
        let mut valid_folds = Vec::new();
        let mut resolved = Vec::new();
        for (i, af) in all_folds.iter().enumerate() {
            let start_pt = all_points[i * 2];
            let end_pt = all_points[i * 2 + 1];
            let start_inlay = inlay_snapshot.to_inlay_point(start_pt);
            let end_inlay = inlay_snapshot.to_inlay_point(end_pt);
            if start_inlay >= end_inlay {
                continue;
            }
            let start_offset = inlay_snapshot
                .rope()
                .point_to_offset(inlay_snapshot.to_buffer_point(start_inlay));
            let end_offset = inlay_snapshot
                .rope()
                .point_to_offset(inlay_snapshot.to_buffer_point(end_inlay));
            let mut valid = af.clone();
            valid.resolved_start = start_offset;
            valid.resolved_end = end_offset;
            valid_folds.push(valid);
            resolved.push(Fold {
                id: af.id,
                range: start_inlay..end_inlay,
                placeholder: af.placeholder.clone(),
            });
        }
        valid_folds.sort_by_key(|f| f.resolved_start);
        self.folds = SumTree::from_iter(valid_folds, ());

        let can_incremental = !inlay_edits.is_empty()
            && self.version == self.last_self_version
            && self.cached_snapshot.is_some();

        resolved.sort_by_key(|f| f.range.start);
        let resolved_tree = SumTree::from_iter(resolved, ());

        let (transforms, edits) = if can_incremental {
            let old_snapshot = self
                .cached_snapshot
                .as_ref()
                .expect("guarded by can_incremental");
            sync_fold_incremental(old_snapshot, &inlay_snapshot, inlay_edits, &resolved_tree)
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

        let edits: Vec<Edit<AnchoredFold>> = new_folds.into_iter().map(Edit::Insert).collect();
        self.folds.edit(edits, ());

        self.merge_overlapping_presorted(buffer_snapshot);
        self.version += 1;
        new_ids
    }

    pub fn unfold(&mut self, ranges: Vec<Range<usize>>, buffer_snapshot: &MultiBufferSnapshot) {
        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        let mut new_folds = SumTree::default();
        for fold in self.folds.iter() {
            if !ranges
                .iter()
                .any(|r| fold.range.overlaps_range(r, &resolve))
            {
                new_folds.push(fold.clone(), ());
            }
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

    fn merge_overlapping_presorted(&mut self, buffer_snapshot: &MultiBufferSnapshot) {
        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        let all: Vec<AnchoredFold> = self.folds.iter().cloned().collect();
        if all.len() <= 1 {
            return;
        }

        let has_overlap = all.windows(2).any(|w| {
            let end = resolve(&w[0].range.end);
            let start = resolve(&w[1].range.start);
            start <= end
        });

        if !has_overlap {
            return;
        }

        let mut merged = Vec::with_capacity(all.len());
        let mut last_end = 0usize;
        for fold in all {
            let fold_range = fold.range.to_offset_range(&resolve);
            if !merged.is_empty() && fold_range.start <= last_end {
                if fold_range.end > last_end {
                    let last: &mut AnchoredFold =
                        merged.last_mut().expect("guarded by !merged.is_empty()");
                    last.range.end = fold.range.end;
                    last.resolved_end = fold_range.end;
                    last_end = fold_range.end;
                }
                continue;
            }
            last_end = fold_range.end;
            merged.push(fold);
        }
        self.folds = SumTree::from_iter(merged, ());
    }
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
                },
                (),
            );
        }
        return transforms;
    }

    let text: &str = if inlay_snapshot.has_inlays() {
        inlay_snapshot.inlay_text()
    } else {
        inlay_snapshot.buffer_snapshot().text()
    };

    let rope = inlay_snapshot.rope();
    let has_inlays = inlay_snapshot.has_inlays();

    let mut scanner = PointScanner::new(text.as_bytes());
    let mut cursor = 0usize;

    for fold in folds.iter() {
        let fold_start = if has_inlays {
            scanner.advance_to(&fold.range.start)
        } else {
            let buf_point = inlay_snapshot.to_buffer_point(fold.range.start);
            rope.point_to_offset(buf_point).min(text.len())
        };
        let fold_end = if has_inlays {
            scanner.advance_to(&fold.range.end)
        } else {
            let buf_point = inlay_snapshot.to_buffer_point(fold.range.end);
            rope.point_to_offset(buf_point).min(text.len())
        };

        if fold_start > cursor {
            let summary = if has_inlays {
                TextSummary::from_str(&text[cursor..fold_start])
            } else {
                rope.text_summary_for_range(cursor..fold_start)
            };
            transforms.push(
                Transform {
                    summary: TransformSummary {
                        input: summary.clone(),
                        output: summary,
                    },
                    placeholder: None,
                },
                (),
            );
        }

        let input_summary = if has_inlays {
            TextSummary::from_str(&text[fold_start..fold_end])
        } else {
            rope.text_summary_for_range(fold_start..fold_end)
        };
        let output_summary = TextSummary::from_str(&fold.placeholder.text);
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input: input_summary,
                    output: output_summary,
                },
                placeholder: Some(fold.placeholder.clone()),
            },
            (),
        );

        cursor = fold_end;
    }

    if cursor < text.len() {
        let summary = if has_inlays {
            TextSummary::from_str(&text[cursor..])
        } else {
            rope.text_summary_for_range(cursor..text.len())
        };
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input: summary.clone(),
                    output: summary,
                },
                placeholder: None,
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
    let has_inlays = inlay_snapshot.has_inlays();
    let rope = inlay_snapshot.rope();
    let text = if has_inlays {
        inlay_snapshot.inlay_text()
    } else {
        inlay_snapshot.buffer_snapshot().text()
    };

    let row_to_offset = |row: u32| -> usize {
        if has_inlays {
            inlay_snapshot.inlay_offset_at_row(row).0
        } else {
            rope.point_to_offset(Point::new(row, 0))
        }
    };

    let text_summary = |a: usize, b: usize| -> TextSummary {
        if has_inlays {
            TextSummary::from_str(&text[a..b])
        } else {
            rope.text_summary_for_range(a..b)
        }
    };

    let mut new_transforms = SumTree::new(());
    let mut cursor = old_snapshot.transforms.cursor::<InputOffset>(());
    let mut row_edits = Patch::empty();

    let mut edits_iter = inlay_edits.into_iter().peekable();
    while let Some(edit) = edits_iter.next() {
        let old_start_offset = row_to_offset(edit.old.start);
        let old_end_offset = row_to_offset(edit.old.end).min(text.len());

        // Preserve unchanged prefix
        new_transforms.append(cursor.slice(&InputOffset(old_start_offset), Bias::Left), ());

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
        let new_start_offset = row_to_offset(edit.new.start);
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
        let new_end_offset = row_to_offset(edit.new.end).min(text.len());
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
                    .min(text.len());
                let fold_end_offset = inlay_snapshot
                    .inlay_point_to_offset(fold.range.end)
                    .0
                    .min(text.len());

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
                .is_none_or(|next| row_to_offset(next.old.start) >= cursor_end)
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

    if new_transforms.is_empty() && !text.is_empty() {
        let summary = if has_inlays {
            TextSummary::from_str(text)
        } else {
            rope.summary().clone()
        };
        new_transforms.push(
            Transform {
                summary: TransformSummary {
                    input: summary.clone(),
                    output: summary,
                },
                placeholder: None,
            },
            (),
        );
    }

    (new_transforms, row_edits)
}

struct PointScanner<'a> {
    bytes: &'a [u8],
    pos: usize,
    row: u32,
}

impl<'a> PointScanner<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            row: 0,
        }
    }

    fn advance_to(&mut self, point: &InlayPoint) -> usize {
        while self.row < point.row() && self.pos < self.bytes.len() {
            if self.bytes[self.pos] == b'\n' {
                self.row += 1;
            }
            self.pos += 1;
        }
        (self.pos + point.column() as usize).min(self.bytes.len())
    }
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

    pub fn to_fold_point(&self, inlay_point: InlayPoint, bias: Bias) -> FoldPoint {
        let (start, end, item) = self
            .transforms
            .find::<Dimensions<InlayPoint, FoldPoint>, _>((), &inlay_point, bias);
        match item {
            Some(t) if t.placeholder.is_some() => {
                if inlay_point.0 == start.0 .0 {
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
        let inlay = self.to_inlay_point(point);
        let clipped = self.inlay_snapshot.clip_point(inlay, bias);
        self.to_fold_point(clipped, bias)
    }

    pub fn fold_count(&self) -> usize {
        self.folds.summary().count
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

    pub fn line_len(&self, fold_row: u32) -> u32 {
        self.fold_line_chars(fold_row)
            .map(|ch| ch.len_utf8() as u32)
            .sum()
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

        // Collect folds from current position forward into a Vec for iteration
        let remaining_folds: Vec<Fold> = {
            let mut cursor = self.folds.cursor::<FoldStart>(());
            cursor.seek(&FoldStart(InlayPoint::default()), Bias::Left);
            let mut folds = Vec::new();
            while let Some(fold) = cursor.item() {
                let end = self.inlay_snapshot.to_buffer_point(fold.range.end);
                if rope.point_to_offset(end) > buffer_offset {
                    folds.push(fold.clone());
                }
                cursor.next();
            }
            folds
        };

        let next_fold_start_offset = remaining_folds.first().map_or(usize::MAX, |f| {
            let start = self.inlay_snapshot.to_buffer_point(f.range.start);
            rope.point_to_offset(start)
        });

        FoldChars {
            inlay_snapshot: &self.inlay_snapshot,
            rope,
            chars,
            buffer_offset,
            remaining_folds,
            fold_idx: 0,
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

        // Collect folds before current position into a Vec for reverse iteration
        let preceding_folds: Vec<Fold> = {
            let mut cursor = self.folds.cursor::<FoldStart>(());
            cursor.seek(&FoldStart(InlayPoint::default()), Bias::Left);
            let mut folds = Vec::new();
            while let Some(fold) = cursor.item() {
                let start = self.inlay_snapshot.to_buffer_point(fold.range.start);
                if rope.point_to_offset(start) < buffer_offset {
                    folds.push(fold.clone());
                }
                cursor.next();
            }
            folds
        };

        let fold_idx = preceding_folds.len();
        let next_fold_end_offset = preceding_folds.last().map_or(0, |f| {
            let end = self.inlay_snapshot.to_buffer_point(f.range.end);
            rope.point_to_offset(end)
        });

        ReversedFoldChars {
            inlay_snapshot: &self.inlay_snapshot,
            rope,
            chars,
            buffer_offset,
            preceding_folds,
            fold_idx,
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

pub struct FoldPointCursor<'a> {
    cursor: Cursor<'a, 'static, Transform, Dimensions<InlayPoint, FoldPoint>>,
}

impl FoldPointCursor<'_> {
    pub fn map(&mut self, inlay_point: InlayPoint, bias: Bias) -> FoldPoint {
        if self.cursor.did_seek() {
            self.cursor.seek_forward(&inlay_point, bias);
        } else {
            self.cursor.seek(&inlay_point, bias);
        }
        let start = self.cursor.start();
        match self.cursor.item() {
            Some(t) if t.placeholder.is_some() => {
                if inlay_point.0 == start.0 .0 {
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
    remaining_folds: Vec<Fold>,
    fold_idx: usize,
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
            let fold = &self.remaining_folds[self.fold_idx];
            let end = self.inlay_snapshot.to_buffer_point(fold.range.end);
            let end_off = self.rope.point_to_offset(end);
            let placeholder_chars: Vec<char> = fold.placeholder.text.chars().collect();
            self.fold_idx += 1;
            self.next_fold_start_offset = if self.fold_idx < self.remaining_folds.len() {
                let start = self
                    .inlay_snapshot
                    .to_buffer_point(self.remaining_folds[self.fold_idx].range.start);
                self.rope.point_to_offset(start)
            } else {
                usize::MAX
            };
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
    preceding_folds: Vec<Fold>,
    fold_idx: usize,
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

        if self.fold_idx > 0 && self.buffer_offset <= self.next_fold_end_offset {
            let fold = &self.preceding_folds[self.fold_idx - 1];
            let start = self.inlay_snapshot.to_buffer_point(fold.range.start);
            let start_off = self.rope.point_to_offset(start);
            let placeholder_chars: Vec<char> = fold.placeholder.text.chars().rev().collect();
            self.fold_idx -= 1;
            self.next_fold_end_offset = if self.fold_idx > 0 {
                let end = self
                    .inlay_snapshot
                    .to_buffer_point(self.preceding_folds[self.fold_idx - 1].range.end);
                self.rope.point_to_offset(end)
            } else {
                0
            };
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
    use super::{FoldMap, FoldPlaceholder, FoldPoint};
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
}
