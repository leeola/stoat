use crate::multi_buffer::{MultiBuffer, MultiBufferSnapshot};
use std::{cmp::Ordering, sync::Arc};
use stoat_text::{
    Bias, ContextLessSummary, Dimension, Dimensions, Item, Point, SeekTarget, SumTree,
};

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockPoint {
    pub row: u32,
    pub column: u32,
}

impl BlockPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self { row, column }
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockRow(pub u32);

#[derive(Clone)]
pub enum BlockContent {
    Text(Arc<String>),
    Lines {
        line_count: u32,
        get_line: Arc<dyn Fn(u32) -> String + Send + Sync>,
    },
}

impl std::fmt::Debug for BlockContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockContent::Text(s) => f.debug_tuple("Text").field(s).finish(),
            BlockContent::Lines { line_count, .. } => f
                .debug_struct("Lines")
                .field("line_count", line_count)
                .finish_non_exhaustive(),
        }
    }
}

impl BlockContent {
    fn line_count(&self) -> u32 {
        match self {
            BlockContent::Text(s) => s.lines().count().max(1) as u32,
            BlockContent::Lines { line_count, .. } => *line_count,
        }
    }

    fn get_line(&self, index: u32) -> String {
        match self {
            BlockContent::Text(s) => s.lines().nth(index as usize).unwrap_or("").to_string(),
            BlockContent::Lines { get_line, .. } => get_line(index),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum BlockPlacement {
    Above(u32),
    Below(u32),
}

#[derive(Clone, Debug)]
pub struct Block {
    pub placement: BlockPlacement,
    pub content: BlockContent,
}

impl Block {
    pub fn line_count(&self) -> u32 {
        self.content.line_count()
    }

    pub fn get_line(&self, index: u32) -> String {
        self.content.get_line(index)
    }
}

#[derive(Clone, Default, Debug)]
pub struct TransformSummary {
    pub input_rows: u32,
    pub output_rows: u32,
}

impl ContextLessSummary for TransformSummary {
    fn add_summary(&mut self, other: &Self) {
        self.input_rows += other.input_rows;
        self.output_rows += other.output_rows;
    }
}

#[derive(Clone, Debug)]
pub struct Transform {
    pub summary: TransformSummary,
    pub block: Option<Block>,
}

impl Item for Transform {
    type Summary = TransformSummary;
    fn summary(&self, _cx: ()) -> TransformSummary {
        self.summary.clone()
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InputRow(pub u32);

impl<'a> Dimension<'a, TransformSummary> for InputRow {
    fn zero(_cx: ()) -> Self {
        InputRow(0)
    }
    fn add_summary(&mut self, summary: &'a TransformSummary, _cx: ()) {
        self.0 += summary.input_rows;
    }
}

impl<'a> SeekTarget<'a, TransformSummary, InputRow> for InputRow {
    fn cmp(&self, cursor_location: &InputRow, _cx: ()) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.0)
    }
}

impl<'a> SeekTarget<'a, TransformSummary, Dimensions<InputRow, OutputRow>> for InputRow {
    fn cmp(&self, cursor_location: &Dimensions<InputRow, OutputRow>, _cx: ()) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.0 .0)
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutputRow(pub u32);

impl<'a> Dimension<'a, TransformSummary> for OutputRow {
    fn zero(_cx: ()) -> Self {
        OutputRow(0)
    }
    fn add_summary(&mut self, summary: &'a TransformSummary, _cx: ()) {
        self.0 += summary.output_rows;
    }
}

impl<'a> SeekTarget<'a, TransformSummary, OutputRow> for OutputRow {
    fn cmp(&self, cursor_location: &OutputRow, _cx: ()) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.0)
    }
}

impl<'a> SeekTarget<'a, TransformSummary, Dimensions<InputRow, OutputRow>> for OutputRow {
    fn cmp(&self, cursor_location: &Dimensions<InputRow, OutputRow>, _cx: ()) -> Ordering {
        Ord::cmp(&self.0, &cursor_location.1 .0)
    }
}

pub enum BlockRowKind<'a> {
    BufferRow { buffer_row: u32 },
    Block { block: &'a Block, line_index: u32 },
}

pub struct BlockMap {
    multi_buffer: MultiBuffer,
}

impl BlockMap {
    pub fn new(multi_buffer: MultiBuffer) -> Self {
        Self { multi_buffer }
    }

    pub fn snapshot(&self, blocks: &[Block]) -> BlockSnapshot {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let buffer_line_count = buffer_snapshot.line_count();
        let transforms = build_transforms(buffer_line_count, blocks);
        let total_rows: OutputRow = transforms.extent(());
        BlockSnapshot {
            buffer_snapshot,
            transforms,
            total_rows: total_rows.0,
        }
    }

    pub fn multi_buffer(&self) -> &MultiBuffer {
        &self.multi_buffer
    }
}

pub struct BlockSnapshot {
    buffer_snapshot: MultiBufferSnapshot,
    transforms: SumTree<Transform>,
    total_rows: u32,
}

impl BlockSnapshot {
    pub fn buffer_to_block(&self, point: Point) -> BlockPoint {
        let target = InputRow(point.row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input, output) = cursor.start();
        let rows_into_transform = point.row.saturating_sub(input.0);
        let block_row = output.0 + rows_into_transform;

        BlockPoint {
            row: block_row,
            column: point.column,
        }
    }

    pub fn block_to_buffer(&self, point: BlockPoint) -> Option<Point> {
        match self.classify_row(point.row) {
            BlockRowKind::BufferRow { buffer_row } => Some(Point::new(buffer_row, point.column)),
            BlockRowKind::Block { .. } => None,
        }
    }

    pub fn classify_row(&self, block_row: u32) -> BlockRowKind<'_> {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if let Some(ref block) = transform.block {
                return BlockRowKind::Block {
                    block,
                    line_index: rows_into_transform,
                };
            }
        }

        let buffer_row = input_start.0 + rows_into_transform;
        BlockRowKind::BufferRow { buffer_row }
    }

    pub fn total_lines(&self) -> u32 {
        self.total_rows
    }

    pub fn buffer_line_count(&self) -> u32 {
        self.buffer_snapshot.line_count()
    }

    pub fn buffer_text(&self) -> &str {
        self.buffer_snapshot.text()
    }

    pub fn buffer_lines(&self) -> impl Iterator<Item = &str> {
        self.buffer_snapshot.lines()
    }

    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        &self.buffer_snapshot
    }
}

fn build_transforms(buffer_line_count: u32, blocks: &[Block]) -> SumTree<Transform> {
    let mut transforms = SumTree::new(());

    if blocks.is_empty() {
        if buffer_line_count > 0 {
            transforms.push(
                Transform {
                    summary: TransformSummary {
                        input_rows: buffer_line_count,
                        output_rows: buffer_line_count,
                    },
                    block: None,
                },
                (),
            );
        }
        return transforms;
    }

    let mut sorted_blocks: Vec<_> = blocks.iter().collect();
    sorted_blocks.sort_by(|a, b| {
        let anchor_a = match a.placement {
            BlockPlacement::Above(row) => (row, 0),
            BlockPlacement::Below(row) => (row, 1),
        };
        let anchor_b = match b.placement {
            BlockPlacement::Above(row) => (row, 0),
            BlockPlacement::Below(row) => (row, 1),
        };
        anchor_a.cmp(&anchor_b)
    });

    let mut current_buffer_row = 0u32;

    for block in sorted_blocks {
        let anchor = match block.placement {
            BlockPlacement::Above(row) => row,
            BlockPlacement::Below(row) => row + 1,
        };

        if anchor > current_buffer_row {
            let rows = anchor - current_buffer_row;
            push_isomorphic(&mut transforms, rows);
            current_buffer_row = anchor;
        }

        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: 0,
                    output_rows: block.line_count(),
                },
                block: Some(block.clone()),
            },
            (),
        );
    }

    if current_buffer_row < buffer_line_count {
        let rows = buffer_line_count - current_buffer_row;
        push_isomorphic(&mut transforms, rows);
    }

    transforms
}

fn push_isomorphic(transforms: &mut SumTree<Transform>, rows: u32) {
    let mut merged = false;
    transforms.update_last(
        |last| {
            if last.block.is_none() {
                last.summary.input_rows += rows;
                last.summary.output_rows += rows;
                merged = true;
            }
        },
        (),
    );

    if !merged {
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: rows,
                    output_rows: rows,
                },
                block: None,
            },
            (),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Block, BlockContent, BlockMap, BlockPlacement, BlockPoint, BlockRowKind};
    use crate::{
        buffer::{BufferId, TextBuffer},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::Point;

    fn create_block_map(content: &str) -> BlockMap {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        BlockMap::new(multi_buffer)
    }

    fn text_block(placement: BlockPlacement, content: &str) -> Block {
        Block {
            placement,
            content: BlockContent::Text(Arc::new(content.to_string())),
        }
    }

    #[test]
    fn no_blocks_passthrough() {
        let block_map = create_block_map("line1\nline2\nline3");
        let snapshot = block_map.snapshot(&[]);

        assert_eq!(snapshot.total_lines(), 3);

        let block = snapshot.buffer_to_block(Point::new(1, 2));
        assert_eq!(block, BlockPoint::new(1, 2));

        let buffer = snapshot.block_to_buffer(BlockPoint::new(1, 2));
        assert_eq!(buffer, Some(Point::new(1, 2)));
    }

    #[test]
    fn classify_buffer_row_no_blocks() {
        let block_map = create_block_map("line1\nline2\nline3");
        let snapshot = block_map.snapshot(&[]);

        match snapshot.classify_row(1) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            BlockRowKind::Block { .. } => panic!("expected buffer row"),
        }
    }

    #[test]
    fn block_below_first_line() {
        let block_map = create_block_map("line1\nline2");
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = block_map.snapshot(&blocks);

        assert_eq!(snapshot.total_lines(), 3);

        match snapshot.classify_row(0) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 0),
            _ => panic!("expected buffer row"),
        }

        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, line_index } => {
                assert_eq!(line_index, 0);
                assert_eq!(block.get_line(0), "deleted");
            },
            _ => panic!("expected block"),
        }

        match snapshot.classify_row(2) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn buffer_to_block_with_block() {
        let block_map = create_block_map("line1\nline2");
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = block_map.snapshot(&blocks);

        let block = snapshot.buffer_to_block(Point::new(0, 0));
        assert_eq!(block, BlockPoint::new(0, 0));

        let block = snapshot.buffer_to_block(Point::new(1, 0));
        assert_eq!(block, BlockPoint::new(2, 0));
    }

    #[test]
    fn block_to_buffer_returns_none_for_block() {
        let block_map = create_block_map("line1\nline2");
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = block_map.snapshot(&blocks);

        assert!(snapshot.block_to_buffer(BlockPoint::new(1, 0)).is_none());
        assert_eq!(
            snapshot.block_to_buffer(BlockPoint::new(2, 0)),
            Some(Point::new(1, 0))
        );
    }

    #[test]
    fn multiline_block() {
        let block_map = create_block_map("line1\nline2");
        let blocks = vec![text_block(BlockPlacement::Below(0), "del1\ndel2\ndel3")];
        let snapshot = block_map.snapshot(&blocks);

        assert_eq!(snapshot.total_lines(), 5);

        for (row, expected) in [(1, "del1"), (2, "del2"), (3, "del3")] {
            match snapshot.classify_row(row) {
                BlockRowKind::Block { block, line_index } => {
                    assert_eq!(block.get_line(line_index), expected);
                },
                _ => panic!("expected block at row {}", row),
            }
        }

        match snapshot.classify_row(4) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn block_above() {
        let block_map = create_block_map("line1\nline2");
        let blocks = vec![text_block(BlockPlacement::Above(1), "inserted")];
        let snapshot = block_map.snapshot(&blocks);

        assert_eq!(snapshot.total_lines(), 3);

        match snapshot.classify_row(0) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 0),
            _ => panic!("expected buffer row"),
        }

        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, .. } => {
                assert_eq!(block.get_line(0), "inserted");
            },
            _ => panic!("expected block"),
        }

        match snapshot.classify_row(2) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            _ => panic!("expected buffer row"),
        }
    }

    #[test]
    fn multiple_blocks() {
        let block_map = create_block_map("line1\nline2\nline3");
        let blocks = vec![
            text_block(BlockPlacement::Below(0), "after0"),
            text_block(BlockPlacement::Below(1), "after1"),
        ];
        let snapshot = block_map.snapshot(&blocks);

        assert_eq!(snapshot.total_lines(), 5);

        let classifications: Vec<_> = (0..5)
            .map(|row| match snapshot.classify_row(row) {
                BlockRowKind::BufferRow { buffer_row } => format!("buf{}", buffer_row),
                BlockRowKind::Block { block, .. } => format!("blk:{}", block.get_line(0)),
            })
            .collect();

        assert_eq!(
            classifications,
            vec!["buf0", "blk:after0", "buf1", "blk:after1", "buf2"]
        );
    }
}
