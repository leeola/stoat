use super::inlay_map::{InlayPoint, InlaySnapshot};
use crate::multi_buffer::MultiBufferSnapshot;
use std::{
    cmp::Ordering,
    ops::{Deref, Range},
    sync::Arc,
};
use stoat_text::{
    Anchor, AnchorRangeExt, Bias, CharsAt, ContextLessSummary, Dimension, Dimensions, Item, Point,
    ReversedCharsAt, Rope, SeekTarget, SumTree, TextSummary,
};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct FoldId(usize);

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
}

impl Default for FoldPlaceholder {
    fn default() -> Self {
        Self {
            text: Arc::from("..."),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Fold {
    pub id: FoldId,
    pub range: Range<InlayPoint>,
    pub placeholder: FoldPlaceholder,
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
}

pub struct FoldMap {
    folds: Vec<AnchoredFold>,
    next_id: usize,
    version: usize,
    cached_snapshot: Option<Arc<FoldSnapshot>>,
    last_inlay_version: usize,
    last_self_version: usize,
}

pub struct FoldSnapshot {
    inlay_snapshot: Arc<InlaySnapshot>,
    transforms: SumTree<Transform>,
    folds: Vec<Fold>,
    version: usize,
}

impl FoldMap {
    pub fn new(inlay_snapshot: Arc<InlaySnapshot>) -> (Self, Arc<FoldSnapshot>) {
        let transforms = build_fold_transforms(&inlay_snapshot, &[]);
        let inlay_version = inlay_snapshot.inlay_version;
        let snapshot = Arc::new(FoldSnapshot {
            inlay_snapshot,
            transforms,
            folds: Vec::new(),
            version: 0,
        });
        let map = FoldMap {
            folds: Vec::new(),
            next_id: 0,
            version: 0,
            cached_snapshot: Some(Arc::clone(&snapshot)),
            last_inlay_version: inlay_version,
            last_self_version: 0,
        };
        (map, snapshot)
    }

    pub fn sync(&mut self, inlay_snapshot: Arc<InlaySnapshot>) -> Arc<FoldSnapshot> {
        if inlay_snapshot.inlay_version == self.last_inlay_version
            && self.version == self.last_self_version
        {
            if let Some(ref cached) = self.cached_snapshot {
                return Arc::clone(cached);
            }
        }
        let buffer = inlay_snapshot.buffer_snapshot();
        let all_anchors: Vec<Anchor> = self
            .folds
            .iter()
            .flat_map(|af| [af.range.start, af.range.end])
            .collect();
        let all_points = buffer.points_for_anchors_batch(&all_anchors);
        let mut valid_folds = Vec::new();
        let mut resolved = Vec::new();
        for (i, af) in self.folds.iter().enumerate() {
            let start_pt = all_points[i * 2];
            let end_pt = all_points[i * 2 + 1];
            let start_inlay = inlay_snapshot.to_inlay_point(start_pt);
            let end_inlay = inlay_snapshot.to_inlay_point(end_pt);
            if start_inlay >= end_inlay {
                continue;
            }
            valid_folds.push(af.clone());
            resolved.push(Fold {
                id: af.id,
                range: start_inlay..end_inlay,
                placeholder: af.placeholder.clone(),
            });
        }
        self.folds = valid_folds;
        let transforms = build_fold_transforms(&inlay_snapshot, &resolved);
        let snapshot = Arc::new(FoldSnapshot {
            inlay_snapshot,
            transforms,
            folds: resolved,
            version: self.version,
        });
        self.last_inlay_version = snapshot.inlay_snapshot.inlay_version;
        self.last_self_version = self.version;
        self.cached_snapshot = Some(Arc::clone(&snapshot));
        snapshot
    }

    pub fn fold(
        &mut self,
        ranges: Vec<Range<Anchor>>,
        placeholder: FoldPlaceholder,
        buffer_snapshot: &MultiBufferSnapshot,
    ) -> Vec<FoldId> {
        let mut new_ids = Vec::with_capacity(ranges.len());
        for range in ranges {
            let id = FoldId(self.next_id);
            self.next_id += 1;
            self.folds.push(AnchoredFold {
                id,
                range,
                placeholder: placeholder.clone(),
            });
            new_ids.push(id);
        }
        self.merge_overlapping(buffer_snapshot);
        self.version += 1;
        new_ids
    }

    pub fn unfold(&mut self, ranges: Vec<Range<usize>>, buffer_snapshot: &MultiBufferSnapshot) {
        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        self.folds
            .retain(|f| !ranges.iter().any(|r| f.range.overlaps_range(r, &resolve)));
        self.version += 1;
    }

    pub fn is_folded_at_offset(
        &self,
        offset: usize,
        buffer_snapshot: &MultiBufferSnapshot,
    ) -> bool {
        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);
        self.folds
            .iter()
            .any(|f| f.range.contains_offset(offset, &resolve))
    }

    pub fn min_anchor_version(&self) -> usize {
        self.folds
            .iter()
            .flat_map(|f| [f.range.start.version, f.range.end.version])
            .min()
            .unwrap_or(self.last_inlay_version)
    }

    /// Merges overlapping fold ranges. When two folds overlap, they are combined
    /// into a single fold retaining the first fold's ID. The second fold's ID
    /// becomes invalid. Callers should not rely on fold IDs surviving across
    /// [`FoldMap::fold`] calls that may create overlapping ranges.
    fn merge_overlapping(&mut self, buffer_snapshot: &MultiBufferSnapshot) {
        if self.folds.len() <= 1 {
            return;
        }

        let resolve = |a: &Anchor| buffer_snapshot.resolve_anchor(a);

        self.folds
            .sort_by(|a, b| a.range.start.cmp(&b.range.start, &resolve));

        let has_overlap = self.folds.windows(2).any(|w| {
            let end = resolve(&w[0].range.end);
            let start = resolve(&w[1].range.start);
            start <= end
        });

        if !has_overlap {
            return;
        }

        let mut merged = Vec::with_capacity(self.folds.len());
        let mut last_end = 0usize;
        for fold in self.folds.drain(..) {
            let fold_range = fold.range.to_offset_range(&resolve);
            if !merged.is_empty() && fold_range.start <= last_end {
                if fold_range.end > last_end {
                    let last: &mut AnchoredFold = merged.last_mut().unwrap();
                    last.range.end = fold.range.end;
                    last_end = fold_range.end;
                }
                continue;
            }
            last_end = fold_range.end;
            merged.push(fold);
        }
        self.folds = merged;
    }
}

fn build_fold_transforms(inlay_snapshot: &InlaySnapshot, folds: &[Fold]) -> SumTree<Transform> {
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

    let rope = &inlay_snapshot.rope;
    let has_inlays = inlay_snapshot.has_inlays();

    let mut scanner = PointScanner::new(text.as_bytes());
    let mut cursor = 0usize;

    for fold in folds {
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
            let summary = TextSummary::from_str(&text[cursor..fold_start]);
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

        let input_summary = TextSummary::from_str(&text[fold_start..fold_end]);
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
        let summary = TextSummary::from_str(&text[cursor..]);
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

    pub fn is_line_folded(&self, inlay_row: u32) -> bool {
        let idx = self
            .folds
            .partition_point(|f| f.range.start.row() <= inlay_row);
        if idx == 0 {
            return false;
        }
        let fold = &self.folds[idx - 1];
        fold.range.end.row() >= inlay_row
            && (fold.range.start.row() != fold.range.end.row()
                || fold.range.start.column() != fold.range.end.column())
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
        let end_idx = self.folds.partition_point(|f| f.range.start < range.end);
        self.folds[..end_idx]
            .iter()
            .filter(|f| f.range.end > range.start)
            .collect()
    }

    pub fn chars_at(&self, fold_point: FoldPoint) -> FoldChars<'_> {
        let inlay_point = self.to_inlay_point(fold_point);
        let buffer_point = self.inlay_snapshot.to_buffer_point(inlay_point);
        let rope = &self.inlay_snapshot.rope;
        let buffer_offset = rope.point_to_offset(buffer_point);
        let chars = rope.chars_at(buffer_offset);

        let fold_idx = self.folds.partition_point(|f| {
            let end = self.inlay_snapshot.to_buffer_point(f.range.end);
            rope.point_to_offset(end) <= buffer_offset
        });

        FoldChars {
            inlay_snapshot: &self.inlay_snapshot,
            rope,
            chars,
            buffer_offset,
            folds: &self.folds,
            fold_idx,
            placeholder_iter: None,
        }
    }

    pub fn reversed_chars_at(&self, fold_point: FoldPoint) -> ReversedFoldChars<'_> {
        let inlay_point = self.to_inlay_point(fold_point);
        let buffer_point = self.inlay_snapshot.to_buffer_point(inlay_point);
        let rope = &self.inlay_snapshot.rope;
        let buffer_offset = rope.point_to_offset(buffer_point);
        let chars = rope.reversed_chars_at(buffer_offset);

        let fold_idx = self.folds.partition_point(|f| {
            let start = self.inlay_snapshot.to_buffer_point(f.range.start);
            rope.point_to_offset(start) < buffer_offset
        });

        ReversedFoldChars {
            inlay_snapshot: &self.inlay_snapshot,
            rope,
            chars,
            buffer_offset,
            folds: &self.folds,
            fold_idx,
            placeholder_iter: None,
        }
    }

    pub fn fold_line(&self, fold_row: u32) -> String {
        self.fold_line_chars(fold_row).collect()
    }
}

pub struct FoldChars<'a> {
    inlay_snapshot: &'a InlaySnapshot,
    rope: &'a Rope,
    chars: CharsAt<'a>,
    buffer_offset: usize,
    folds: &'a [Fold],
    fold_idx: usize,
    placeholder_iter: Option<std::str::Chars<'a>>,
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

        if self.fold_idx < self.folds.len() {
            let fold = &self.folds[self.fold_idx];
            let start = self.inlay_snapshot.to_buffer_point(fold.range.start);
            let start_off = self.rope.point_to_offset(start);
            if self.buffer_offset >= start_off {
                let end = self.inlay_snapshot.to_buffer_point(fold.range.end);
                let end_off = self.rope.point_to_offset(end);
                self.fold_idx += 1;
                self.placeholder_iter = Some(fold.placeholder.text.chars());
                self.chars = self.rope.chars_at(end_off);
                self.buffer_offset = end_off;
                return self.next();
            }
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
    folds: &'a [Fold],
    fold_idx: usize,
    placeholder_iter: Option<std::iter::Rev<std::str::Chars<'a>>>,
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

        if self.fold_idx > 0 {
            let fold = &self.folds[self.fold_idx - 1];
            let end = self.inlay_snapshot.to_buffer_point(fold.range.end);
            let end_off = self.rope.point_to_offset(end);
            if self.buffer_offset <= end_off {
                let start = self.inlay_snapshot.to_buffer_point(fold.range.start);
                let start_off = self.rope.point_to_offset(start);
                self.fold_idx -= 1;
                self.placeholder_iter = Some(fold.placeholder.text.chars().rev());
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
    use super::{FoldMap, FoldPlaceholder, FoldPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::inlay_map::{InlayMap, InlayPoint},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::Bias;

    fn make_snapshot(content: &str) -> Arc<super::FoldSnapshot> {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
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
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
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
                let s_off = buffer_snapshot.rope.point_to_offset(s_buf);
                let e_off = buffer_snapshot.rope.point_to_offset(e_buf);
                buffer_snapshot.anchor_at(s_off, Bias::Right)
                    ..buffer_snapshot.anchor_at(e_off, Bias::Left)
            })
            .collect();
        fold_map.fold(anchor_ranges, FoldPlaceholder::default(), &buffer_snapshot);
        fold_map.sync(inlay_snapshot)
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
        let mut buffer = TextBuffer::new();
        buffer.rope.push("line0\nline1\nline2");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let start_off = buffer_snapshot
            .rope
            .point_to_offset(stoat_text::Point::new(1, 0));
        let end_off = buffer_snapshot
            .rope
            .point_to_offset(stoat_text::Point::new(1, 5));
        let anchor_range = buffer_snapshot.anchor_at(start_off, Bias::Right)
            ..buffer_snapshot.anchor_at(end_off, Bias::Left);
        fold_map.fold(
            vec![anchor_range],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let snap = fold_map.sync(inlay_snapshot.clone());
        assert_eq!(snap.line_count(), 3);

        fold_map.unfold(vec![start_off..end_off], &buffer_snapshot);
        let snap = fold_map.sync(inlay_snapshot);
        assert_eq!(snap.line_count(), 3);
        assert_eq!(
            snap.to_fold_point(InlayPoint::new(1, 3), Bias::Right),
            FoldPoint::new(1, 3)
        );
    }

    #[test]
    fn overlapping_folds_merge() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let to_anchor = |row: u32, col: u32, bias: Bias| {
            let off = buffer_snapshot
                .rope
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
        let snap = fold_map.sync(inlay_snapshot);
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
        let mut buffer = TextBuffer::new();
        buffer.rope.push("line0\nline1\nline2");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());
        let start_off = buffer_snapshot
            .rope
            .point_to_offset(stoat_text::Point::new(0, 3));
        let end_off = buffer_snapshot
            .rope
            .point_to_offset(stoat_text::Point::new(0, 5));
        fold_map.fold(
            vec![
                buffer_snapshot.anchor_at(start_off, Bias::Right)
                    ..buffer_snapshot.anchor_at(end_off, Bias::Left),
            ],
            FoldPlaceholder::default(),
            &buffer_snapshot,
        );
        let snap = fold_map.sync(inlay_snapshot);
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
        let mut buffer = TextBuffer::new();
        buffer.rope.push("line0\nline1\nline2");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let start_off = snap.rope.point_to_offset(stoat_text::Point::new(1, 0));
        let end_off = snap.rope.point_to_offset(stoat_text::Point::new(1, 5));
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
        let fold_snap = fold_map.sync(inlay2);
        assert_eq!(fold_snap.folds.len(), 0);
    }

    #[test]
    fn fold_preserved_after_adjacent_edit() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("aaabbbccc");
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
        let fold_snap = fold_map.sync(inlay2);
        assert_eq!(fold_snap.folds.len(), 1);
    }

    #[test]
    fn fold_collapses_when_endpoints_merge() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("abcXYZdef");
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
        let fold_snap = fold_map.sync(inlay2);
        assert_eq!(fold_snap.folds.len(), 0);
    }

    #[test]
    fn fold_survives_edit_before() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snap.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let start_off = snap.rope.point_to_offset(stoat_text::Point::new(2, 0));
        let end_off = snap.rope.point_to_offset(stoat_text::Point::new(2, 5));
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
        let fold_snap = fold_map.sync(inlay2);
        assert_eq!(fold_snap.fold_line(2), "...");
    }

    #[test]
    fn fold_map_invalidates_on_inlay_splice() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut inlay_map, inlay_snap) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snap);

        let off = buffer_snapshot
            .rope
            .point_to_offset(stoat_text::Point::new(0, 5));
        let anchor = buffer_snapshot.anchor_at(off, Bias::Right);
        inlay_map.splice(Vec::new(), vec![(anchor, ": str".to_string())]);
        let inlay_snap2 = inlay_map.sync(buffer_snapshot);
        assert!(inlay_snap2.has_inlays());

        let fold_snap2 = fold_map.sync(inlay_snap2);
        assert!(fold_snap2.inlay_snapshot().has_inlays());
    }

    #[test]
    fn non_overlapping_folds_no_merge() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("line0\nline1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot.clone());
        let (mut fold_map, _) = FoldMap::new(inlay_snapshot.clone());

        let to_anchor = |row: u32, col: u32, bias: Bias| {
            let off = buffer_snapshot
                .rope
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
        let snap = fold_map.sync(inlay_snapshot);
        assert_eq!(snap.folds.len(), 2);
    }
}
