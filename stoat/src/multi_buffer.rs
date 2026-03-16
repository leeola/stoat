use crate::{
    buffer::{self, BufferId, EditRecord, SharedBuffer},
    git::BufferDiff,
};
use std::sync::{Arc, OnceLock};
use stoat_text::{Anchor, Bias, Point, Rope};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExcerptId(u64);

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MultiBufferPoint {
    pub row: u32,
    pub column: u32,
}

impl MultiBufferPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self { row, column }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MultiBufferRow(pub u32);

struct Excerpt {
    #[allow(dead_code)]
    id: ExcerptId,
    #[allow(dead_code)]
    buffer_id: BufferId,
    buffer: SharedBuffer,
}

pub struct MultiBuffer {
    excerpts: Vec<Excerpt>,
    #[allow(dead_code)]
    next_excerpt_id: u64,
    singleton: bool,
}

impl MultiBuffer {
    pub fn singleton(buffer_id: BufferId, buffer: SharedBuffer) -> Self {
        let excerpt = Excerpt {
            id: ExcerptId(0),
            buffer_id,
            buffer,
        };
        Self {
            excerpts: vec![excerpt],
            next_excerpt_id: 1,
            singleton: true,
        }
    }

    pub fn is_singleton(&self) -> bool {
        self.singleton
    }

    pub fn snapshot(&self) -> MultiBufferSnapshot {
        let excerpt = &self.excerpts[0];
        let buffer = excerpt.buffer.read().expect("buffer lock poisoned");
        MultiBufferSnapshot {
            singleton: self.singleton,
            rope: buffer.rope.clone(),
            text_cache: OnceLock::new(),
            diff: buffer.diff.clone(),
            version: buffer.version,
            edit_log: buffer.edit_log.clone(),
            compacted_to: buffer.compacted_to(),
        }
    }

    pub fn compact_edit_log(&self, watermark: usize) {
        if let Some(buf) = self.as_singleton() {
            buf.write()
                .expect("buffer lock")
                .compact_edit_log(watermark);
        }
    }

    pub fn as_singleton(&self) -> Option<&SharedBuffer> {
        if self.singleton {
            Some(&self.excerpts[0].buffer)
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct MultiBufferSnapshot {
    singleton: bool,
    pub(crate) rope: Rope,
    text_cache: OnceLock<String>,
    pub diff: Option<BufferDiff>,
    pub version: usize,
    pub(crate) edit_log: Arc<Vec<EditRecord>>,
    compacted_to: usize,
}

impl MultiBufferSnapshot {
    pub fn line_count(&self) -> u32 {
        self.rope.max_point().row + 1
    }

    pub fn text(&self) -> &str {
        self.text_cache.get_or_init(|| self.rope.to_string())
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text().split('\n')
    }

    pub fn line_at_row(&self, row: u32) -> String {
        self.rope.line_at_row(row)
    }

    pub fn max_point(&self) -> MultiBufferPoint {
        let p = self.rope.max_point();
        MultiBufferPoint::new(p.row, p.column)
    }

    pub fn point_to_multi_buffer_point(&self, point: Point) -> MultiBufferPoint {
        MultiBufferPoint {
            row: point.row,
            column: point.column,
        }
    }

    pub fn multi_buffer_point_to_point(&self, point: MultiBufferPoint) -> Point {
        Point::new(point.row, point.column)
    }

    pub fn anchor_at(&self, offset: usize, bias: Bias) -> Anchor {
        Anchor {
            version: self.version,
            offset: offset.min(self.rope.len()),
            bias,
        }
    }

    pub fn resolve_anchor(&self, anchor: &Anchor) -> usize {
        buffer::resolve_anchor_in_log(&self.edit_log, anchor, self.rope.len())
    }

    pub fn point_for_anchor(&self, anchor: &Anchor) -> Point {
        self.rope.offset_to_point(self.resolve_anchor(anchor))
    }

    pub fn resolve_anchors_batch(&self, anchors: &[Anchor]) -> Vec<usize> {
        buffer::resolve_anchors_batch(&self.edit_log, anchors, self.rope.len())
    }

    pub fn points_for_anchors_batch(&self, anchors: &[Anchor]) -> Vec<Point> {
        let offsets = self.resolve_anchors_batch(anchors);
        self.rope.offsets_to_points_batch(&offsets)
    }

    pub fn is_anchor_valid(&self, anchor: &Anchor) -> bool {
        anchor.offset == usize::MAX
            || (anchor.offset == 0 && anchor.bias == Bias::Left)
            || anchor.version >= self.compacted_to
    }

    pub fn cmp_anchors(&self, a: &Anchor, b: &Anchor) -> std::cmp::Ordering {
        a.cmp(b, &|anchor| self.resolve_anchor(anchor))
    }

    pub fn is_singleton(&self) -> bool {
        self.singleton
    }
}

#[cfg(test)]
mod tests {
    use super::{MultiBuffer, MultiBufferPoint};
    use crate::buffer::{BufferId, TextBuffer};
    use std::sync::{Arc, RwLock};
    use stoat_text::Point;

    fn create_test_buffer(content: &str) -> (BufferId, Arc<RwLock<TextBuffer>>) {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        (BufferId::new(0), Arc::new(RwLock::new(buffer)))
    }

    #[test]
    fn singleton_creation() {
        let (id, buffer) = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(id, buffer);
        assert!(multi.is_singleton());
        assert!(multi.as_singleton().is_some());
    }

    #[test]
    fn snapshot_line_count() {
        let (id, buffer) = create_test_buffer("line1\nline2\nline3");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        assert_eq!(snapshot.line_count(), 3);
    }

    #[test]
    fn snapshot_text() {
        let (id, buffer) = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        assert_eq!(snapshot.text(), "hello\nworld");
    }

    #[test]
    fn snapshot_lines() {
        let (id, buffer) = create_test_buffer("a\nb\nc");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let lines: Vec<_> = snapshot.lines().collect();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn snapshot_max_point() {
        let (id, buffer) = create_test_buffer("short\nlonger line\nx");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let max = snapshot.max_point();
        assert_eq!(max.row, 2);
        assert_eq!(max.column, 1);
    }

    #[test]
    fn anchor_valid_within_bounds() {
        let (id, buffer) = create_test_buffer("0123456789");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(5, stoat_text::Bias::Right);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn anchor_max_is_valid() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(5, stoat_text::Bias::Left);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn passthrough_coordinates() {
        let (id, buffer) = create_test_buffer("hello\nworld");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();

        let point = Point::new(1, 3);
        let mb_point = snapshot.point_to_multi_buffer_point(point);
        assert_eq!(mb_point, MultiBufferPoint::new(1, 3));

        let back = snapshot.multi_buffer_point_to_point(mb_point);
        assert_eq!(back, point);
    }

    #[test]
    fn stale_anchor_invalid_after_compaction() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);
        let snap1 = multi.snapshot();
        let anchor = snap1.anchor_at(2, stoat_text::Bias::Right);

        multi
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(0..0, "XX");
        multi.compact_edit_log(anchor.version + 1);

        let snap2 = multi.snapshot();
        assert!(!snap2.is_anchor_valid(&anchor));
    }

    #[test]
    fn fresh_anchor_valid() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let anchor = snapshot.anchor_at(3, stoat_text::Bias::Right);
        assert!(snapshot.is_anchor_valid(&anchor));
    }

    #[test]
    fn anchor_min_max_always_valid() {
        let (id, buffer) = create_test_buffer("hello");
        let multi = MultiBuffer::singleton(id, buffer);

        multi
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(0..0, "XX");
        multi.compact_edit_log(100);

        let snapshot = multi.snapshot();
        assert!(snapshot.is_anchor_valid(&stoat_text::Anchor::min()));
        assert!(snapshot.is_anchor_valid(&stoat_text::Anchor::max()));
    }

    #[test]
    fn cmp_anchors_by_offset() {
        let (id, buffer) = create_test_buffer("hello world");
        let multi = MultiBuffer::singleton(id, buffer);
        let snapshot = multi.snapshot();
        let a = snapshot.anchor_at(3, stoat_text::Bias::Left);
        let b = snapshot.anchor_at(7, stoat_text::Bias::Left);
        assert_eq!(snapshot.cmp_anchors(&a, &b), std::cmp::Ordering::Less);
        assert_eq!(snapshot.cmp_anchors(&b, &a), std::cmp::Ordering::Greater);
        assert_eq!(snapshot.cmp_anchors(&a, &a), std::cmp::Ordering::Equal);
    }
}
