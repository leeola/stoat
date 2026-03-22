use super::wrap_map::WrapSnapshot;
use std::{cmp::Ordering, ops::Deref, sync::Arc};
use stoat_text::{
    patch::Patch, Bias, ContextLessSummary, Dimension, Dimensions, Item, Point, SeekTarget, SumTree,
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
    Text(Arc<Vec<String>>),
    Lines {
        line_count: u32,
        get_line: Arc<dyn Fn(u32) -> String + Send + Sync>,
    },
}

impl std::fmt::Debug for BlockContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockContent::Text(lines) => f.debug_tuple("Text").field(lines).finish(),
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
            BlockContent::Text(lines) => lines.len().max(1) as u32,
            BlockContent::Lines { line_count, .. } => *line_count,
        }
    }

    fn get_line(&self, index: u32) -> String {
        match self {
            BlockContent::Text(lines) => lines.get(index as usize).cloned().unwrap_or_default(),
            BlockContent::Lines { get_line, .. } => get_line(index),
        }
    }

    fn line_len(&self, index: u32) -> u32 {
        match self {
            BlockContent::Text(lines) => lines.get(index as usize).map_or(0, |l| l.len() as u32),
            BlockContent::Lines { get_line, .. } => get_line(index).len() as u32,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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

    pub fn line_len(&self, index: u32) -> u32 {
        self.content.line_len(index)
    }

    pub fn write_line(&self, buf: &mut String, index: u32) {
        match &self.content {
            BlockContent::Text(lines) => {
                if let Some(line) = lines.get(index as usize) {
                    buf.push_str(line);
                }
            },
            BlockContent::Lines { get_line, .. } => buf.push_str(&get_line(index)),
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct TransformSummary {
    pub input_rows: u32,
    pub output_rows: u32,
    pub longest_row: u32,
    pub longest_row_chars: u32,
}

impl ContextLessSummary for TransformSummary {
    fn add_summary(&mut self, other: &Self) {
        if other.longest_row_chars > self.longest_row_chars {
            self.longest_row = self.output_rows + other.longest_row;
            self.longest_row_chars = other.longest_row_chars;
        }
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
    cached_transforms: Option<SumTree<Transform>>,
    cached_total_rows: u32,
    last_block_fingerprint: Vec<(BlockPlacement, u32)>,
}

impl Default for BlockMap {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockMap {
    pub fn new() -> Self {
        Self {
            cached_transforms: None,
            cached_total_rows: 0,
            last_block_fingerprint: Vec::new(),
        }
    }

    pub fn sync(
        &mut self,
        wrap_snapshot: Arc<WrapSnapshot>,
        blocks: &[Block],
        wrap_edits: &Patch<u32>,
    ) -> BlockSnapshot {
        let fingerprint: Vec<(BlockPlacement, u32)> = blocks
            .iter()
            .map(|b| (b.placement, b.line_count()))
            .collect();

        if wrap_edits.is_empty() && fingerprint == self.last_block_fingerprint {
            if let Some(ref transforms) = self.cached_transforms {
                return BlockSnapshot {
                    wrap_snapshot,
                    transforms: transforms.clone(),
                    total_rows: self.cached_total_rows,
                };
            }
        }

        let wrap_line_count = wrap_snapshot.line_count();
        let transforms = build_transforms(wrap_line_count, blocks, &wrap_snapshot);
        let total_rows: OutputRow = transforms.extent(());

        self.cached_transforms = Some(transforms.clone());
        self.cached_total_rows = total_rows.0;
        self.last_block_fingerprint = fingerprint;

        BlockSnapshot {
            wrap_snapshot,
            transforms,
            total_rows: total_rows.0,
        }
    }
}

pub struct BlockSnapshot {
    wrap_snapshot: Arc<WrapSnapshot>,
    transforms: SumTree<Transform>,
    total_rows: u32,
}

impl Deref for BlockSnapshot {
    type Target = WrapSnapshot;
    fn deref(&self) -> &WrapSnapshot {
        &self.wrap_snapshot
    }
}

impl BlockSnapshot {
    pub fn buffer_to_block(&self, point: Point) -> BlockPoint {
        let inlay_point = self
            .wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .inlay_snapshot()
            .to_inlay_point(point);
        let fold_point = self
            .wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .to_fold_point(inlay_point, Bias::Right);
        let tab_point = self.wrap_snapshot.tab_snapshot().to_tab_point(fold_point);
        let wrap_point = self.wrap_snapshot.to_wrap_point(tab_point);
        let wrap_row = wrap_point.row();

        let target = InputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input, output, _) = cursor.start();
        let rows_into_transform = wrap_row.saturating_sub(input.0);
        let block_row = output.0 + rows_into_transform;

        BlockPoint {
            row: block_row,
            column: wrap_point.column(),
        }
    }

    pub fn block_to_buffer(&self, point: BlockPoint) -> Option<Point> {
        let target = OutputRow(point.row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = point.row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if transform.block.is_some() {
                return None;
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        let wrap_point = super::wrap_map::WrapPoint::new(wrap_row, point.column);
        let tab_point = self.wrap_snapshot.to_tab_point(wrap_point);
        let fold_point = self
            .wrap_snapshot
            .tab_snapshot()
            .to_fold_point(tab_point, Bias::Left);
        let inlay_point = self
            .wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .to_inlay_point(fold_point);
        let buf = self
            .wrap_snapshot
            .tab_snapshot()
            .fold_snapshot()
            .inlay_snapshot()
            .to_buffer_point(inlay_point);
        Some(buf)
    }

    pub fn classify_row(&self, block_row: u32) -> BlockRowKind<'_> {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if let Some(ref block) = transform.block {
                return BlockRowKind::Block {
                    block,
                    line_index: rows_into_transform,
                };
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        let tab_point = self
            .wrap_snapshot
            .to_tab_point(super::wrap_map::WrapPoint::new(wrap_row, 0));
        let inlay_point = self
            .wrap_snapshot
            .fold_snapshot()
            .to_inlay_point(super::fold_map::FoldPoint::new(tab_point.row(), 0));
        let buffer_point = self
            .wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .to_buffer_point(inlay_point);

        BlockRowKind::BufferRow {
            buffer_row: buffer_point.row,
        }
    }

    pub fn clip_point(&self, point: BlockPoint, bias: Bias) -> BlockPoint {
        let row = point.row.min(self.total_rows.saturating_sub(1));
        match self.classify_row(row) {
            BlockRowKind::BufferRow { .. } => {
                let col = point.column.min(self.line_len(row));
                BlockPoint::new(row, col)
            },
            BlockRowKind::Block { .. } => {
                let target = OutputRow(row + 1);
                let mut cursor = self
                    .transforms
                    .cursor::<Dimensions<InputRow, OutputRow>>(());
                cursor.seek(&target, Bias::Left);

                if bias == Bias::Left {
                    cursor.prev();
                    while let Some(t) = cursor.item() {
                        if t.block.is_none() {
                            let end = cursor.end();
                            let last_buf_row = end.1 .0.saturating_sub(1);
                            return BlockPoint::new(last_buf_row, self.line_len(last_buf_row));
                        }
                        cursor.prev();
                    }
                    BlockPoint::new(0, 0)
                } else {
                    cursor.next();
                    while let Some(t) = cursor.item() {
                        if t.block.is_none() {
                            let start_row = cursor.start().1 .0;
                            return BlockPoint::new(start_row, 0);
                        }
                        cursor.next();
                    }
                    self.max_point()
                }
            },
        }
    }

    pub fn line_len(&self, block_row: u32) -> u32 {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if let Some(ref block) = transform.block {
                return block.line_len(rows_into_transform);
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.line_len(wrap_row)
    }

    pub fn max_point(&self) -> BlockPoint {
        let last_row = self.total_rows.saturating_sub(1);
        BlockPoint::new(last_row, self.line_len(last_row))
    }

    pub fn total_lines(&self) -> u32 {
        self.total_rows
    }

    pub fn buffer_line_count(&self) -> u32 {
        self.wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
            .line_count()
    }

    pub fn buffer_text(&self) -> &str {
        self.wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
            .text()
    }

    pub fn buffer_lines(&self) -> impl Iterator<Item = &str> {
        self.wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
            .lines()
    }

    pub fn buffer_snapshot(&self) -> &crate::multi_buffer::MultiBufferSnapshot {
        self.wrap_snapshot
            .fold_snapshot()
            .inlay_snapshot()
            .buffer_snapshot()
    }

    pub fn longest_row(&self) -> (u32, u32) {
        let s = self.transforms.summary();
        (s.longest_row, s.longest_row_chars)
    }

    pub fn wrap_snapshot(&self) -> &WrapSnapshot {
        &self.wrap_snapshot
    }

    pub fn write_display_line(&self, buf: &mut String, block_row: u32) {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if let Some(ref block) = transform.block {
                block.write_line(buf, rows_into_transform);
                return;
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.write_display_line(buf, wrap_row);
    }

    pub fn display_line(&self, block_row: u32) -> String {
        let mut result = String::new();
        self.write_display_line(&mut result, block_row);
        result
    }

    pub fn soft_wrap_indent(&self, block_row: u32) -> u32 {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if transform.block.is_some() {
                return 0;
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.soft_wrap_indent(wrap_row)
    }

    pub fn wrap_width(&self) -> Option<u32> {
        self.wrap_snapshot.wrap_width()
    }

    pub fn is_wrap_continuation(&self, block_row: u32) -> bool {
        let target = OutputRow(block_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let rows_into_transform = block_row.saturating_sub(output_start.0);

        if let Some(transform) = cursor.item() {
            if transform.block.is_some() {
                return false;
            }
        }

        let wrap_row = input_start.0 + rows_into_transform;
        self.wrap_snapshot.classify_row(wrap_row) == super::wrap_map::WrapRowKind::Continuation
    }
}

fn build_transforms(
    wrap_line_count: u32,
    blocks: &[Block],
    wrap_snapshot: &WrapSnapshot,
) -> SumTree<Transform> {
    let mut transforms = SumTree::new(());

    if blocks.is_empty() {
        if wrap_line_count > 0 {
            let (longest_row, longest_row_chars) = wrap_snapshot.longest_line();
            transforms.push(
                Transform {
                    summary: TransformSummary {
                        input_rows: wrap_line_count,
                        output_rows: wrap_line_count,
                        longest_row,
                        longest_row_chars,
                    },
                    block: None,
                },
                (),
            );
        }
        return transforms;
    }

    let mut keyed_blocks: Vec<(u32, &Block)> = blocks
        .iter()
        .map(|b| (block_anchor_wrap_row(b, wrap_snapshot), b))
        .collect();
    keyed_blocks.sort_unstable_by_key(|&(row, _)| row);

    let mut current_wrap_row = 0u32;

    for (anchor, block) in keyed_blocks {
        if anchor > current_wrap_row {
            let rows = anchor - current_wrap_row;
            push_isomorphic(&mut transforms, rows, current_wrap_row, wrap_snapshot);
            current_wrap_row = anchor;
        }

        let (blk_longest_row, blk_longest_chars) = longest_block_line(block);
        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: 0,
                    output_rows: block.line_count(),
                    longest_row: blk_longest_row,
                    longest_row_chars: blk_longest_chars,
                },
                block: Some(block.clone()),
            },
            (),
        );
    }

    if current_wrap_row < wrap_line_count {
        let rows = wrap_line_count - current_wrap_row;
        push_isomorphic(&mut transforms, rows, current_wrap_row, wrap_snapshot);
    }

    transforms
}

fn block_anchor_wrap_row(block: &Block, wrap_snapshot: &WrapSnapshot) -> u32 {
    let buffer_row = match block.placement {
        BlockPlacement::Above(row) => row,
        BlockPlacement::Below(row) => row,
    };

    let inlay_point = wrap_snapshot
        .fold_snapshot()
        .inlay_snapshot()
        .to_inlay_point(Point::new(buffer_row, 0));
    let fold_point = wrap_snapshot
        .fold_snapshot()
        .to_fold_point(inlay_point, Bias::Right);
    let tab_point = super::tab_map::TabPoint::new(fold_point.row(), fold_point.column());
    let wrap_point = wrap_snapshot.to_wrap_point(tab_point);
    let wrap_row = wrap_point.row();

    match block.placement {
        BlockPlacement::Above(_) => wrap_row,
        BlockPlacement::Below(_) => wrap_row + 1,
    }
}

fn longest_block_line(block: &Block) -> (u32, u32) {
    let mut best_row = 0u32;
    let mut best_chars = 0u32;
    for i in 0..block.line_count() {
        let len = block.line_len(i);
        if len > best_chars {
            best_row = i;
            best_chars = len;
        }
    }
    (best_row, best_chars)
}

fn push_isomorphic(
    transforms: &mut SumTree<Transform>,
    rows: u32,
    start_wrap_row: u32,
    wrap_snapshot: &WrapSnapshot,
) {
    let (longest_row, longest_row_chars) =
        wrap_snapshot.longest_in_output_range(start_wrap_row, rows);

    let mut merged = false;
    transforms.update_last(
        |last| {
            if last.block.is_none() {
                if longest_row_chars > last.summary.longest_row_chars {
                    last.summary.longest_row = last.summary.output_rows + longest_row;
                    last.summary.longest_row_chars = longest_row_chars;
                }
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
                    longest_row,
                    longest_row_chars,
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
        display_map::{fold_map::FoldMap, inlay_map::InlayMap, tab_map::TabMap, wrap_map::WrapMap},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::{patch::Patch, Bias, Point};

    fn create_block_snapshot(content: &str, blocks: &[Block]) -> super::BlockSnapshot {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (_, wrap_snapshot) = WrapMap::new(tab_snapshot, None);
        let mut block_map = BlockMap::new();
        block_map.sync(wrap_snapshot, blocks, &Patch::empty())
    }

    fn text_block(placement: BlockPlacement, content: &str) -> Block {
        let lines: Vec<String> = content.lines().map(String::from).collect();
        Block {
            placement,
            content: BlockContent::Text(Arc::new(lines)),
        }
    }

    #[test]
    fn no_blocks_passthrough() {
        let snapshot = create_block_snapshot("line1\nline2\nline3", &[]);

        assert_eq!(snapshot.total_lines(), 3);

        let block = snapshot.buffer_to_block(Point::new(1, 2));
        assert_eq!(block, BlockPoint::new(1, 2));

        let buffer = snapshot.block_to_buffer(BlockPoint::new(1, 2));
        assert_eq!(buffer, Some(Point::new(1, 2)));
    }

    #[test]
    fn classify_buffer_row_no_blocks() {
        let snapshot = create_block_snapshot("line1\nline2\nline3", &[]);

        match snapshot.classify_row(1) {
            BlockRowKind::BufferRow { buffer_row } => assert_eq!(buffer_row, 1),
            BlockRowKind::Block { .. } => panic!("expected buffer row"),
        }
    }

    #[test]
    fn block_below_first_line() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

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
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

        let block = snapshot.buffer_to_block(Point::new(0, 0));
        assert_eq!(block, BlockPoint::new(0, 0));

        let block = snapshot.buffer_to_block(Point::new(1, 0));
        assert_eq!(block, BlockPoint::new(2, 0));
    }

    #[test]
    fn block_to_buffer_returns_none_for_block() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

        assert!(snapshot.block_to_buffer(BlockPoint::new(1, 0)).is_none());
        assert_eq!(
            snapshot.block_to_buffer(BlockPoint::new(2, 0)),
            Some(Point::new(1, 0))
        );
    }

    #[test]
    fn multiline_block() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "del1\ndel2\ndel3")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

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
        let blocks = vec![text_block(BlockPlacement::Above(1), "inserted")];
        let snapshot = create_block_snapshot("line1\nline2", &blocks);

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
        let blocks = vec![
            text_block(BlockPlacement::Below(0), "after0"),
            text_block(BlockPlacement::Below(1), "after1"),
        ];
        let snapshot = create_block_snapshot("line1\nline2\nline3", &blocks);

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

    #[test]
    fn line_len_no_blocks() {
        let snapshot = create_block_snapshot("hello\nhi", &[]);
        assert_eq!(snapshot.line_len(0), 5);
        assert_eq!(snapshot.line_len(1), 2);
    }

    #[test]
    fn line_len_with_block() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted line")];
        let snapshot = create_block_snapshot("hello\nhi", &blocks);
        assert_eq!(snapshot.line_len(0), 5);
        assert_eq!(snapshot.line_len(1), 12);
        assert_eq!(snapshot.line_len(2), 2);
    }

    #[test]
    fn max_point_no_blocks() {
        let snapshot = create_block_snapshot("hello\nhi", &[]);
        assert_eq!(snapshot.max_point(), BlockPoint::new(1, 2));
    }

    #[test]
    fn clip_point_clamps_column() {
        let snapshot = create_block_snapshot("hello\nhi", &[]);
        assert_eq!(
            snapshot.clip_point(BlockPoint::new(0, 100), Bias::Left),
            BlockPoint::new(0, 5)
        );
        assert_eq!(
            snapshot.clip_point(BlockPoint::new(5, 0), Bias::Left),
            BlockPoint::new(1, 0)
        );
    }

    #[test]
    fn clip_point_snaps_off_block_row() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let snapshot = create_block_snapshot("hello\nworld", &blocks);
        // Row 1 is a block row
        let clipped_left = snapshot.clip_point(BlockPoint::new(1, 0), Bias::Left);
        assert_eq!(clipped_left, BlockPoint::new(0, 5));

        let clipped_right = snapshot.clip_point(BlockPoint::new(1, 0), Bias::Right);
        assert_eq!(clipped_right, BlockPoint::new(2, 0));
    }

    #[test]
    fn block_to_buffer_reverses_tabs() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("\thello");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (_, wrap_snapshot) = WrapMap::new(tab_snapshot, None);
        let mut block_map = BlockMap::new();
        let snapshot = block_map.sync(wrap_snapshot, &[], &Patch::empty());

        let buf = snapshot.block_to_buffer(BlockPoint::new(0, 5)).unwrap();
        assert_eq!(buf, Point::new(0, 2));
    }

    #[test]
    fn block_line_len_matches_get_line() {
        let block = text_block(BlockPlacement::Below(0), "short\nlonger line\nx");
        for i in 0..block.line_count() {
            assert_eq!(
                block.line_len(i),
                block.get_line(i).len() as u32,
                "mismatch at line {i}"
            );
        }
    }

    #[test]
    fn block_content_pre_split_matches() {
        let text_content = BlockContent::Text(Arc::new(
            "first\nsecond line\nthird"
                .lines()
                .map(String::from)
                .collect(),
        ));
        let lines_content = BlockContent::Lines {
            line_count: 3,
            get_line: Arc::new(|i| ["first", "second line", "third"][i as usize].to_string()),
        };

        assert_eq!(text_content.line_count(), lines_content.line_count());
        for i in 0..text_content.line_count() {
            assert_eq!(
                text_content.get_line(i),
                lines_content.get_line(i),
                "get_line mismatch at {i}"
            );
            assert_eq!(
                text_content.line_len(i),
                lines_content.line_len(i),
                "line_len mismatch at {i}"
            );
        }
    }

    #[test]
    fn write_display_line_matches_display_line() {
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted\nlines")];
        let snapshot = create_block_snapshot("hello\nworld\nfoo", &blocks);
        for row in 0..snapshot.total_lines() {
            let expected = snapshot.display_line(row);
            let mut buf = String::new();
            snapshot.write_display_line(&mut buf, row);
            assert_eq!(buf, expected, "mismatch at row {row}");
        }
    }

    fn create_wrap_snapshot(content: &str) -> Arc<super::WrapSnapshot> {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (_, wrap_snapshot) = WrapMap::new(tab_snapshot, None);
        wrap_snapshot
    }

    #[test]
    fn cache_reused_when_nothing_changes() {
        let wrap_snapshot = create_wrap_snapshot("hello\nworld");
        let blocks = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let mut block_map = BlockMap::new();

        let snap1 = block_map.sync(Arc::clone(&wrap_snapshot), &blocks, &Patch::empty());
        let snap2 = block_map.sync(wrap_snapshot, &blocks, &Patch::empty());

        assert_eq!(snap1.total_lines(), snap2.total_lines());
        assert_eq!(snap1.longest_row(), snap2.longest_row());
    }

    #[test]
    fn cache_invalidated_on_block_change() {
        let wrap_snapshot = create_wrap_snapshot("hello\nworld");
        let blocks1 = vec![text_block(BlockPlacement::Below(0), "deleted")];
        let blocks2 = vec![text_block(BlockPlacement::Below(0), "deleted\nextra line")];
        let mut block_map = BlockMap::new();

        let snap1 = block_map.sync(Arc::clone(&wrap_snapshot), &blocks1, &Patch::empty());
        assert_eq!(snap1.total_lines(), 3);

        let snap2 = block_map.sync(wrap_snapshot, &blocks2, &Patch::empty());
        assert_eq!(snap2.total_lines(), 4);
    }
}
