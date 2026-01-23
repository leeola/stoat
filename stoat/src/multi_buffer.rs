use crate::{
    buffer::{BufferId, SharedBuffer},
    git::BufferDiff,
};
use stoat_text::Point;

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
            line_count: buffer.line_count(),
            text: buffer.rope.to_string(),
            diff: buffer.diff.clone(),
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

pub struct MultiBufferSnapshot {
    singleton: bool,
    line_count: u32,
    text: String,
    pub diff: Option<BufferDiff>,
}

impl MultiBufferSnapshot {
    pub fn line_count(&self) -> u32 {
        self.line_count
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.lines()
    }

    pub fn max_point(&self) -> MultiBufferPoint {
        let row = self.line_count.saturating_sub(1);
        let column = self
            .text
            .lines()
            .nth(row as usize)
            .map(|line| line.len() as u32)
            .unwrap_or(0);
        MultiBufferPoint::new(row, column)
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
}
