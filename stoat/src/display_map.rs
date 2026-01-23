mod block_map;

use crate::{
    git::{BufferDiff, DiffStatus},
    multi_buffer::MultiBuffer,
};
pub use block_map::{
    Block, BlockContent, BlockMap, BlockPlacement, BlockPoint, BlockRow, BlockRowKind,
    BlockSnapshot,
};
use std::sync::Arc;
use stoat_text::Point;

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayPoint {
    pub row: u32,
    pub column: u32,
}

impl DisplayPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self { row, column }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayRow(pub u32);

pub struct DisplayMap {
    block_map: BlockMap,
}

impl DisplayMap {
    pub fn new(multi_buffer: MultiBuffer) -> Self {
        Self {
            block_map: BlockMap::new(multi_buffer),
        }
    }

    pub fn snapshot(&self) -> DisplaySnapshot {
        let buffer_snapshot = self.block_map.multi_buffer().snapshot();
        let blocks = collect_blocks_from_diff(buffer_snapshot.diff.as_ref());
        let block_snapshot = self.block_map.snapshot(&blocks);
        DisplaySnapshot {
            block_snapshot,
            diff: buffer_snapshot.diff.clone(),
        }
    }
}

fn collect_blocks_from_diff(diff: Option<&BufferDiff>) -> Vec<Block> {
    let diff = match diff {
        Some(d) => d,
        None => return Vec::new(),
    };

    let base_text = match diff.base_text() {
        Some(t) => Arc::clone(t),
        None => return Vec::new(),
    };

    diff.deleted_hunks()
        .iter()
        .map(|hunk| {
            let byte_range = hunk.base_byte_range.clone();
            let base_text = Arc::clone(&base_text);
            Block {
                placement: BlockPlacement::Below(hunk.after_buffer_line),
                content: BlockContent::Lines {
                    line_count: hunk.line_count,
                    get_line: Arc::new(move |line_idx| {
                        let content = &base_text[byte_range.clone()];
                        content
                            .lines()
                            .nth(line_idx as usize)
                            .unwrap_or("")
                            .to_string()
                    }),
                },
            }
        })
        .collect()
}

pub struct DisplaySnapshot {
    block_snapshot: BlockSnapshot,
    diff: Option<BufferDiff>,
}

impl DisplaySnapshot {
    pub fn buffer_to_display(&self, point: Point) -> DisplayPoint {
        let block = self.block_snapshot.buffer_to_block(point);
        DisplayPoint {
            row: block.row,
            column: block.column,
        }
    }

    pub fn display_to_buffer(&self, point: DisplayPoint) -> Option<Point> {
        self.block_snapshot
            .block_to_buffer(BlockPoint::new(point.row, point.column))
    }

    pub fn classify_row(&self, display_row: u32) -> BlockRowKind<'_> {
        self.block_snapshot.classify_row(display_row)
    }

    pub fn max_point(&self) -> DisplayPoint {
        let row = self.block_snapshot.total_lines().saturating_sub(1);
        let column = match self.classify_row(row) {
            BlockRowKind::BufferRow { buffer_row } => self
                .block_snapshot
                .buffer_lines()
                .nth(buffer_row as usize)
                .map(|line| line.len() as u32)
                .unwrap_or(0),
            BlockRowKind::Block { block, line_index } => block.get_line(line_index).len() as u32,
        };
        DisplayPoint::new(row, column)
    }

    pub fn line_count(&self) -> u32 {
        self.block_snapshot.total_lines()
    }

    pub fn buffer_line_count(&self) -> u32 {
        self.block_snapshot.buffer_line_count()
    }

    pub fn text(&self) -> &str {
        self.block_snapshot.buffer_text()
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.block_snapshot.buffer_lines()
    }

    pub fn line_diff_status(&self, buffer_line: u32) -> DiffStatus {
        self.diff
            .as_ref()
            .map(|d| d.status_for_line(buffer_line))
            .unwrap_or_default()
    }

    pub fn has_deletion_after(&self, buffer_line: u32) -> bool {
        self.diff
            .as_ref()
            .map(|d| d.has_deletion_after(buffer_line))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockRowKind, DisplayMap, DisplayPoint, DisplayRow};
    use crate::{
        buffer::{BufferId, TextBuffer},
        git::{BufferDiff, DeletedHunk},
        multi_buffer::MultiBuffer,
    };
    use std::{
        ops::Range,
        sync::{Arc, RwLock},
    };
    use stoat_text::Point;

    fn create_display_map(content: &str) -> DisplayMap {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        DisplayMap::new(multi_buffer)
    }

    fn create_display_map_with_diff(content: &str, diff: BufferDiff) -> DisplayMap {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        buffer.diff = Some(diff);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        DisplayMap::new(multi_buffer)
    }

    fn make_diff_with_deletion(
        after_line: u32,
        base_text: &str,
        byte_range: Range<usize>,
        line_count: u32,
    ) -> BufferDiff {
        let mut diff = BufferDiff::default();
        let base = Arc::new(base_text.to_string());
        diff.set_base_text(base);
        diff.add_deleted_hunk(DeletedHunk {
            after_buffer_line: after_line,
            base_byte_range: byte_range,
            line_count,
        });
        diff
    }

    #[test]
    fn passthrough_coordinates() {
        let display_map = create_display_map("hello\nworld\n");
        let snapshot = display_map.snapshot();

        let buffer_point = Point::new(1, 3);
        let display_point = snapshot.buffer_to_display(buffer_point);
        assert_eq!(display_point, DisplayPoint::new(1, 3));

        let back = snapshot.display_to_buffer(display_point);
        assert_eq!(back, Some(buffer_point));
    }

    #[test]
    fn line_count() {
        let display_map = create_display_map("line1\nline2\nline3");
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.line_count(), 3);
    }

    #[test]
    fn max_point() {
        let display_map = create_display_map("short\nlonger line\nx");
        let snapshot = display_map.snapshot();

        let max = snapshot.max_point();
        assert_eq!(max.row, 2);
        assert_eq!(max.column, 1);
    }

    #[test]
    fn display_row_default() {
        let row = DisplayRow::default();
        assert_eq!(row.0, 0);
    }

    #[test]
    fn line_count_includes_deleted() {
        let base = "line1\ndeleted\nline2";
        let diff = make_diff_with_deletion(0, base, 6..13, 1);
        let display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();

        assert_eq!(snapshot.line_count(), 3);
        assert_eq!(snapshot.buffer_line_count(), 2);
    }

    #[test]
    fn classify_deleted_row() {
        let base = "line1\ndeleted\nline2";
        let diff = make_diff_with_deletion(0, base, 6..13, 1);
        let display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();

        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, line_index } => {
                assert_eq!(block.get_line(line_index), "deleted");
            },
            _ => panic!("expected block"),
        }
    }
}
