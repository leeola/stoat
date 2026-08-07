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
    pub position: Point,
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
    /// Buffer offsets where [`Self::splice`] added or dropped an inlay since
    /// the last sync.
    ///
    /// These are what let the next sync rebuild the rows a hint batch touched
    /// instead of the file. Left empty when a splice could not keep the set
    /// ordered, which is what routes that sync back to the full rebuild.
    spliced_offsets: Vec<usize>,
}

pub struct InlaySnapshot {
    buffer: MultiBufferSnapshot,
    transforms: SumTree<Transform>,
    inlay_count: usize,
    /// Bumped every time a snapshot is rebuilt, including for a buffer edit that
    /// added or removed no inlay. A consumer asking "is this the same snapshot"
    /// wants this one.
    pub inlay_version: usize,
    /// Bumped only when an inlay is added or removed. A consumer asking whether
    /// the buffer-to-inlay mapping still places a given buffer position the same
    /// way wants this one, since a rebuild alone does not move it.
    pub inlay_set_version: usize,
}

impl Deref for InlaySnapshot {
    type Target = MultiBufferSnapshot;
    fn deref(&self) -> &MultiBufferSnapshot {
        &self.buffer
    }
}

impl InlayMap {
    pub fn new(buffer_snapshot: MultiBufferSnapshot) -> (Self, Arc<InlaySnapshot>) {
        let transforms = build_transforms(buffer_snapshot.rope(), &[]);
        let snapshot = Arc::new(InlaySnapshot {
            buffer: buffer_snapshot,
            transforms,
            inlay_count: 0,
            inlay_version: 0,
            inlay_set_version: 0,
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
            spliced_offsets: Vec::new(),
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
            && let Some(ref cached) = self.cached_snapshot
        {
            return (Arc::clone(cached), Patch::empty());
        }

        let inlay_count = self.inlays.len();
        let inlays_changed = self.version != self.last_self_version;

        // A splice resolved its offsets against the buffer as it stood then, so
        // they only describe this one while the text has not moved. Only the
        // version says that. An empty edit patch does not, since a caller can
        // hand over a newer buffer without one.
        let spliced = std::mem::take(&mut self.spliced_offsets);
        let text_still_matches = buffer_snapshot.version() == self.last_buffer_version;
        let splice_edits = (!spliced.is_empty() && text_still_matches)
            .then(|| splice_patch(spliced, buffer_snapshot.rope().len()));
        let edits_in = splice_edits.as_ref().unwrap_or(buffer_edits);

        let can_incremental = !edits_in.is_empty()
            && (!inlays_changed || splice_edits.is_some())
            && self.cached_snapshot.is_some()
            && self.inlays_sorted
            && self.cached_offsets.len() == self.inlays.len();

        let (resolved, inlay_offsets) = if can_incremental {
            self.resolve_incremental(&buffer_snapshot, edits_in)
        } else {
            self.resolve_all(&buffer_snapshot)
        };

        let (transforms, edits) = if can_incremental {
            let old_snapshot = self
                .cached_snapshot
                .as_ref()
                .expect("guarded by can_incremental");
            sync_incremental(
                old_snapshot,
                &buffer_snapshot,
                edits_in,
                &resolved,
                &inlay_offsets,
            )
        } else {
            let old_line_count = self
                .cached_snapshot
                .as_ref()
                .map(|s| s.line_count())
                .unwrap_or(0);
            let transforms = build_transforms(buffer_snapshot.rope(), &resolved);
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
            inlay_set_version: self.version,
        });
        self.last_buffer_version = snapshot.buffer.version();
        self.last_self_version = self.version;
        self.cached_snapshot = Some(Arc::clone(&snapshot));
        (snapshot, edits)
    }

    fn resolve_all(&mut self, buffer_snapshot: &MultiBufferSnapshot) -> (Vec<Inlay>, Vec<usize>) {
        let anchors: Vec<Anchor> = self.inlays.iter().map(|ai| ai.position).collect();
        let text_len = buffer_snapshot.rope().len();
        let offsets: Vec<usize> = buffer_snapshot
            .resolve_anchors_batch(&anchors)
            .into_iter()
            .map(|offset| offset.min(text_len))
            .collect();
        let points = buffer_snapshot.rope().offsets_to_points_batch(&offsets);

        // Paired with their offsets so the sort below can order by offset and
        // the offsets ride along, rather than converting each sorted point back.
        let mut resolved: Vec<(usize, Inlay)> = self
            .inlays
            .iter()
            .zip(offsets)
            .zip(points)
            .map(|((ai, offset), position)| {
                (
                    offset,
                    Inlay {
                        id: ai.id,
                        position,
                        text: Arc::clone(&ai.text),
                        kind: ai.kind,
                    },
                )
            })
            .collect();

        if !self.inlays_sorted {
            // Points are monotonic in offset, so this is the (row, column)
            // order, and the sort is stable either way for inlays sharing one
            // offset.
            resolved.sort_by_key(|(offset, _)| *offset);
            let id_to_pos: HashMap<usize, usize> = resolved
                .iter()
                .enumerate()
                .map(|(i, (_, r))| (r.id.0, i))
                .collect();
            self.inlays
                .sort_by_key(|ai| id_to_pos.get(&ai.id.0).copied().unwrap_or(usize::MAX));
            self.inlays_sorted = true;
        }

        resolved.into_iter().map(|(offset, i)| (i, offset)).unzip()
    }

    /// Only re-resolve anchors for inlays within edit ranges; adjust the rest
    /// by delta.
    fn resolve_incremental(
        &mut self,
        buffer_snapshot: &MultiBufferSnapshot,
        buffer_edits: &Patch<usize>,
    ) -> (Vec<Inlay>, Vec<usize>) {
        let mut offsets = self.cached_offsets.clone();
        let text_len = buffer_snapshot.rope().len();
        let mut needs_resolve: Vec<bool> = vec![false; offsets.len()];

        shift_offsets(&mut offsets, &mut needs_resolve, buffer_edits);

        let affected: Vec<(usize, Anchor)> = needs_resolve
            .iter()
            .enumerate()
            .filter(|&(_, &needs)| needs)
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
        let points = buffer_snapshot
            .rope()
            .offsets_to_points_batch(&inlay_offsets);
        let resolved: Vec<Inlay> = self
            .inlays
            .iter()
            .zip(points)
            .map(|(ai, position)| Inlay {
                id: ai.id,
                position,
                text: Arc::clone(&ai.text),
                kind: ai.kind,
            })
            .collect();

        (resolved, inlay_offsets)
    }

    pub fn version_unchanged(&self) -> bool {
        self.version == self.last_self_version
    }

    /// Remove the inlays named by `remove`, add `insert`, and return the ids of
    /// what was added.
    ///
    /// `buffer_snapshot` is what the inserted anchors resolve against, which is
    /// how the set stays ordered by offset. Leaving it unordered would cost the
    /// next sync a full re-resolve of every anchor.
    pub fn splice(
        &mut self,
        buffer_snapshot: &MultiBufferSnapshot,
        remove: Vec<InlayId>,
        insert: Vec<(Anchor, String, InlayKind)>,
    ) -> Vec<InlayId> {
        // The offsets are only in step with the set once a sync has resolved
        // them, so before then there is nothing to place against.
        let offsets_usable = self.inlays_sorted && self.cached_offsets.len() == self.inlays.len();

        if !remove.is_empty() {
            let remove_set: HashSet<InlayId> = remove.into_iter().collect();
            if offsets_usable {
                let mut kept = 0;
                for i in 0..self.inlays.len() {
                    if remove_set.contains(&self.inlays[i].id) {
                        self.spliced_offsets.push(self.cached_offsets[i]);
                        continue;
                    }
                    self.inlays.swap(kept, i);
                    self.cached_offsets.swap(kept, i);
                    kept += 1;
                }
                self.inlays.truncate(kept);
                self.cached_offsets.truncate(kept);
            } else {
                self.inlays.retain(|inlay| !remove_set.contains(&inlay.id));
            }
        }

        let mut new_ids = Vec::with_capacity(insert.len());
        if insert.is_empty() {
            self.version += 1;
            return new_ids;
        }

        let text_len = buffer_snapshot.rope().len();
        let offsets: Vec<usize> = {
            let anchors: Vec<Anchor> = insert.iter().map(|(anchor, _, _)| *anchor).collect();
            buffer_snapshot
                .resolve_anchors_batch(&anchors)
                .into_iter()
                .map(|offset| offset.min(text_len))
                .collect()
        };

        for ((position, text, kind), offset) in insert.into_iter().zip(offsets) {
            let id = InlayId(self.next_id);
            self.next_id += 1;
            let inlay = AnchoredInlay {
                id,
                position,
                text: Arc::from(text),
                kind,
            };

            if offsets_usable {
                // After any inlay already at this offset, so a batch arriving
                // together keeps the order it was handed over in.
                let at = self.cached_offsets.partition_point(|&o| o <= offset);
                self.inlays.insert(at, inlay);
                self.cached_offsets.insert(at, offset);
                self.spliced_offsets.push(offset);
            } else {
                self.inlays.push(inlay);
            }
            new_ids.push(id);
        }

        if !offsets_usable {
            // The set is being rebuilt wholesale, so naming rows would only
            // describe part of what changed.
            self.spliced_offsets.clear();
        }
        self.inlays_sorted = offsets_usable;
        self.version += 1;
        new_ids
    }
}

/// The offsets a splice touched, as a patch marking each for rebuild.
///
/// An inlay stands for no buffer text, so nothing here replaces anything and
/// every range maps to itself. They span a byte rather than nothing only
/// because [`Patch::push`] drops an empty edit, which would leave the sync
/// nothing to act on.
fn splice_patch(mut offsets: Vec<usize>, text_len: usize) -> Patch<usize> {
    offsets.sort_unstable();
    offsets.dedup();

    let mut patch = Patch::empty();
    for offset in offsets {
        let start = offset.min(text_len);
        let end = (offset + 1).min(text_len);
        if start < end {
            patch.push(stoat_text::patch::Edit {
                old: start..end,
                new: start..end,
            });
        }
    }
    patch
}

/// Carry each offset in `offsets` across `edits`, flagging in `needs_resolve`
/// the ones that land inside an edit's replaced range.
///
/// One forward pass over both ascending sequences, rather than re-shifting the
/// whole trailing slice once per edit. An offset ends up carrying the summed
/// delta of every edit it sits at or after, including one that lands inside an
/// edit. Those are additionally flagged, and the caller re-resolves them from
/// their anchors, so their carried value is overwritten rather than used.
///
/// `offsets` must be ascending, which is the order the inlay set is kept in.
fn shift_offsets(offsets: &mut [usize], needs_resolve: &mut [bool], edits: &Patch<usize>) {
    let mut delta: isize = 0;
    let mut i = 0;
    for edit in edits {
        while i < offsets.len() && offsets[i] < edit.old.start {
            offsets[i] = ((offsets[i] as isize) + delta).max(0) as usize;
            i += 1;
        }
        while i < offsets.len() && offsets[i] < edit.old.end {
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

fn build_transforms(rope: &Rope, inlays: &[Inlay]) -> SumTree<Transform> {
    let mut transforms = SumTree::new(());

    if inlays.is_empty() {
        if !rope.is_empty() {
            transforms.push(Transform::Isomorphic(rope.summary().clone()), ());
        }
        return transforms;
    }

    let mut cursor = 0usize;

    for inlay in inlays {
        let offset = rope.point_to_offset(inlay.position).min(rope.len());

        if offset > cursor {
            transforms.push(
                Transform::Isomorphic(rope.text_summary_for_range(cursor..offset)),
                (),
            );
        }
        transforms.push(Transform::Inlay(inlay.clone()), ());
        cursor = offset;
    }

    if cursor < rope.len() {
        transforms.push(
            Transform::Isomorphic(rope.text_summary_for_range(cursor..rope.len())),
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

/// Byte index in `text` where its `line`-th row begins, counting from zero.
///
/// Answers `text.len()` when `text` holds fewer rows than that, which keeps a
/// caller measuring the last row from running past the end.
fn line_start_byte(text: &str, line: u32) -> usize {
    if line == 0 {
        return 0;
    }

    let mut seen = 0;
    for (i, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == line {
                return i + 1;
            }
        }
    }
    text.len()
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

    let mut new_transforms = SumTree::new(());
    let mut cursor = old_snapshot.transforms.cursor::<InputOffset>(());
    let mut row_edits = Patch::empty();
    let mut inlay_ix = 0;

    let mut edits_iter = buffer_edits.into_iter().peekable();
    while let Some(edit) = edits_iter.next() {
        // Preserve unchanged prefix
        new_transforms.append(cursor.slice(&InputOffset(edit.old.start), Bias::Left), ());

        // If cursor item ends exactly at edit start, merge it with prefix
        if let Some(Transform::Isomorphic(summary)) = cursor.item()
            && cursor.start().0 + summary.len == edit.old.start
        {
            push_isomorphic(&mut new_transforms, summary.clone());
            cursor.next();
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

        // The row holding the last byte the edit wrote, plus one, matching how
        // the old range was taken above. Reading it as the row *reached*
        // instead loses a row whenever the edit ends on a row boundary, which
        // is what inserting or replacing a whole line does, and the patch then
        // reports a shift of zero for an edit that moved every row below it.
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

    if new_transforms.is_empty() && !new_rope.is_empty() {
        new_transforms.push(Transform::Isomorphic(new_rope.summary().clone()), ());
    }

    (new_transforms, row_edits)
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

    /// Move `point` to the nearest position a caret can occupy, preferring the
    /// side `bias` names.
    ///
    /// The two positions bracketing a hint answer to one buffer point, so
    /// resolving through the buffer would fold them together and leave a caret
    /// unable to cross. A point landing inside a hint is sent to whichever edge
    /// the bias asks for instead, the way a fold placeholder is treated. A point
    /// already on an edge is a position in its own right and stays there.
    pub fn clip_point(&self, point: InlayPoint, bias: Bias) -> InlayPoint {
        let (start, end, item) =
            self.transforms
                .find::<Dimensions<Point, InlayPoint>, _>((), &point, Bias::Right);

        if let Some(Transform::Inlay(_)) = item {
            return if bias == Bias::Left || point == start.1 {
                start.1
            } else {
                end.1
            };
        }

        let buf = {
            let overshoot = point_overshoot(start.1 .0, point.0);
            start.0 + overshoot
        };
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

    /// Byte length of inlay row `row` as it paints, excluding its newline.
    ///
    /// `row` is an inlay row, not a buffer row. The two agree until a hint
    /// carrying a newline splits the row it sits on, and then only the inlay
    /// row names what a caller bounding a rendered row is asking about.
    pub fn line_len(&self, row: u32) -> u32 {
        if !self.has_inlays() {
            return self.buffer.rope().line_len(row);
        }

        let total = &self.transforms.summary().output;
        if row > total.lines.row {
            return 0;
        }

        let start = self.row_start_offset(row);
        // The next row starts one past this row's newline, and the last row
        // runs to the end of the text.
        let end = if row == total.lines.row {
            total.len
        } else {
            self.row_start_offset(row + 1).saturating_sub(1)
        };
        (end - start) as u32
    }

    /// Output offset where inlay row `row` begins.
    ///
    /// Unlike [`Self::inlay_point_to_offset`], which treats a hint as one
    /// indivisible position, this reaches inside a hint's own text. A hint
    /// carrying a newline starts a row there, and that row's beginning is a
    /// real place in the painted output even though it is no place in the
    /// buffer.
    fn row_start_offset(&self, row: u32) -> usize {
        let target = InlayPoint::new(row, 0);
        let (start, _end, item) = self
            .transforms
            .find::<Dimensions<OutputOffset, Point, InlayPoint>, _>((), &target, Bias::Right);

        match item {
            Some(Transform::Inlay(inlay)) => {
                start.0 .0 + line_start_byte(&inlay.text, row - start.2.row())
            },
            _ => {
                let overshoot = point_overshoot(start.2 .0, target.0);
                let rope = self.buffer.rope();
                start.0 .0
                    + (rope.point_to_offset(start.1 + overshoot) - rope.point_to_offset(start.1))
            },
        }
    }

    pub fn has_inlays(&self) -> bool {
        self.inlay_count > 0
    }

    /// Whether any inlay sits on a buffer row within `rows`.
    ///
    /// Lets a per-row paint fast path stay fast on rows with no inlay even when
    /// the buffer carries inlays elsewhere. Seeks the inlay transforms to
    /// `rows.start` rather than scanning them all.
    pub fn has_inlays_in_row_range(&self, rows: Range<u32>) -> bool {
        if !self.has_inlays() {
            return false;
        }
        let mut cursor = self.transforms.cursor::<Point>(());
        cursor.seek(&Point::new(rows.start, 0), Bias::Left);
        while let Some(transform) = cursor.item() {
            let pos: Point = *cursor.start();
            if pos.row >= rows.end {
                break;
            }
            if let Transform::Inlay(inlay) = transform
                && rows.contains(&inlay.position.row)
            {
                return true;
            }
            cursor.next();
        }
        false
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
    use super::{InlayMap, InlayPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::{patch::Patch, Bias, Point};

    fn make_snapshot(content: &str) -> Arc<super::InlaySnapshot> {
        let buffer = TextBuffer::with_text(BufferId::new(0), content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, snapshot) = InlayMap::new(buffer_snapshot);
        snapshot
    }

    /// The whole snapshot as the chunk stream paints it, which is what these
    /// tests are comparing against. Nothing builds this outside tests, since a
    /// real consumer wants the span it is about to render.
    fn painted_text(snapshot: &super::InlaySnapshot) -> String {
        snapshot
            .chunks(
                super::InlayOffset(0)..super::InlayOffset(snapshot.total_summary().len),
                Arc::from(Vec::new()),
            )
            .map(|chunk| chunk.text.to_string())
            .collect()
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
                    super::InlayKind::Hint,
                )
            })
            .collect();
        map.splice(&buffer_snapshot, Vec::new(), anchored_inlays);
        let (snapshot, _) = map.sync(buffer_snapshot, &Patch::empty());
        snapshot
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
    fn splice_add_and_remove() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(buffer_snapshot.clone());

        let off = buffer_snapshot.rope().point_to_offset(Point::new(0, 5));
        let anchor = buffer_snapshot.anchor_at(off, Bias::Right);
        let ids = map.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![(anchor, ": str".to_string(), super::InlayKind::Hint)],
        );
        let (snap, _) = map.sync(buffer_snapshot.clone(), &Patch::empty());
        assert_eq!(
            snap.to_inlay_point(Point::new(0, 5)),
            InlayPoint::new(0, 10)
        );

        map.splice(&buffer_snapshot, ids, Vec::new());
        let (snap, _) = map.sync(buffer_snapshot, &Patch::empty());
        assert_eq!(snap.to_inlay_point(Point::new(0, 5)), InlayPoint::new(0, 5));
    }

    /// Splicing places new inlays rather than appending them, so the set stays
    /// ordered by offset with its cached offsets in step. A sync reading either
    /// one out of order would carry the wrong anchor across an edit.
    #[test]
    fn splicing_keeps_the_inlay_set_ordered() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "0123456789\nabcdefghij");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(buffer_snapshot.clone());

        let hint = |column: u32, text: &str| {
            let off = buffer_snapshot
                .rope()
                .point_to_offset(Point::new(0, column));
            (
                buffer_snapshot.anchor_at(off, Bias::Right),
                text.to_string(),
                super::InlayKind::Hint,
            )
        };

        // Deliberately out of order, and a sync to resolve them.
        let ids = map.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![hint(8, "h"), hint(2, "b"), hint(5, "e")],
        );
        map.sync(buffer_snapshot.clone(), &Patch::empty());

        // Now placed rather than appended, since the offsets are resolved.
        map.splice(
            &buffer_snapshot,
            vec![ids[2]],
            vec![hint(1, "a"), hint(9, "i")],
        );

        assert_eq!(
            map.cached_offsets,
            vec![1, 2, 8, 9],
            "offsets stay ascending across a splice",
        );
        assert_eq!(
            map.inlays
                .iter()
                .map(|inlay| inlay.text.to_string())
                .collect::<Vec<_>>(),
            vec!["a", "b", "h", "i"],
            "the set follows its offsets, and the removed hint is gone",
        );
        assert!(map.inlays_sorted);
    }

    /// Hints arrive in batches while typing, and a patch spanning the file
    /// makes every layer below rebuild for a few rows of change. What the
    /// splice actually touched is the row the hint landed on.
    #[test]
    fn splicing_one_hint_emits_a_patch_covering_only_its_row() {
        let text: String = (0..200).map(|i| format!("line{i}\n")).collect();
        let buffer = TextBuffer::with_text(BufferId::new(0), &text);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(buffer_snapshot.clone());
        map.sync(buffer_snapshot.clone(), &Patch::empty());

        let off = buffer_snapshot.rope().point_to_offset(Point::new(100, 0));
        let anchor = buffer_snapshot.anchor_at(off, Bias::Right);
        map.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![(anchor, ": str".to_string(), super::InlayKind::Hint)],
        );

        let (snapshot, edits) = map.sync(buffer_snapshot, &Patch::empty());
        assert_eq!(
            edits
                .edits()
                .iter()
                .map(|edit| (edit.old.clone(), edit.new.clone()))
                .collect::<Vec<_>>(),
            vec![(100..101, 100..101)],
            "only the hint's row changed",
        );
        assert_eq!(
            snapshot.line_len(100),
            "line100".len() as u32 + ": str".len() as u32,
        );
    }

    /// A splice now patches the rows it touched rather than rebuilding, so the
    /// patched text has to be held against what a rebuild produces. A dropped
    /// hint and one left beside its replacement both read as ordinary text.
    #[test]
    fn a_spliced_sync_agrees_with_a_full_rebuild() {
        let buffer = TextBuffer::with_text(BufferId::new(0), "alpha beta\ngamma delta\nepsilon");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();

        let hint = |row: u32, column: u32, text: &str| {
            let off = buffer_snapshot
                .rope()
                .point_to_offset(Point::new(row, column));
            (
                buffer_snapshot.anchor_at(off, Bias::Right),
                text.to_string(),
                super::InlayKind::Hint,
            )
        };

        let mut patched = InlayMap::new(buffer_snapshot.clone()).0;
        patched.sync(buffer_snapshot.clone(), &Patch::empty());
        let ids = patched.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![hint(0, 5, ": a"), hint(1, 5, ": b"), hint(2, 3, ": c")],
        );
        patched.sync(buffer_snapshot.clone(), &Patch::empty());
        patched.splice(
            &buffer_snapshot,
            vec![ids[1]],
            vec![hint(0, 10, ": d"), hint(1, 0, ": e")],
        );
        let (patched_snapshot, _) = patched.sync(buffer_snapshot.clone(), &Patch::empty());

        // The same set, built with nothing to patch against.
        let mut rebuilt = InlayMap::new(buffer_snapshot.clone()).0;
        rebuilt.splice(
            &buffer_snapshot,
            Vec::new(),
            vec![
                hint(0, 5, ": a"),
                hint(2, 3, ": c"),
                hint(0, 10, ": d"),
                hint(1, 0, ": e"),
            ],
        );
        let (rebuilt_snapshot, _) = rebuilt.sync(buffer_snapshot, &Patch::empty());

        assert_eq!(
            painted_text(&patched_snapshot),
            painted_text(&rebuilt_snapshot),
        );
    }

    #[test]
    fn line_len_no_inlays() {
        let snap = make_snapshot("hello\nworld");
        assert_eq!(snap.line_len(0), 5);
        assert_eq!(snap.line_len(1), 5);
    }

    /// A hint sits between two buffer positions that are really one, so the
    /// only thing telling the two sides apart is the bias. Clipping both to the
    /// same place leaves a caret unable to cross a hint.
    #[test]
    fn clipping_a_point_inside_a_hint_follows_the_bias() {
        // "hello: str world", with the hint filling columns 5 through 10.
        let snap =
            make_snapshot_with_inlays("hello world", vec![(Point::new(0, 5), ": str".to_string())]);

        assert_eq!(
            snap.clip_point(InlayPoint::new(0, 7), Bias::Left),
            InlayPoint::new(0, 5),
            "leftward, to the position before the hint",
        );
        assert_eq!(
            snap.clip_point(InlayPoint::new(0, 7), Bias::Right),
            InlayPoint::new(0, 10),
            "rightward, to the position after it",
        );

        for bias in [Bias::Left, Bias::Right] {
            assert_eq!(
                snap.clip_point(InlayPoint::new(0, 5), bias),
                InlayPoint::new(0, 5),
                "the position before the hint is already a real one, {bias:?}",
            );
            assert_eq!(
                snap.clip_point(InlayPoint::new(0, 10), bias),
                InlayPoint::new(0, 10),
                "so is the position after it, {bias:?}",
            );
        }
    }

    /// Clipping also holds a point inside the buffer, which the bias handling
    /// must not cost.
    #[test]
    fn clipping_still_clamps_past_the_end() {
        let snap =
            make_snapshot_with_inlays("hello world", vec![(Point::new(0, 5), ": str".to_string())]);

        assert_eq!(
            snap.clip_point(InlayPoint::new(0, 400), Bias::Left),
            InlayPoint::new(0, 16),
            "a column past the end lands on it",
        );
        assert_eq!(
            snap.clip_point(InlayPoint::new(9, 0), Bias::Left),
            InlayPoint::new(0, 0),
            "so does a row past the end",
        );
    }

    /// Every row a hint produces has to measure what it paints, which is what a
    /// caller bounding a rendered row relies on. Reading the buffer's row length
    /// and adding the hint's bytes cannot say that once the hint carries a
    /// newline, since the row it sat on has become two.
    #[test]
    fn line_len_matches_the_painted_row() {
        let cases = [
            (": str", vec![16]),
            (" → u32", vec![19]),
            ("a\nb", vec![6, 7]),
            ("one\ntwo\n", vec![8, 3, 6]),
        ];

        for (hint, want) in cases {
            let snap = make_snapshot_with_inlays(
                "hello world",
                vec![(Point::new(0, 5), hint.to_string())],
            );

            let painted: Vec<u32> = painted_text(&snap)
                .split('\n')
                .map(|row| row.len() as u32)
                .collect();
            assert_eq!(painted, want, "the fixture for {hint:?} paints these rows");

            let measured: Vec<u32> = (0..painted.len() as u32)
                .map(|row| snap.line_len(row))
                .collect();
            assert_eq!(measured, want, "measuring the rows a {hint:?} hint leaves");
        }
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
            &snap,
            Vec::new(),
            vec![(anchor, ": str".to_string(), super::InlayKind::Hint)],
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
        assert_eq!(inlay_chunks[0].inlay_kind, Some(super::InlayKind::Hint));
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

    /// `resolve_all` sorts by offset now rather than by (row, column). Both are
    /// stable sorts, so two inlays anchored at one offset have to keep the order
    /// they were spliced in.
    #[test]
    fn inlays_sharing_an_offset_keep_insertion_order() {
        let snap = make_snapshot_with_inlays(
            "hello\nworld",
            vec![
                (Point::new(0, 2), "<first>".to_string()),
                (Point::new(0, 2), "<second>".to_string()),
            ],
        );

        let total = snap.buffer.rope().len() + "<first><second>".len();
        let text: String = snap
            .chunks(
                super::InlayOffset(0)..super::InlayOffset(total),
                Arc::from(Vec::new()),
            )
            .map(|c| c.text.into_owned())
            .collect();

        assert_eq!(
            text, "he<first><second>llo\nworld",
            "the tied inlays render in splice order, not reversed or interleaved",
        );
    }

    fn edit(
        old: std::ops::Range<usize>,
        new: std::ops::Range<usize>,
    ) -> stoat_text::patch::Edit<usize> {
        stoat_text::patch::Edit { old, new }
    }

    /// One offset at a time, straight from the coordinate contract, as the
    /// oracle the forward walk's results are compared against.
    ///
    /// An edit's new range is stated in post-all-edits coordinates, so the
    /// difference between its ends is where everything after it has landed by
    /// then. An offset therefore takes that difference from the last edit it
    /// sits past, and nothing from the ones before, which is what makes this an
    /// answer rather than a second copy of the walk.
    fn shift_offsets_per_edit(
        offsets: &mut [usize],
        needs_resolve: &mut [bool],
        edits: &Patch<usize>,
    ) {
        for (i, offset) in offsets.iter_mut().enumerate() {
            let mut delta = 0isize;
            for edit in edits {
                if *offset >= edit.old.end {
                    delta = (edit.new.end as isize) - (edit.old.end as isize);
                } else if *offset >= edit.old.start {
                    needs_resolve[i] = true;
                }
            }
            *offset = ((*offset as isize) + delta).max(0) as usize;
        }
    }

    /// Shifted offsets paired with the re-resolve flags, one walk's whole
    /// output.
    type Shifted = (Vec<usize>, Vec<bool>);

    fn both_shifts(offsets: &[usize], edits: &Patch<usize>) -> (Shifted, Shifted) {
        let mut a = (offsets.to_vec(), vec![false; offsets.len()]);
        super::shift_offsets(&mut a.0, &mut a.1, edits);
        let mut b = (offsets.to_vec(), vec![false; offsets.len()]);
        shift_offsets_per_edit(&mut b.0, &mut b.1, edits);
        (a, b)
    }

    #[test]
    fn shifting_offsets_lands_hand_computed_values() {
        // 5..8 shrinks to 2 bytes, so everything past it stands at -1. The
        // insert's new range, 19..24, is already stated with that -1 applied,
        // so its own ends give +4 as the total and not as a further step.
        let edits = Patch::new(vec![edit(5..8, 5..7), edit(20..20, 19..24)]);
        let offsets = [0, 4, 5, 7, 8, 12, 20, 30];
        let (mine, _) = both_shifts(&offsets, &edits);

        assert_eq!(
            mine.0,
            vec![0, 4, 5, 7, 7, 11, 24, 34],
            "before the first edit unshifted, after it -1, and past the insert +4",
        );
        assert_eq!(
            mine.1,
            vec![false, false, true, true, false, false, false, false],
            "only the offsets inside 5..8 are flagged for re-resolution",
        );
    }

    #[test]
    fn shifting_offsets_matches_the_per_edit_walk() {
        let cases = [
            (
                Patch::new(vec![
                    edit(5..8, 5..7),
                    edit(20..20, 19..24),
                    edit(40..50, 44..44),
                ]),
                vec![0, 3, 5, 6, 8, 19, 20, 21, 39, 40, 45, 50, 80],
            ),
            (Patch::new(vec![edit(0..10, 0..0)]), vec![0, 5, 10, 11, 99]),
            (Patch::new(vec![edit(0..0, 0..4)]), vec![0, 1, 2]),
            (Patch::new(Vec::new()), vec![0, 7, 13]),
        ];

        for (edits, offsets) in cases {
            let (mine, oracle) = both_shifts(&offsets, &edits);
            assert_eq!(mine, oracle, "offsets {offsets:?}");
        }
    }

    /// The row patch is the only thing telling the fold layer how far an edit
    /// moved the rows below it, and an edit landing on a row boundary is where
    /// that is easiest to get wrong. Under-reporting the new side by a row
    /// makes a whole-line insert look like an in-place change, and the fold
    /// layer then consumes the displaced text without emitting it again.
    #[test]
    fn row_patch_reports_the_rows_an_edit_shifts() {
        let cases = [
            (
                "fn a() {}\n",
                0..0,
                "//1\n",
                0..1,
                0..2,
                "a line inserted above",
            ),
            (
                "abc\ndef\n",
                0..4,
                "xyz\n",
                0..2,
                0..2,
                "a line replaced in place",
            ),
            ("abc\ndef\n", 0..4, "", 0..2, 0..1, "a line deleted"),
            (
                "abc\ndef\n",
                1..1,
                "X",
                0..1,
                0..1,
                "an insert inside one row",
            ),
        ];

        for (text, range, insert, want_old, want_new, what) in cases {
            let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), text)));
            let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
            let (mut map, _) = InlayMap::new(multi_buffer.snapshot());

            let before = multi_buffer.snapshot();
            shared.write().expect("poisoned").edit(range, insert);
            let after = multi_buffer.snapshot();

            let (_, rows) = map.sync(after.clone(), &after.edits_since(before.version()));
            assert_eq!(
                rows.edits()
                    .iter()
                    .map(|e| (e.old.clone(), e.new.clone()))
                    .collect::<Vec<_>>(),
                vec![(want_old, want_new)],
                "{what}",
            );
        }
    }
}
