use crate::{
    display_map::highlights::{BufferChunks, Chunk, HighlightEndpoint},
    multi_buffer::MultiBufferSnapshot,
};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ops::{Add, AddAssign, Deref, Range, Sub},
    sync::Arc,
};
use stoat_text::{
    patch::Patch, Anchor, Bias, ContextLessSummary, Cursor, Dimension, Dimensions, Item, Point,
    Rope, SeekTarget, SumTree, TextSummary,
};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InlayId(usize);

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InlayOffset(pub usize);

impl Add for InlayOffset {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub for InlayOffset {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for InlayOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum InlayKind {
    Hint,
    EditPrediction,
    Other,
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InlayPoint(pub Point);

impl InlayPoint {
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

impl From<Point> for InlayPoint {
    fn from(point: Point) -> Self {
        Self(point)
    }
}

#[derive(Clone, Debug)]
pub struct Inlay {
    pub id: InlayId,
    pub text: Arc<str>,
    pub kind: InlayKind,
}

#[derive(Clone, Debug)]
enum Transform {
    Isomorphic(TextSummary),
    Inlay(Inlay),
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
        match self {
            Transform::Isomorphic(s) => TransformSummary {
                input: s.clone(),
                output: s.clone(),
            },
            Transform::Inlay(inlay) => TransformSummary {
                input: TextSummary::default(),
                output: TextSummary::from_str(&inlay.text),
            },
        }
    }
}

impl<'a> Dimension<'a, TransformSummary> for Point {
    fn zero(_cx: ()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        *self += s.input.lines;
    }
}

impl<'a> Dimension<'a, TransformSummary> for InlayPoint {
    fn zero(_cx: ()) -> Self {
        Self::default()
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        self.0 += s.output.lines;
    }
}

impl<'a> SeekTarget<'a, TransformSummary, Dimensions<Point, InlayPoint>> for InlayPoint {
    fn cmp(&self, cursor_location: &Dimensions<Point, InlayPoint>, _cx: ()) -> Ordering {
        Ord::cmp(self, &cursor_location.1)
    }
}

#[derive(Clone, Debug)]
struct AnchoredInlay {
    id: InlayId,
    position: Anchor,
    text: Arc<str>,
    kind: InlayKind,
}

pub struct InlayMap {
    inlays: Vec<AnchoredInlay>,
    next_id: usize,
    version: usize,
    snapshot_version: usize,
    cached_snapshot: Option<Arc<InlaySnapshot>>,
    last_buffer_version: u64,
    last_self_version: usize,
    inlays_sorted: bool,
    cached_offsets: Vec<usize>,
}

pub struct InlaySnapshot {
    buffer: MultiBufferSnapshot,
    transforms: SumTree<Transform>,
    inlay_count: usize,
    pub inlay_version: usize,
}

impl Deref for InlaySnapshot {
    type Target = MultiBufferSnapshot;
    fn deref(&self) -> &MultiBufferSnapshot {
        &self.buffer
    }
}

impl InlayMap {
    pub fn new(buffer_snapshot: MultiBufferSnapshot) -> (Self, Arc<InlaySnapshot>) {
        let transforms = build_transforms(buffer_snapshot.rope(), buffer_snapshot.text(), &[], &[]);
        let snapshot = Arc::new(InlaySnapshot {
            buffer: buffer_snapshot,
            transforms,
            inlay_count: 0,
            inlay_version: 0,
        });
        let map = InlayMap {
            inlays: Vec::new(),
            next_id: 0,
            version: 0,
            snapshot_version: 0,
            cached_snapshot: Some(Arc::clone(&snapshot)),
            last_buffer_version: snapshot.buffer.version(),
            last_self_version: 0,
            inlays_sorted: true,
            cached_offsets: Vec::new(),
        };
        (map, snapshot)
    }

    pub fn sync(
        &mut self,
        buffer_snapshot: MultiBufferSnapshot,
        buffer_edits: &Patch<usize>,
    ) -> (Arc<InlaySnapshot>, Patch<u32>) {
        if buffer_snapshot.version() == self.last_buffer_version
            && self.version == self.last_self_version
        {
            if let Some(ref cached) = self.cached_snapshot {
                return (Arc::clone(cached), Patch::empty());
            }
        }

        let inlays_changed = self.version != self.last_self_version;
        let buffer_changed = !buffer_edits.is_empty();
        let can_incremental = buffer_changed
            && !inlays_changed
            && self.cached_snapshot.is_some()
            && self.inlays_sorted
            && self.cached_offsets.len() == self.inlays.len();
        // An inlay splice with no buffer edit drives sync_incremental with
        // synthetic empty edits at the changed inlay offsets, so only those
        // rows are patched instead of the whole file.
        let can_incremental_inlay =
            inlays_changed && !buffer_changed && self.cached_snapshot.is_some();

        let (resolved, inlay_offsets) = if can_incremental {
            self.resolve_incremental(&buffer_snapshot, buffer_edits)
        } else {
            self.resolve_all(&buffer_snapshot)
        };

        let splice_edits = if can_incremental_inlay {
            let old_snapshot = self
                .cached_snapshot
                .as_ref()
                .expect("guarded by can_incremental_inlay");
            Some(inlay_splice_edits(old_snapshot, &resolved, &inlay_offsets))
        } else {
            None
        };

        // Skip inlays whose anchor was invalidated (its hinted text deleted):
        // the anchor still resolves to the collapse offset, so without this the
        // hint would linger there until the next LSP refresh. Only the rendered
        // transforms drop it; `self.inlays`/`cached_offsets` keep the full set
        // so the hint reappears if the deletion is undone.
        let valid: Vec<bool> = self
            .inlays
            .iter()
            .map(|ai| buffer_snapshot.is_anchor_valid(&ai.position))
            .collect();
        let resolved: Vec<Inlay> = resolved
            .into_iter()
            .zip(&valid)
            .filter_map(|(inlay, &keep)| keep.then_some(inlay))
            .collect();
        let transform_offsets: Vec<usize> = inlay_offsets
            .iter()
            .zip(&valid)
            .filter_map(|(&off, &keep)| keep.then_some(off))
            .collect();
        let inlay_count = resolved.len();

        let (transforms, edits) = if can_incremental {
            let old_snapshot = self
                .cached_snapshot
                .as_ref()
                .expect("guarded by can_incremental");
            sync_incremental(
                old_snapshot,
                &buffer_snapshot,
                buffer_edits,
                &resolved,
                &transform_offsets,
            )
        } else if let Some(splice_edits) = &splice_edits {
            let old_snapshot = self
                .cached_snapshot
                .as_ref()
                .expect("guarded by can_incremental_inlay");
            sync_incremental(
                old_snapshot,
                &buffer_snapshot,
                splice_edits,
                &resolved,
                &transform_offsets,
            )
        } else {
            // Inlay-output rows, matching new_line_count's space below;
            // line_count() would return buffer rows, which diverge from
            // output rows once a multi-line inlay exists.
            let old_line_count = self
                .cached_snapshot
                .as_ref()
                .map(|s| s.total_summary().lines.row + 1)
                .unwrap_or(0);
            let transforms = build_transforms(
                buffer_snapshot.rope(),
                buffer_snapshot.text(),
                &resolved,
                &transform_offsets,
            );
            let new_line_count = if transforms.is_empty() {
                buffer_snapshot.line_count()
            } else {
                transforms.summary().output.lines.row + 1
            };
            let edits = Patch::new(vec![stoat_text::patch::Edit {
                old: 0..old_line_count,
                new: 0..new_line_count,
            }]);
            (transforms, edits)
        };

        self.cached_offsets = inlay_offsets;
        self.snapshot_version += 1;
        let snapshot = Arc::new(InlaySnapshot {
            buffer: buffer_snapshot,
            transforms,
            inlay_count,
            inlay_version: self.snapshot_version,
        });
        self.last_buffer_version = snapshot.buffer.version();
        self.last_self_version = self.version;
        self.cached_snapshot = Some(Arc::clone(&snapshot));
        (snapshot, edits)
    }

    fn resolve_all(&mut self, buffer_snapshot: &MultiBufferSnapshot) -> (Vec<Inlay>, Vec<usize>) {
        let anchors: Vec<Anchor> = self.inlays.iter().map(|ai| ai.position).collect();
        let mut offsets = buffer_snapshot.resolve_anchors_batch(&anchors);

        if !self.inlays_sorted {
            // Resolved offset order matches point order, so sort the inlays and
            // their offsets together by offset.
            let mut order: Vec<usize> = (0..self.inlays.len()).collect();
            order.sort_by_key(|&i| offsets[i]);
            self.inlays = order.iter().map(|&i| self.inlays[i].clone()).collect();
            offsets = order.iter().map(|&i| offsets[i]).collect();
            self.inlays_sorted = true;
        }

        let text_len = buffer_snapshot.text().len();
        let inlay_offsets: Vec<usize> = offsets.iter().map(|&o| o.min(text_len)).collect();
        let resolved: Vec<Inlay> = self
            .inlays
            .iter()
            .map(|ai| Inlay {
                id: ai.id,
                text: Arc::clone(&ai.text),
                kind: ai.kind,
            })
            .collect();
        (resolved, inlay_offsets)
    }

    /// Only re-resolve anchors for inlays within edit ranges; adjust the rest
    /// by delta.
    fn resolve_incremental(
        &mut self,
        buffer_snapshot: &MultiBufferSnapshot,
        buffer_edits: &Patch<usize>,
    ) -> (Vec<Inlay>, Vec<usize>) {
        let mut offsets = self.cached_offsets.clone();
        let text_len = buffer_snapshot.text().len();
        let mut needs_resolve: Vec<bool> = vec![false; offsets.len()];

        // Process edits in reverse to avoid index shifting issues
        for edit in buffer_edits.into_iter().rev() {
            let delta = (edit.new.end as isize) - (edit.old.end as isize);
            let start_idx = offsets.partition_point(|&o| o < edit.old.start);
            // Inclusive of edit.old.end: an anchor at the edit boundary (e.g. a
            // Bias::Left anchor at an insertion point) must be re-resolved, not
            // blindly delta-shifted, so it can stay put per its bias.
            let end_idx = offsets.partition_point(|&o| o <= edit.old.end);

            for flag in &mut needs_resolve[start_idx..end_idx] {
                *flag = true;
            }

            for offset in &mut offsets[end_idx..] {
                *offset = ((*offset as isize) + delta).max(0) as usize;
            }
        }

        let affected: Vec<(usize, Anchor)> = needs_resolve
            .iter()
            .enumerate()
            .filter(|(_, &needs)| needs)
            .map(|(i, _)| (i, self.inlays[i].position))
            .collect();

        if !affected.is_empty() {
            let anchors: Vec<Anchor> = affected.iter().map(|(_, a)| *a).collect();
            let resolved_offsets = buffer_snapshot.resolve_anchors_batch(&anchors);
            for ((idx, _), offset) in affected.iter().zip(resolved_offsets) {
                offsets[*idx] = offset.min(text_len);
            }
        }

        let inlay_offsets: Vec<usize> = offsets.iter().map(|&o| o.min(text_len)).collect();
        let resolved: Vec<Inlay> = self
            .inlays
            .iter()
            .map(|ai| Inlay {
                id: ai.id,
                text: Arc::clone(&ai.text),
                kind: ai.kind,
            })
            .collect();
        (resolved, inlay_offsets)
    }

    pub fn version_unchanged(&self) -> bool {
        self.version == self.last_self_version
    }

    pub fn splice(
        &mut self,
        remove: Vec<InlayId>,
        insert: Vec<(Anchor, String, InlayKind)>,
    ) -> Vec<InlayId> {
        if !remove.is_empty() {
            let remove_set: HashSet<InlayId> = remove.into_iter().collect();
            self.inlays.retain(|inlay| !remove_set.contains(&inlay.id));
        }

        let mut new_ids = Vec::with_capacity(insert.len());
        for (position, text, kind) in insert {
            let id = InlayId(self.next_id);
            self.next_id += 1;
            self.inlays.push(AnchoredInlay {
                id,
                position,
                text: Arc::from(text),
                kind,
            });
            new_ids.push(id);
        }

        self.inlays_sorted = false;
        self.version += 1;
        new_ids
    }
}

fn build_transforms(
    rope: &Rope,
    text: &str,
    inlays: &[Inlay],
    inlay_offsets: &[usize],
) -> SumTree<Transform> {
    let mut transforms = SumTree::new(());

    if inlays.is_empty() {
        if !text.is_empty() {
            transforms.push(Transform::Isomorphic(rope.summary().clone()), ());
        }
        return transforms;
    }

    let mut cursor = 0usize;

    for (inlay, &offset) in inlays.iter().zip(inlay_offsets) {
        let offset = offset.min(text.len());

        if offset > cursor {
            transforms.push(
                Transform::Isomorphic(rope.text_summary_for_range(cursor..offset)),
                (),
            );
        }
        transforms.push(Transform::Inlay(inlay.clone()), ());
        cursor = offset;
    }

    if cursor < text.len() {
        transforms.push(
            Transform::Isomorphic(rope.text_summary_for_range(cursor..text.len())),
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

impl<'a> Dimension<'a, TransformSummary> for InlayOffset {
    fn zero(_cx: ()) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        self.0 += s.output.len;
    }
}

/// Accumulates the inlay-space (`output`) text summary across transforms, so a
/// cursor can sum the interior of a range in O(log n) for
/// [`InlaySnapshot::text_summary_for_range`].
#[derive(Clone, Default)]
struct OutputTextSummary(TextSummary);

impl<'a> Dimension<'a, TransformSummary> for OutputTextSummary {
    fn zero(_cx: ()) -> Self {
        Self(TextSummary::default())
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        ContextLessSummary::add_summary(&mut self.0, &s.output);
    }
}

pub(super) type OutputOffset = InlayOffset;

impl<'a> SeekTarget<'a, TransformSummary, Dimensions<OutputOffset, Point, InlayPoint>>
    for InlayPoint
{
    fn cmp(
        &self,
        cursor_location: &Dimensions<OutputOffset, Point, InlayPoint>,
        _cx: (),
    ) -> Ordering {
        Ord::cmp(self, &cursor_location.2)
    }
}

fn push_isomorphic(tree: &mut SumTree<Transform>, summary: TextSummary) {
    if summary.len == 0 {
        return;
    }
    let mut summary = Some(summary);
    tree.update_last(
        |t| {
            if let Transform::Isomorphic(existing) = t {
                ContextLessSummary::add_summary(existing, &summary.take().expect("set on entry"));
            }
        },
        (),
    );
    if let Some(s) = summary {
        tree.push(Transform::Isomorphic(s), ());
    }
}

fn sync_incremental(
    old_snapshot: &InlaySnapshot,
    buffer_snapshot: &MultiBufferSnapshot,
    buffer_edits: &Patch<usize>,
    resolved_inlays: &[Inlay],
    inlay_offsets: &[usize],
) -> (SumTree<Transform>, Patch<u32>) {
    let old_rope = old_snapshot.buffer.rope();
    let new_rope = buffer_snapshot.rope();
    let new_text = buffer_snapshot.text();

    let mut new_transforms = SumTree::new(());
    let mut cursor = old_snapshot.transforms.cursor::<InputOffset>(());
    let mut row_edits = Patch::empty();
    let mut inlay_ix = 0;

    let mut edits_iter = buffer_edits.into_iter().peekable();
    while let Some(edit) = edits_iter.next() {
        // Preserve unchanged prefix
        new_transforms.append(cursor.slice(&InputOffset(edit.old.start), Bias::Left), ());

        // If cursor item ends exactly at edit start, merge it with prefix
        if let Some(Transform::Isomorphic(summary)) = cursor.item() {
            if cursor.start().0 + summary.len == edit.old.start {
                push_isomorphic(&mut new_transforms, summary.clone());
                cursor.next();
            }
        }

        // Record old output rows
        let old_start_point = old_rope.offset_to_point(edit.old.start);
        let old_end_point = old_rope.offset_to_point(edit.old.end);
        let old_inlay_start_row = old_snapshot.to_inlay_point(old_start_point).row();
        let old_inlay_end_row = if edit.old.start == edit.old.end {
            old_inlay_start_row + 1
        } else {
            old_snapshot.to_inlay_point(old_end_point).row() + 1
        };

        // Seek past old content
        cursor.seek_forward(&InputOffset(edit.old.end), Bias::Right);

        // Push gap from current new position to edit.new.start
        let current_pos = new_transforms.summary().input.len;
        if edit.new.start > current_pos {
            push_isomorphic(
                &mut new_transforms,
                new_rope.text_summary_for_range(current_pos..edit.new.start),
            );
        }
        let new_start_row = new_transforms.summary().output.lines.row;

        // Skip inlays before this edit
        while inlay_ix < inlay_offsets.len() && inlay_offsets[inlay_ix] < edit.new.start {
            inlay_ix += 1;
        }

        // Insert inlays within the edit range
        while inlay_ix < inlay_offsets.len() && inlay_offsets[inlay_ix] <= edit.new.end {
            let inlay_off = inlay_offsets[inlay_ix];
            let current_pos = new_transforms.summary().input.len;
            if inlay_off > current_pos {
                push_isomorphic(
                    &mut new_transforms,
                    new_rope.text_summary_for_range(current_pos..inlay_off),
                );
            }
            new_transforms.push(Transform::Inlay(resolved_inlays[inlay_ix].clone()), ());
            inlay_ix += 1;
        }

        // Push remaining text to edit.new.end
        let current_pos = new_transforms.summary().input.len;
        if edit.new.end > current_pos {
            push_isomorphic(
                &mut new_transforms,
                new_rope.text_summary_for_range(current_pos..edit.new.end),
            );
        }

        // `new_out.row` sits past the edit's new content; +1 covers the
        // row it lands on. When the content is newline-terminated that row
        // holds the tail shifted down by the edit, so it must be included.
        // Over-spanning by a row is harmless; under-spanning drops the
        // tail's row from downstream invalidation.
        let new_end_row = new_transforms.summary().output.lines.row + 1;

        row_edits.push(stoat_text::patch::Edit {
            old: old_inlay_start_row..old_inlay_end_row,
            new: new_start_row..new_end_row,
        });

        // Handle tail of current transform
        if let Some(item) = cursor.item() {
            let cursor_end = cursor.start().0 + item.summary(()).input.len;
            if edits_iter
                .peek()
                .is_none_or(|next| next.old.start >= cursor_end)
            {
                let tail = cursor_end - edit.old.end;
                let tail_end_new = edit.new.end + tail;
                let current_pos = new_transforms.summary().input.len;
                if tail_end_new > current_pos {
                    push_isomorphic(
                        &mut new_transforms,
                        new_rope.text_summary_for_range(current_pos..tail_end_new),
                    );
                }
                cursor.next();
            }
        }
    }

    new_transforms.append(cursor.suffix(), ());

    if new_transforms.is_empty() && !new_text.is_empty() {
        new_transforms.push(Transform::Isomorphic(new_rope.summary().clone()), ());
    }

    (new_transforms, row_edits)
}

/// Synthetic empty edits at the buffer offsets where the inlay set changed.
///
/// Diffs the cached snapshot's inlays against the freshly resolved set by id;
/// the buffer is unchanged, so a surviving inlay keeps its offset. A removed
/// inlay contributes its old offset, an inserted one its new offset, a moved
/// one both. Routing these `offset..offset` edits through [`sync_incremental`]
/// reconstructs only the affected rows rather than the whole file.
fn inlay_splice_edits(
    old_snapshot: &InlaySnapshot,
    new_inlays: &[Inlay],
    new_offsets: &[usize],
) -> Patch<usize> {
    // Each inlay's buffer offset is the running sum of isomorphic input
    // lengths up to its transform (inlays contribute no input).
    let mut old: HashMap<usize, usize> = HashMap::new();
    let mut offset = 0usize;
    for transform in old_snapshot.transforms.iter() {
        match transform {
            Transform::Isomorphic(summary) => offset += summary.len,
            Transform::Inlay(inlay) => {
                old.insert(inlay.id.0, offset);
            },
        }
    }

    let mut offsets: Vec<usize> = Vec::new();
    let mut new: HashMap<usize, usize> = HashMap::with_capacity(new_inlays.len());
    for (inlay, &offset) in new_inlays.iter().zip(new_offsets) {
        new.insert(inlay.id.0, offset);
        if old.get(&inlay.id.0) != Some(&offset) {
            offsets.push(offset);
        }
    }
    for (id, &offset) in &old {
        if !new.contains_key(id) {
            offsets.push(offset);
        }
    }

    offsets.sort_unstable();
    offsets.dedup();
    Patch::new(
        offsets
            .into_iter()
            .map(|offset| stoat_text::patch::Edit {
                old: offset..offset,
                new: offset..offset,
            })
            .collect(),
    )
}

fn point_overshoot(base: Point, target: Point) -> Point {
    if target.row == base.row {
        Point::new(0, target.column - base.column)
    } else {
        Point::new(target.row - base.row, target.column)
    }
}

impl InlaySnapshot {
    pub fn to_inlay_point(&self, buffer_point: Point) -> InlayPoint {
        let (start, _end, item) = self.transforms.find::<Dimensions<Point, InlayPoint>, _>(
            (),
            &buffer_point,
            Bias::Right,
        );
        match item {
            Some(Transform::Isomorphic(_)) | None => {
                let overshoot = point_overshoot(start.0, buffer_point);
                InlayPoint(start.1 .0 + overshoot)
            },
            Some(Transform::Inlay(_)) => start.1,
        }
    }

    pub fn to_buffer_point(&self, inlay_point: InlayPoint) -> Point {
        let (start, _end, item) =
            self.transforms
                .find::<Dimensions<Point, InlayPoint>, _>((), &inlay_point, Bias::Right);
        match item {
            Some(Transform::Isomorphic(_)) | None => {
                let overshoot = point_overshoot(start.1 .0, inlay_point.0);
                start.0 + overshoot
            },
            Some(Transform::Inlay(_)) => start.0,
        }
    }

    pub fn clip_point(&self, point: InlayPoint, _bias: Bias) -> InlayPoint {
        let buf = self.to_buffer_point(point);
        let max_row = self.buffer.line_count().saturating_sub(1);
        let row = buf.row.min(max_row);
        let line_len = self.buffer.rope().line_len(row);
        let col = buf.column.min(line_len);
        self.to_inlay_point(Point::new(row, col))
    }

    pub fn line_count(&self) -> u32 {
        self.buffer.line_count()
    }

    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        &self.buffer
    }

    pub fn total_summary(&self) -> TextSummary {
        self.transforms.summary().output.clone()
    }

    /// Text summary of an inlay-offset range without materializing the text.
    ///
    /// Walks the transform tree summing the `output` (inlay-space) summary:
    /// a partial summary at each boundary transform (a rope summary for an
    /// isomorphic span, an inlay-text slice for an inlay) and an O(log n)
    /// interior via `cursor.summary`. Replaces slicing a whole-file
    /// [`InlaySnapshot::inlay_text`] String. Range bounds must fall on
    /// character boundaries (inlay/fold offsets do).
    pub fn text_summary_for_range(&self, range: Range<InlayOffset>) -> TextSummary {
        let mut summary = TextSummary::default();

        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InlayOffset, InputOffset>>(());
        cursor.seek(&range.start, Bias::Right);

        let overshoot = range.start.0 - cursor.start().0 .0;
        match cursor.item() {
            Some(Transform::Isomorphic(_)) => {
                let buffer_start = cursor.start().1 .0;
                let suffix_start = buffer_start + overshoot;
                let suffix_end =
                    buffer_start + (cursor.end().0 .0.min(range.end.0) - cursor.start().0 .0);
                summary = self
                    .buffer
                    .rope()
                    .text_summary_for_range(suffix_start..suffix_end);
                cursor.next();
            },
            Some(Transform::Inlay(inlay)) => {
                let suffix_start = overshoot;
                let suffix_end = cursor.end().0 .0.min(range.end.0) - cursor.start().0 .0;
                summary = TextSummary::from_str(&inlay.text[suffix_start..suffix_end]);
                cursor.next();
            },
            None => {},
        }

        if range.end.0 > cursor.start().0 .0 {
            let interior: OutputTextSummary =
                cursor.summary(&InlayOffset(range.end.0), Bias::Right);
            ContextLessSummary::add_summary(&mut summary, &interior.0);

            let overshoot = range.end.0 - cursor.start().0 .0;
            match cursor.item() {
                Some(Transform::Isomorphic(_)) => {
                    let buffer_start = cursor.start().1 .0;
                    let prefix = self
                        .buffer
                        .rope()
                        .text_summary_for_range(buffer_start..buffer_start + overshoot);
                    ContextLessSummary::add_summary(&mut summary, &prefix);
                },
                Some(Transform::Inlay(inlay)) => {
                    let prefix = TextSummary::from_str(&inlay.text[0..overshoot]);
                    ContextLessSummary::add_summary(&mut summary, &prefix);
                },
                None => {},
            }
        }

        summary
    }

    pub fn line_len(&self, row: u32) -> u32 {
        let len = self.buffer.rope().line_len(row);
        if !self.has_inlays() {
            return len;
        }
        let target = Point::new(row, 0);
        let mut cursor = self.transforms.cursor::<Point>(());
        cursor.seek(&target, Bias::Left);
        let mut extra = 0u32;
        while let Some(transform) = cursor.item() {
            let pos: Point = *cursor.start();
            if pos.row > row {
                break;
            }
            if let Transform::Inlay(ref inlay) = transform {
                if pos.row == row {
                    extra += inlay.text.len() as u32;
                }
            }
            cursor.next();
        }
        len + extra
    }

    pub fn has_inlays(&self) -> bool {
        self.inlay_count > 0
    }

    pub fn inlay_point_to_offset(&self, point: InlayPoint) -> InlayOffset {
        if !self.has_inlays() {
            return InlayOffset(self.buffer.rope().point_to_offset(point.0));
        }
        let (start, _end, item) = self
            .transforms
            .find::<Dimensions<OutputOffset, Point, InlayPoint>, _>((), &point, Bias::Right);
        match item {
            Some(Transform::Isomorphic(_)) | None => {
                let overshoot = point_overshoot(start.2 .0, point.0);
                let buffer_point = start.1 + overshoot;
                let buffer_offset = self.buffer.rope().point_to_offset(buffer_point);
                let start_buffer_offset = self.buffer.rope().point_to_offset(start.1);
                InlayOffset(start.0 .0 + (buffer_offset - start_buffer_offset))
            },
            Some(Transform::Inlay(_)) => start.0,
        }
    }

    pub fn inlay_offset_at_row(&self, row: u32) -> InlayOffset {
        self.inlay_point_to_offset(InlayPoint::new(row, 0))
    }

    /// The whole inlay-mapped text as a String. Test-only oracle for
    /// [`InlaySnapshot::text_summary_for_range`]; production reads summaries,
    /// not the materialized text.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inlay_text(&self) -> String {
        let buffer_text = self.buffer.text();
        let mut result = String::new();
        let mut buffer_offset = 0usize;

        for transform in self.transforms.iter() {
            match transform {
                Transform::Isomorphic(s) => {
                    let end = buffer_offset + s.len;
                    result.push_str(&buffer_text[buffer_offset..end]);
                    buffer_offset = end;
                },
                Transform::Inlay(inlay) => {
                    result.push_str(&inlay.text);
                },
            }
        }
        result
    }

    pub fn inlay_point_cursor(&self) -> InlayPointCursor<'_> {
        InlayPointCursor {
            cursor: self.transforms.cursor::<Dimensions<Point, InlayPoint>>(()),
        }
    }

    /// Stream [`Chunk`]s covering `range` with highlight styles merged in.
    ///
    /// Walks the inlay transform tree and interleaves buffer text (from
    /// [`BufferChunks`]) with inserted inlay text. Inlay text is emitted
    /// unstyled and tagged via [`Chunk::is_inlay`] and [`Chunk::inlay_kind`].
    ///
    /// `endpoints` must be sorted over the buffer byte range that corresponds
    /// to `range`. Inlay bytes contribute no highlights and are skipped over
    /// when consulting endpoints.
    ///
    /// Fast path: when the snapshot has zero inlays, delegates directly to a
    /// single [`BufferChunks`] over the matching buffer range without any
    /// transform cursor work.
    pub fn chunks<'a>(
        &'a self,
        range: Range<InlayOffset>,
        endpoints: Arc<[HighlightEndpoint]>,
    ) -> InlayChunks<'a> {
        if !self.has_inlays() {
            return InlayChunks::Passthrough(Box::new(BufferChunks::new(
                self.buffer.rope(),
                range.start.0..range.end.0,
                endpoints,
            )));
        }

        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InlayOffset, InputOffset>>(());
        cursor.seek(&range.start, Bias::Right);

        InlayChunks::Transforming(Box::new(InlayChunksInner {
            snapshot: self,
            endpoints,
            cursor,
            buffer_chunks: None,
            offset: range.start,
            end: range.end,
        }))
    }
}

/// Iterator returned by [`InlaySnapshot::chunks`].
pub enum InlayChunks<'a> {
    /// Snapshot has no inlays; this is a thin wrapper around [`BufferChunks`].
    Passthrough(Box<BufferChunks<'a>>),
    /// Snapshot has at least one inlay; walks transforms to interleave inlay
    /// text with buffer chunks.
    Transforming(Box<InlayChunksInner<'a>>),
}

#[doc(hidden)]
pub struct InlayChunksInner<'a> {
    snapshot: &'a InlaySnapshot,
    endpoints: Arc<[HighlightEndpoint]>,
    cursor: Cursor<'a, 'static, Transform, Dimensions<InlayOffset, InputOffset>>,
    buffer_chunks: Option<BufferChunks<'a>>,
    offset: InlayOffset,
    end: InlayOffset,
}

impl<'a> Iterator for InlayChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        match self {
            InlayChunks::Passthrough(bc) => bc.next(),
            InlayChunks::Transforming(inner) => inner.next(),
        }
    }
}

impl<'a> InlayChunksInner<'a> {
    fn next(&mut self) -> Option<Chunk<'a>> {
        loop {
            if self.offset >= self.end {
                return None;
            }

            if let Some(bc) = self.buffer_chunks.as_mut() {
                if let Some(chunk) = bc.next() {
                    let len = chunk.text.len();
                    self.offset.0 += len;
                    return Some(chunk);
                }
                self.buffer_chunks = None;
                self.cursor.next();
                continue;
            }

            let transform = self.cursor.item()?;
            let cursor_start = self.cursor.start();
            let cursor_end = self.cursor.end();
            let trans_start_inlay = cursor_start.0;
            let trans_end_inlay = cursor_end.0;
            let trans_start_buf = cursor_start.1 .0;

            if trans_start_inlay.0 >= self.end.0 {
                return None;
            }

            match transform {
                Transform::Isomorphic(_) => {
                    let local_start_inlay = self.offset.0.max(trans_start_inlay.0);
                    let local_end_inlay = self.end.0.min(trans_end_inlay.0);
                    let local_start_buf =
                        trans_start_buf + (local_start_inlay - trans_start_inlay.0);
                    let local_end_buf = trans_start_buf + (local_end_inlay - trans_start_inlay.0);
                    self.buffer_chunks = Some(BufferChunks::new(
                        self.snapshot.buffer.rope(),
                        local_start_buf..local_end_buf,
                        self.endpoints.clone(),
                    ));
                },
                Transform::Inlay(inlay) => {
                    let inlay_text: &'a str = inlay.text.as_ref();
                    let kind = inlay.kind;
                    let trans_end = trans_end_inlay;
                    self.cursor.next();
                    self.offset = trans_end;
                    return Some(Chunk {
                        text: Cow::Borrowed(inlay_text),
                        is_inlay: true,
                        inlay_kind: Some(kind),
                        highlight_style: None,
                        ..Default::default()
                    });
                },
            }
        }
    }
}

pub struct InlayPointCursor<'a> {
    cursor: Cursor<'a, 'static, Transform, Dimensions<Point, InlayPoint>>,
}

impl InlayPointCursor<'_> {
    pub fn map(&mut self, buffer_point: Point) -> InlayPoint {
        if self.cursor.did_seek() {
            self.cursor.seek_forward(&buffer_point, Bias::Right);
        } else {
            self.cursor.seek(&buffer_point, Bias::Right);
        }
        let start = self.cursor.start();
        match self.cursor.item() {
            Some(Transform::Isomorphic(_)) | None => {
                let overshoot = point_overshoot(start.0, buffer_point);
                InlayPoint(start.1 .0 + overshoot)
            },
            Some(Transform::Inlay(_)) => start.1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InlayKind, InlayMap, InlayOffset, InlayPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::{patch::Patch, Bias, Point, TextSummary};

    fn make_snapshot(content: &str) -> Arc<super::InlaySnapshot> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, snapshot) = InlayMap::new(buffer_snapshot);
        snapshot
    }

    fn make_snapshot_with_inlays(
        content: &str,
        inlays: Vec<(Point, String)>,
    ) -> Arc<super::InlaySnapshot> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(buffer_snapshot.clone());
        let anchored_inlays = inlays
            .into_iter()
            .map(|(pos, text)| {
                let off = buffer_snapshot.rope().point_to_offset(pos);
                (
                    buffer_snapshot.anchor_at(off, Bias::Right),
                    text,
                    InlayKind::Hint,
                )
            })
            .collect();
        map.splice(Vec::new(), anchored_inlays);
        let (snapshot, _) = map.sync(buffer_snapshot, &Patch::empty());
        snapshot
    }

    #[test]
    fn text_summary_for_range_matches_inlay_text_slicing() {
        let snap = make_snapshot_with_inlays(
            "hello world\nsecond line",
            vec![
                (Point::new(0, 5), ": str".to_string()),
                (Point::new(1, 6), " hint".to_string()),
            ],
        );
        let text = snap.inlay_text().to_string();
        let total = snap.total_summary().len;
        assert_eq!(
            total,
            text.len(),
            "total summary length is the inlay text len"
        );

        // Ranges include ones starting/ending inside an inlay span and a
        // full-document range, to exercise both boundary partials and the
        // interior sum.
        let ranges = [0..total, 0..4, 6..10, 5..14, 14..total, total..total];
        for range in ranges {
            let expected = TextSummary::from_str(&text[range.clone()]);
            let got = snap.text_summary_for_range(InlayOffset(range.start)..InlayOffset(range.end));
            assert_eq!(got.len, expected.len, "len mismatch for {range:?}");
            assert_eq!(got.lines, expected.lines, "lines mismatch for {range:?}");
        }
    }

    #[test]
    fn text_summary_for_range_no_inlays() {
        let snap = make_snapshot("hello\nworld\nfoo");
        let text = snap.buffer_snapshot().text();
        let total = snap.total_summary().len;
        for range in [0..total, 0..6, 6..12, 3..total, total..total] {
            let expected = TextSummary::from_str(&text[range.clone()]);
            let got = snap.text_summary_for_range(InlayOffset(range.start)..InlayOffset(range.end));
            assert_eq!(got.len, expected.len, "len mismatch for {range:?}");
            assert_eq!(got.lines, expected.lines, "lines mismatch for {range:?}");
        }
    }

    #[test]
    fn splice_incremental_matches_full_rebuild() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "alpha\nbeta\ngamma\ndelta");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let anchor_at = |row: u32, col: u32| {
            let off = buffer_snapshot.rope().point_to_offset(Point::new(row, col));
            buffer_snapshot.anchor_at(off, Bias::Right)
        };

        // First hint, full-rebuilt on the initial sync.
        let (mut map, _) = InlayMap::new(buffer_snapshot.clone());
        let a_ids = map.splice(
            Vec::new(),
            vec![(anchor_at(0, 5), ": A".to_string(), InlayKind::Hint)],
        );
        let _ = map.sync(buffer_snapshot.clone(), &Patch::empty());

        // Remove A, add B two rows down: drives the incremental splice path.
        map.splice(
            a_ids,
            vec![(anchor_at(2, 5), ": B".to_string(), InlayKind::Hint)],
        );
        let (inc_snap, edits) = map.sync(buffer_snapshot.clone(), &Patch::empty());

        // Oracle: a fresh map holding only the final inlay (full rebuild).
        let (mut full_map, _) = InlayMap::new(buffer_snapshot.clone());
        full_map.splice(
            Vec::new(),
            vec![(anchor_at(2, 5), ": B".to_string(), InlayKind::Hint)],
        );
        let (full_snap, _) = full_map.sync(buffer_snapshot.clone(), &Patch::empty());

        assert_eq!(
            inc_snap.inlay_text(),
            full_snap.inlay_text(),
            "incremental splice must match a full rebuild"
        );

        let total_rows = inc_snap.total_summary().lines.row + 1;
        assert!(!edits.is_empty(), "splice must emit a row patch");
        assert!(
            !edits
                .edits()
                .iter()
                .any(|e| e.new.start == 0 && e.new.end >= total_rows),
            "splice row patch must be localized, got {:?}",
            edits.edits()
        );
    }

    #[test]
    fn passthrough_no_inlays() {
        let snap = make_snapshot("hello\nworld");
        let point = Point::new(1, 3);
        let inlay = snap.to_inlay_point(point);
        assert_eq!(inlay, InlayPoint::new(1, 3));
        let back = snap.to_buffer_point(inlay);
        assert_eq!(back, point);
    }

    #[test]
    fn single_inlay() {
        let snap =
            make_snapshot_with_inlays("hello world", vec![(Point::new(0, 5), ": str".to_string())]);
        assert_eq!(snap.to_inlay_point(Point::new(0, 0)), InlayPoint::new(0, 0));
        assert_eq!(
            snap.to_inlay_point(Point::new(0, 5)),
            InlayPoint::new(0, 10)
        );
        assert_eq!(
            snap.to_inlay_point(Point::new(0, 6)),
            InlayPoint::new(0, 11)
        );

        assert_eq!(
            snap.to_buffer_point(InlayPoint::new(0, 0)),
            Point::new(0, 0)
        );
        assert_eq!(
            snap.to_buffer_point(InlayPoint::new(0, 10)),
            Point::new(0, 5)
        );
        assert_eq!(
            snap.to_buffer_point(InlayPoint::new(0, 11)),
            Point::new(0, 6)
        );
    }

    #[test]
    fn inside_inlay_snaps_to_position() {
        let snap =
            make_snapshot_with_inlays("hello world", vec![(Point::new(0, 5), ": str".to_string())]);
        assert_eq!(
            snap.to_buffer_point(InlayPoint::new(0, 7)),
            Point::new(0, 5)
        );
    }

    #[test]
    fn multiple_inlays() {
        let snap = make_snapshot_with_inlays(
            "ab cd ef",
            vec![
                (Point::new(0, 2), "X".to_string()),
                (Point::new(0, 5), "YY".to_string()),
            ],
        );
        // "ab" + "X" + " cd" + "YY" + " ef"
        assert_eq!(snap.to_inlay_point(Point::new(0, 0)), InlayPoint::new(0, 0));
        assert_eq!(snap.to_inlay_point(Point::new(0, 2)), InlayPoint::new(0, 3));
        assert_eq!(snap.to_inlay_point(Point::new(0, 5)), InlayPoint::new(0, 8));
        assert_eq!(
            snap.to_inlay_point(Point::new(0, 8)),
            InlayPoint::new(0, 11)
        );

        assert_eq!(
            snap.to_buffer_point(InlayPoint::new(0, 0)),
            Point::new(0, 0)
        );
        assert_eq!(
            snap.to_buffer_point(InlayPoint::new(0, 3)),
            Point::new(0, 2)
        );
        assert_eq!(
            snap.to_buffer_point(InlayPoint::new(0, 8)),
            Point::new(0, 5)
        );
        assert_eq!(
            snap.to_buffer_point(InlayPoint::new(0, 11)),
            Point::new(0, 8)
        );
    }

    #[test]
    fn out_of_order_inlays_render_sorted() {
        // Inserted with the later position first; resolve_all must sort them by
        // resolved offset before building transforms.
        let snap = make_snapshot_with_inlays(
            "ab cd ef",
            vec![
                (Point::new(0, 5), "YY".to_string()),
                (Point::new(0, 2), "X".to_string()),
            ],
        );
        assert_eq!(snap.inlay_text(), "abX cdYY ef");
    }

    #[test]
    fn left_biased_inlay_stays_at_insertion_point() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "helloworld");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let snap0 = multi_buffer.snapshot();
        let version = snap0.version();

        let (mut map, _) = InlayMap::new(snap0.clone());
        let anchor = snap0.anchor_at(5, Bias::Left);
        map.splice(
            Vec::new(),
            vec![(anchor, ": X".to_string(), InlayKind::Hint)],
        );
        let _ = map.sync(snap0, &Patch::empty());

        // Insert exactly at the Left-biased anchor's offset, driving
        // resolve_incremental: the anchor must stay at 5, not shift right.
        {
            let mut buf = shared.write().unwrap();
            buf.edit(5..5, "ZZ");
        }
        let snap1 = multi_buffer.snapshot();
        let edits = snap1.edits_since(version);
        assert!(!edits.is_empty(), "edit must drive resolve_incremental");
        let (inlay_snap, _) = map.sync(snap1, &edits);

        assert_eq!(inlay_snap.inlay_text(), "hello: XZZworld");
    }

    #[test]
    fn splice_add_and_remove() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(buffer_snapshot.clone());

        let off = buffer_snapshot.rope().point_to_offset(Point::new(0, 5));
        let anchor = buffer_snapshot.anchor_at(off, Bias::Right);
        let ids = map.splice(
            Vec::new(),
            vec![(anchor, ": str".to_string(), InlayKind::Hint)],
        );
        let (snap, _) = map.sync(buffer_snapshot.clone(), &Patch::empty());
        assert_eq!(
            snap.to_inlay_point(Point::new(0, 5)),
            InlayPoint::new(0, 10)
        );

        map.splice(ids, Vec::new());
        let (snap, _) = map.sync(buffer_snapshot, &Patch::empty());
        assert_eq!(snap.to_inlay_point(Point::new(0, 5)), InlayPoint::new(0, 5));
    }

    #[test]
    fn line_len_no_inlays() {
        let snap = make_snapshot("hello\nworld");
        assert_eq!(snap.line_len(0), 5);
        assert_eq!(snap.line_len(1), 5);
    }

    #[test]
    fn line_len_with_inlay() {
        let snap =
            make_snapshot_with_inlays("hello world", vec![(Point::new(0, 5), ": str".to_string())]);
        assert_eq!(snap.line_len(0), 16);
    }

    #[test]
    fn multiline_buffer() {
        let snap = make_snapshot_with_inlays(
            "aaa\nbbb\nccc",
            vec![
                (Point::new(0, 3), "X".to_string()),
                (Point::new(2, 0), "Y".to_string()),
            ],
        );
        assert_eq!(snap.to_inlay_point(Point::new(0, 3)), InlayPoint::new(0, 4));
        assert_eq!(snap.to_inlay_point(Point::new(1, 2)), InlayPoint::new(1, 2));
        assert_eq!(snap.to_inlay_point(Point::new(2, 0)), InlayPoint::new(2, 1));
        assert_eq!(snap.to_inlay_point(Point::new(2, 3)), InlayPoint::new(2, 4));
    }

    #[test]
    fn inlay_survives_edit() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let snap = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(snap.clone());

        let off = snap.rope().point_to_offset(Point::new(0, 5));
        let anchor = snap.anchor_at(off, Bias::Right);
        map.splice(
            Vec::new(),
            vec![(anchor, ": str".to_string(), InlayKind::Hint)],
        );

        {
            let mut buf = shared.write().unwrap();
            buf.edit(0..0, "XX");
        }

        let snap2 = multi_buffer.snapshot();
        let (inlay_snap, _) = map.sync(snap2, &Patch::empty());
        assert_eq!(
            inlay_snap.to_inlay_point(Point::new(0, 7)),
            InlayPoint::new(0, 12)
        );
    }

    #[test]
    fn chunks_passthrough_no_inlays_round_trips() {
        use super::InlayOffset;

        let snap = make_snapshot("hello\nworld");
        let endpoints = Arc::from(Vec::new());
        let total = snap.buffer.rope().len();
        let collected: String = snap
            .chunks(InlayOffset(0)..InlayOffset(total), endpoints)
            .map(|c| c.text.into_owned())
            .collect();
        assert_eq!(collected, "hello\nworld");
    }

    #[test]
    fn chunks_with_inlay_emits_interleaved_text() {
        use super::InlayOffset;

        let snap =
            make_snapshot_with_inlays("hello world", vec![(Point::new(0, 5), ": str".to_string())]);
        // Total inlay-space length: "hello" + ": str" + " world" = 5 + 5 + 6 = 16
        let total = 5 + 5 + 6;
        let chunks: Vec<_> = snap
            .chunks(InlayOffset(0)..InlayOffset(total), Arc::from(Vec::new()))
            .collect();

        let full_text: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(full_text, "hello: str world");

        // Exactly one chunk must carry the inlay marker with text ": str".
        let inlay_chunks: Vec<_> = chunks.iter().filter(|c| c.is_inlay).collect();
        assert_eq!(inlay_chunks.len(), 1);
        assert_eq!(inlay_chunks[0].text.as_ref(), ": str");
        assert_eq!(inlay_chunks[0].inlay_kind, Some(InlayKind::Hint));
    }

    #[test]
    fn chunks_clamps_to_inlay_range() {
        use super::InlayOffset;

        let snap =
            make_snapshot_with_inlays("abcdefghij", vec![(Point::new(0, 5), "!!".to_string())]);
        // "abcde" (5) + "!!" (2) + "fghij" (5) = 12
        // Ask for inlay offsets [3, 9): expect "de" + "!!" + "fg" = "de!!fg".
        let chunks: Vec<_> = snap
            .chunks(InlayOffset(3)..InlayOffset(9), Arc::from(Vec::new()))
            .collect();
        let text: String = chunks.iter().map(|c| c.text.as_ref()).collect();
        assert_eq!(text, "de!!fg");
    }

    #[test]
    fn incremental_sync_row_patch_spans_inserted_line() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "ab\ncd");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap1 = multi_buffer.snapshot();
        let version1 = snap1.version();
        let (mut map, _) = InlayMap::new(snap1);

        {
            let mut buf = shared.write().unwrap();
            buf.edit(0..0, "\n");
        }

        let snap2 = multi_buffer.snapshot();
        let buffer_edits = snap2.edits_since(version1);
        let (snap, inlay_edits) = map.sync(snap2, &buffer_edits);

        // Inserting a newline at offset 0 turns old row 0 ("ab") into new
        // rows 0 ("") and 1 ("ab"), so the row patch must span new 0..2.
        let rows: Vec<_> = (&inlay_edits)
            .into_iter()
            .map(|e| (e.old.start, e.old.end, e.new.start, e.new.end))
            .collect();
        assert_eq!(rows, [(0, 1, 0, 2)]);
        assert_eq!(snap.total_summary().len, 6);
    }

    #[test]
    fn invalid_inlay_anchor_dropped_on_delete() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "abcdef");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());

        let snap1 = multi_buffer.snapshot();
        let version1 = snap1.version();
        let (mut map, _) = InlayMap::new(snap1.clone());

        // Anchor a hint inside "def" (offset 3).
        let anchor = snap1.anchor_at(3, Bias::Right);
        map.splice(
            Vec::new(),
            vec![(anchor, ": hint".to_string(), InlayKind::Hint)],
        );
        let (before, _) = map.sync(snap1, &Patch::empty());
        assert!(
            before.has_inlays(),
            "hint present before its text is deleted"
        );

        // Delete the anchored text; its anchor is now invalid.
        {
            let mut buf = shared.write().unwrap();
            buf.edit(3..6, "");
        }
        let snap2 = multi_buffer.snapshot();
        let edits = snap2.edits_since(version1);
        let (after, _) = map.sync(snap2, &edits);

        assert!(
            !after.has_inlays(),
            "stale hint dropped once its anchor is invalidated"
        );
    }
}
