use crate::multi_buffer::MultiBufferSnapshot;
use std::{
    cmp::Ordering,
    collections::HashSet,
    ops::Deref,
    sync::{Arc, OnceLock},
};
use stoat_text::{
    patch::Patch, Anchor, Bias, ContextLessSummary, Dimension, Dimensions, Item, Point, Rope,
    SeekTarget, SumTree, TextSummary,
};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct InlayId(usize);

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
}

pub struct InlayMap {
    inlays: Vec<AnchoredInlay>,
    next_id: usize,
    version: usize,
    snapshot_version: usize,
    cached_snapshot: Option<Arc<InlaySnapshot>>,
    last_buffer_version: usize,
    last_self_version: usize,
}

pub struct InlaySnapshot {
    buffer: MultiBufferSnapshot,
    transforms: SumTree<Transform>,
    inlay_count: usize,
    inlay_text_cache: OnceLock<String>,
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
        let transforms = build_transforms(&buffer_snapshot.rope, buffer_snapshot.text(), &[]);
        let snapshot = Arc::new(InlaySnapshot {
            buffer: buffer_snapshot,
            transforms,
            inlay_count: 0,
            inlay_text_cache: OnceLock::new(),
            inlay_version: 0,
        });
        let map = InlayMap {
            inlays: Vec::new(),
            next_id: 0,
            version: 0,
            snapshot_version: 0,
            cached_snapshot: Some(Arc::clone(&snapshot)),
            last_buffer_version: snapshot.buffer.version,
            last_self_version: 0,
        };
        (map, snapshot)
    }

    pub fn sync(
        &mut self,
        buffer_snapshot: MultiBufferSnapshot,
        buffer_edits: &Patch<usize>,
    ) -> (Arc<InlaySnapshot>, Patch<u32>) {
        if buffer_snapshot.version == self.last_buffer_version
            && self.version == self.last_self_version
        {
            if let Some(ref cached) = self.cached_snapshot {
                return (Arc::clone(cached), Patch::empty());
            }
        }

        let inlay_count = self.inlays.len();
        let anchors: Vec<Anchor> = self.inlays.iter().map(|ai| ai.position).collect();
        let offsets = buffer_snapshot.resolve_anchors_batch(&anchors);
        let mut resolved: Vec<Inlay> = self
            .inlays
            .iter()
            .zip(offsets)
            .map(|(ai, offset)| Inlay {
                id: ai.id,
                position: buffer_snapshot.rope.offset_to_point(offset),
                text: Arc::clone(&ai.text),
            })
            .collect();
        resolved.sort_by_key(|i| (i.position.row, i.position.column));

        let can_incremental = !buffer_edits.is_empty()
            && self.version == self.last_self_version
            && self.cached_snapshot.is_some();

        let (transforms, edits) = if can_incremental {
            let old_snapshot = self.cached_snapshot.as_ref().unwrap();
            let inlay_offsets: Vec<usize> = resolved
                .iter()
                .map(|i| {
                    buffer_snapshot
                        .rope
                        .point_to_offset(i.position)
                        .min(buffer_snapshot.text().len())
                })
                .collect();
            sync_incremental(
                old_snapshot,
                &buffer_snapshot,
                buffer_edits,
                &resolved,
                &inlay_offsets,
            )
        } else {
            let old_line_count = self
                .cached_snapshot
                .as_ref()
                .map(|s| s.line_count())
                .unwrap_or(0);
            let transforms =
                build_transforms(&buffer_snapshot.rope, buffer_snapshot.text(), &resolved);
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

        self.snapshot_version += 1;
        let snapshot = Arc::new(InlaySnapshot {
            buffer: buffer_snapshot,
            transforms,
            inlay_count,
            inlay_text_cache: OnceLock::new(),
            inlay_version: self.snapshot_version,
        });
        self.last_buffer_version = snapshot.buffer.version;
        self.last_self_version = self.version;
        self.cached_snapshot = Some(Arc::clone(&snapshot));
        (snapshot, edits)
    }

    pub fn min_anchor_version(&self) -> usize {
        self.inlays
            .iter()
            .map(|i| i.position.version)
            .min()
            .unwrap_or(self.last_buffer_version)
    }

    pub fn splice(&mut self, remove: Vec<InlayId>, insert: Vec<(Anchor, String)>) -> Vec<InlayId> {
        if !remove.is_empty() {
            let remove_set: HashSet<InlayId> = remove.into_iter().collect();
            self.inlays.retain(|inlay| !remove_set.contains(&inlay.id));
        }

        let mut new_ids = Vec::with_capacity(insert.len());
        for (position, text) in insert {
            let id = InlayId(self.next_id);
            self.next_id += 1;
            self.inlays.push(AnchoredInlay {
                id,
                position,
                text: Arc::from(text),
            });
            new_ids.push(id);
        }

        self.version += 1;
        new_ids
    }
}

fn build_transforms(rope: &Rope, text: &str, inlays: &[Inlay]) -> SumTree<Transform> {
    let mut transforms = SumTree::new(());

    if inlays.is_empty() {
        if !text.is_empty() {
            transforms.push(Transform::Isomorphic(rope.summary().clone()), ());
        }
        return transforms;
    }

    let mut cursor = 0usize;

    for inlay in inlays {
        let offset = rope.point_to_offset(inlay.position).min(text.len());

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

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct OutputOffset(pub usize);

impl<'a> Dimension<'a, TransformSummary> for OutputOffset {
    fn zero(_cx: ()) -> Self {
        Self(0)
    }

    fn add_summary(&mut self, s: &'a TransformSummary, _cx: ()) {
        self.0 += s.output.len;
    }
}

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
                ContextLessSummary::add_summary(existing, &summary.take().unwrap());
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
    let old_rope = &old_snapshot.buffer.rope;
    let new_rope = &buffer_snapshot.rope;
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

        let new_out = new_transforms.summary().output.lines;
        let new_end_row = if new_out.column > 0 {
            new_out.row + 1
        } else {
            new_out.row.max(new_start_row + 1)
        };

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
        let line_len = self.buffer.rope.line_len(row);
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

    pub fn line_len(&self, row: u32) -> u32 {
        let len = self.buffer.rope.line_len(row);
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
                if inlay.position.row == row {
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

    pub fn inlay_point_to_offset(&self, point: InlayPoint) -> usize {
        if !self.has_inlays() {
            return self.buffer.rope.point_to_offset(point.0);
        }
        let (start, _end, item) = self
            .transforms
            .find::<Dimensions<OutputOffset, Point, InlayPoint>, _>((), &point, Bias::Right);
        match item {
            Some(Transform::Isomorphic(_)) | None => {
                let overshoot = point_overshoot(start.2 .0, point.0);
                let buffer_point = start.1 + overshoot;
                let buffer_offset = self.buffer.rope.point_to_offset(buffer_point);
                let start_buffer_offset = self.buffer.rope.point_to_offset(start.1);
                start.0 .0 + (buffer_offset - start_buffer_offset)
            },
            Some(Transform::Inlay(_)) => start.0 .0,
        }
    }

    pub fn inlay_offset_at_row(&self, row: u32) -> usize {
        self.inlay_point_to_offset(InlayPoint::new(row, 0))
    }

    pub fn inlay_text(&self) -> &str {
        self.inlay_text_cache.get_or_init(|| {
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
        })
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
    use stoat_text::{patch::Patch, Point};

    fn make_snapshot(content: &str) -> Arc<super::InlaySnapshot> {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
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
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(buffer_snapshot.clone());
        let anchored_inlays = inlays
            .into_iter()
            .map(|(pos, text)| {
                let off = buffer_snapshot.rope.point_to_offset(pos);
                (
                    buffer_snapshot.anchor_at(off, stoat_text::Bias::Right),
                    text,
                )
            })
            .collect();
        map.splice(Vec::new(), anchored_inlays);
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
        let mut buffer = TextBuffer::new();
        buffer.rope.push("hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(buffer_snapshot.clone());

        let off = buffer_snapshot.rope.point_to_offset(Point::new(0, 5));
        let anchor = buffer_snapshot.anchor_at(off, stoat_text::Bias::Right);
        let ids = map.splice(Vec::new(), vec![(anchor, ": str".to_string())]);
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
        let mut buffer = TextBuffer::new();
        buffer.rope.push("hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let snap = multi_buffer.snapshot();
        let (mut map, _) = InlayMap::new(snap.clone());

        let off = snap.rope.point_to_offset(Point::new(0, 5));
        let anchor = snap.anchor_at(off, stoat_text::Bias::Right);
        map.splice(Vec::new(), vec![(anchor, ": str".to_string())]);

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
}
