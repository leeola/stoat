mod block_map;
mod fold_map;
pub mod inlay_map;
pub mod invisibles;
pub mod tab_map;
mod wrap_map;

use crate::{
    git::{BufferDiff, DiffStatus},
    multi_buffer::MultiBuffer,
};
pub use block_map::{
    Block, BlockContent, BlockMap, BlockPlacement, BlockPoint, BlockRow, BlockRowKind,
    BlockSnapshot,
};
pub use fold_map::{FoldMap, FoldPlaceholder, FoldPoint, FoldSnapshot};
pub use inlay_map::{InlayMap, InlayPoint, InlaySnapshot};
use std::sync::Arc;
use stoat_text::{patch::Patch, Bias, CharsAt, Point, ReversedCharsAt, Rope};
pub use tab_map::{TabMap, TabPoint, TabRow, TabSnapshot};
use unicode_width::UnicodeWidthChar;
pub use wrap_map::{WrapMap, WrapPoint, WrapSnapshot};

pub(crate) fn display_width(ch: char) -> u32 {
    ch.width().unwrap_or(0) as u32
}

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
    multi_buffer: MultiBuffer,
    inlay_map: InlayMap,
    fold_map: FoldMap,
    tab_map: TabMap,
    wrap_map: WrapMap,
    block_map: BlockMap,
    last_buffer_version: usize,
}

impl DisplayMap {
    pub fn new(multi_buffer: MultiBuffer) -> Self {
        let buffer_snapshot = multi_buffer.snapshot();
        let version = buffer_snapshot.version;
        let (inlay_map, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (fold_map, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let (tab_snapshot, _) = tab_map.sync(fold_snapshot, Patch::empty());
        let (wrap_map, _wrap_snapshot) = WrapMap::new(tab_snapshot, None);
        let block_map = BlockMap::new();

        Self {
            multi_buffer,
            inlay_map,
            fold_map,
            tab_map,
            wrap_map,
            block_map,
            last_buffer_version: version,
        }
    }

    pub fn fold(&mut self, ranges: Vec<std::ops::Range<Point>>) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let anchor_ranges = ranges
            .into_iter()
            .map(|r| {
                let start_off = buffer_snapshot.rope.point_to_offset(r.start);
                let end_off = buffer_snapshot.rope.point_to_offset(r.end);
                buffer_snapshot.anchor_at(start_off, Bias::Right)
                    ..buffer_snapshot.anchor_at(end_off, Bias::Left)
            })
            .collect();
        self.fold_map
            .fold(anchor_ranges, FoldPlaceholder::default(), &buffer_snapshot);
    }

    pub fn unfold(&mut self, ranges: Vec<std::ops::Range<Point>>) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let offset_ranges = ranges
            .into_iter()
            .map(|r| {
                let start_off = buffer_snapshot.rope.point_to_offset(r.start);
                let end_off = buffer_snapshot.rope.point_to_offset(r.end);
                start_off..end_off
            })
            .collect();
        self.fold_map.unfold(offset_ranges, &buffer_snapshot);
    }

    pub fn toggle_fold(&mut self, ranges: Vec<std::ops::Range<Point>>) {
        let buffer_snapshot = self.multi_buffer.snapshot();
        let any_folded = ranges.iter().any(|r| {
            let offset = buffer_snapshot.rope.point_to_offset(r.start);
            self.fold_map.is_folded_at_offset(offset, &buffer_snapshot)
        });
        if any_folded {
            self.unfold(ranges);
        } else {
            self.fold(ranges);
        }
    }

    pub fn set_wrap_width(&mut self, width: Option<u32>) {
        self.wrap_map.set_wrap_width(width);
    }

    pub fn snapshot(&mut self) -> DisplaySnapshot {
        let watermark = self
            .fold_map
            .min_anchor_version()
            .min(self.inlay_map.min_anchor_version());
        self.multi_buffer.compact_edit_log(watermark);
        let buffer_snapshot = self.multi_buffer.snapshot();
        let diff = buffer_snapshot.diff.clone();
        let buffer_edits = buffer_snapshot.edits_since(self.last_buffer_version);
        self.last_buffer_version = buffer_snapshot.version;
        let (inlay_snapshot, inlay_edits) = self.inlay_map.sync(buffer_snapshot, &buffer_edits);
        let (fold_snapshot, fold_edits) = self.fold_map.sync(inlay_snapshot, &inlay_edits);
        let (tab_snapshot, tab_edits) = self.tab_map.sync(fold_snapshot, fold_edits);
        let (wrap_snapshot, wrap_edits) = self.wrap_map.sync(tab_snapshot, &tab_edits);
        let blocks = collect_blocks_from_diff(diff.as_ref());
        let block_snapshot = self.block_map.sync(wrap_snapshot, &blocks, &wrap_edits);

        DisplaySnapshot {
            block_snapshot,
            diff,
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
            let content = &base_text[hunk.base_byte_range.clone()];
            let lines: Vec<String> = content.lines().map(String::from).collect();
            Block {
                placement: BlockPlacement::Below(hunk.after_buffer_line),
                content: BlockContent::Text(Arc::new(lines)),
            }
        })
        .collect()
}

pub struct DisplaySnapshot {
    block_snapshot: BlockSnapshot,
    diff: Option<BufferDiff>,
}

impl DisplaySnapshot {
    pub fn version(&self) -> usize {
        self.fold_snapshot().version()
    }

    pub fn tab_snapshot(&self) -> &TabSnapshot {
        self.block_snapshot.wrap_snapshot().tab_snapshot()
    }

    pub fn fold_snapshot(&self) -> &FoldSnapshot {
        self.tab_snapshot().fold_snapshot()
    }

    pub fn inlay_snapshot(&self) -> &InlaySnapshot {
        self.fold_snapshot().inlay_snapshot()
    }

    pub fn wrap_snapshot(&self) -> &WrapSnapshot {
        self.block_snapshot.wrap_snapshot()
    }

    pub fn longest_row(&self) -> (u32, u32) {
        self.block_snapshot.longest_row()
    }

    pub fn is_line_folded(&self, buffer_row: u32) -> bool {
        let inlay_point = self
            .fold_snapshot()
            .inlay_snapshot()
            .to_inlay_point(Point::new(buffer_row, 0));
        self.fold_snapshot().is_line_folded(inlay_point.row())
    }

    pub fn buffer_to_display(&self, point: Point) -> DisplayPoint {
        let block = self.block_snapshot.buffer_to_block(point);
        DisplayPoint::new(block.row, block.column)
    }

    pub fn display_to_buffer(&self, point: DisplayPoint) -> Option<Point> {
        self.block_snapshot
            .block_to_buffer(BlockPoint::new(point.row, point.column))
    }

    pub fn classify_row(&self, display_row: u32) -> BlockRowKind<'_> {
        self.block_snapshot.classify_row(display_row)
    }

    pub fn clip_point(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        let bp = self
            .block_snapshot
            .clip_point(BlockPoint::new(point.row, point.column), bias);
        DisplayPoint::new(bp.row, bp.column)
    }

    pub fn max_point(&self) -> DisplayPoint {
        let bp = self.block_snapshot.max_point();
        DisplayPoint::new(bp.row, bp.column)
    }

    pub fn line_len(&self, display_row: u32) -> u32 {
        self.block_snapshot.line_len(display_row)
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

    pub fn write_display_line(&self, buf: &mut String, display_row: u32) {
        self.block_snapshot.write_display_line(buf, display_row);
    }

    pub fn display_line(&self, display_row: u32) -> String {
        let mut result = String::new();
        self.write_display_line(&mut result, display_row);
        result
    }

    pub fn display_lines(&self, range: std::ops::Range<u32>) -> impl Iterator<Item = String> + '_ {
        range.map(move |row| self.display_line(row))
    }

    pub fn is_wrap_continuation(&self, display_row: u32) -> bool {
        self.block_snapshot.is_wrap_continuation(display_row)
    }

    pub fn soft_wrap_indent(&self, display_row: u32) -> u32 {
        self.block_snapshot.soft_wrap_indent(display_row)
    }

    pub fn wrap_width(&self) -> Option<u32> {
        self.block_snapshot.wrap_width()
    }

    pub fn has_deletion_after(&self, buffer_line: u32) -> bool {
        self.diff
            .as_ref()
            .map(|d| d.has_deletion_after(buffer_line))
            .unwrap_or(false)
    }

    pub fn buffer_chars_at(&self, point: Point) -> BufferCharsAt<'_> {
        let rope = &self.block_snapshot.buffer_snapshot().rope;
        let offset = rope.point_to_offset(point);
        BufferCharsAt {
            chars: rope.chars_at(offset),
            point,
        }
    }

    pub fn reverse_buffer_chars_at(&self, point: Point) -> ReversedBufferCharsAt<'_> {
        let rope = &self.block_snapshot.buffer_snapshot().rope;
        let offset = rope.point_to_offset(point);
        ReversedBufferCharsAt {
            chars: rope.reversed_chars_at(offset),
            point,
            rope,
        }
    }

    pub fn prev_line_boundary(&self, point: Point) -> (Point, DisplayPoint) {
        let display = self.buffer_to_display(point);
        let start = DisplayPoint::new(display.row, 0);
        let buf = self.display_to_buffer(start).unwrap_or(Point::zero());
        (buf, start)
    }

    pub fn next_line_boundary(&self, point: Point) -> (Point, DisplayPoint) {
        let display = self.buffer_to_display(point);
        let end = DisplayPoint::new(display.row, self.line_len(display.row));
        let max = self.block_snapshot.buffer_snapshot().rope.max_point();
        let buf = self.display_to_buffer(end).unwrap_or(max);
        (buf, end)
    }

    pub fn clip_at_line_end(&self, point: DisplayPoint) -> DisplayPoint {
        let clipped = self.clip_point(point, Bias::Left);
        DisplayPoint::new(clipped.row, clipped.column.min(self.line_len(clipped.row)))
    }
}

pub struct BufferCharsAt<'a> {
    chars: CharsAt<'a>,
    point: Point,
}

impl Iterator for BufferCharsAt<'_> {
    type Item = (char, Point);

    fn next(&mut self) -> Option<(char, Point)> {
        let ch = self.chars.next()?;
        let point = self.point;
        if ch == '\n' {
            self.point.row += 1;
            self.point.column = 0;
        } else {
            self.point.column += ch.len_utf8() as u32;
        }
        Some((ch, point))
    }
}

pub struct ReversedBufferCharsAt<'a> {
    chars: ReversedCharsAt<'a>,
    point: Point,
    rope: &'a Rope,
}

impl Iterator for ReversedBufferCharsAt<'_> {
    type Item = (char, Point);

    fn next(&mut self) -> Option<(char, Point)> {
        let ch = self.chars.next()?;
        if ch == '\n' {
            self.point.row -= 1;
            self.point.column = self.rope.line_len(self.point.row);
        } else {
            self.point.column -= ch.len_utf8() as u32;
        }
        Some((ch, self.point))
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockRowKind, DisplayMap, DisplayPoint, DisplayRow, InlayPoint};
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
    fn display_snapshot_version() {
        let mut dm = create_display_map("hello");
        let v1 = dm.snapshot().version();
        let v2 = dm.snapshot().version();
        assert_eq!(v1, v2);
    }

    #[test]
    fn passthrough_coordinates() {
        let mut display_map = create_display_map("hello\nworld\n");
        let snapshot = display_map.snapshot();

        let buffer_point = Point::new(1, 3);
        let display_point = snapshot.buffer_to_display(buffer_point);
        assert_eq!(display_point, DisplayPoint::new(1, 3));

        let back = snapshot.display_to_buffer(display_point);
        assert_eq!(back, Some(buffer_point));
    }

    #[test]
    fn line_count() {
        let mut display_map = create_display_map("line1\nline2\nline3");
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.line_count(), 3);
    }

    #[test]
    fn max_point() {
        let mut display_map = create_display_map("short\nlonger line\nx");
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
        let mut display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();

        assert_eq!(snapshot.line_count(), 3);
        assert_eq!(snapshot.buffer_line_count(), 2);
    }

    #[test]
    fn classify_deleted_row() {
        let base = "line1\ndeleted\nline2";
        let diff = make_diff_with_deletion(0, base, 6..13, 1);
        let mut display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();

        match snapshot.classify_row(1) {
            BlockRowKind::Block { block, line_index } => {
                assert_eq!(block.get_line(line_index), "deleted");
            },
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn roundtrip_with_tabs() {
        let mut display_map = create_display_map("\thello");
        let snapshot = display_map.snapshot();

        let display = snapshot.buffer_to_display(Point::new(0, 1));
        assert_eq!(display, DisplayPoint::new(0, 4));

        let back = snapshot.display_to_buffer(display).unwrap();
        assert_eq!(back, Point::new(0, 1));

        let display5 = DisplayPoint::new(0, 5);
        let back5 = snapshot.display_to_buffer(display5).unwrap();
        assert_eq!(back5, Point::new(0, 2));
    }

    #[test]
    fn roundtrip_with_folds() {
        let mut display_map = create_display_map("fn main() {\n    body;\n}");
        display_map.fold(vec![Point::new(0, 11)..Point::new(2, 0)]);
        let snapshot = display_map.snapshot();

        let display = snapshot.buffer_to_display(Point::new(2, 1));
        let back = snapshot.display_to_buffer(display).unwrap();
        assert_eq!(back, Point::new(2, 1));
    }

    #[test]
    fn line_len_display() {
        let mut display_map = create_display_map("\thello\nworld");
        let snapshot = display_map.snapshot();

        assert_eq!(snapshot.line_len(0), 9);
        assert_eq!(snapshot.line_len(1), 5);
    }

    #[test]
    fn clip_point_clamps() {
        use stoat_text::Bias;
        let mut display_map = create_display_map("hello\nhi");
        let snapshot = display_map.snapshot();

        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(0, 100), Bias::Left),
            DisplayPoint::new(0, 5)
        );
        assert_eq!(
            snapshot.clip_point(DisplayPoint::new(10, 0), Bias::Left),
            DisplayPoint::new(1, 0)
        );
    }

    #[test]
    fn toggle_fold_folds_then_unfolds() {
        let mut display_map = create_display_map("fn main() {\n    body;\n}");
        let range = vec![Point::new(0, 11)..Point::new(2, 0)];

        display_map.toggle_fold(range.clone());
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.line_count(), 1);

        display_map.toggle_fold(range);
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.line_count(), 3);
    }

    #[test]
    fn wrap_width_none_by_default() {
        let mut display_map = create_display_map("hello");
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.wrap_width(), None);
    }

    #[test]
    fn wrap_width_after_set() {
        let mut display_map = create_display_map("hello");
        display_map.set_wrap_width(Some(40));
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.wrap_width(), Some(40));
    }

    #[test]
    fn longest_row_no_blocks() {
        let mut display_map = create_display_map("short\nlonger line\nx");
        let snapshot = display_map.snapshot();
        let (row, chars) = snapshot.longest_row();
        assert_eq!(chars, 11);
        assert_eq!(row, 1);
    }

    #[test]
    fn longest_row_with_blocks() {
        let base = "line1\ndeleted long line here\nline2";
        let diff = make_diff_with_deletion(0, base, 6..28, 1);
        let mut display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();
        let (_, chars) = snapshot.longest_row();
        assert!(chars >= 5);
    }

    #[test]
    fn is_line_folded_through_display() {
        let mut display_map = create_display_map("line0\nline1\nline2\nline3");
        display_map.fold(vec![Point::new(1, 0)..Point::new(2, 5)]);
        let snapshot = display_map.snapshot();
        assert!(!snapshot.is_line_folded(0));
        assert!(snapshot.is_line_folded(1));
        assert!(snapshot.is_line_folded(2));
        assert!(!snapshot.is_line_folded(3));
    }

    #[test]
    fn buffer_chars_at_simple() {
        let mut display_map = create_display_map("hello");
        let snapshot = display_map.snapshot();
        let chars: Vec<(char, Point)> = snapshot.buffer_chars_at(Point::new(0, 0)).collect();
        assert_eq!(
            chars,
            vec![
                ('h', Point::new(0, 0)),
                ('e', Point::new(0, 1)),
                ('l', Point::new(0, 2)),
                ('l', Point::new(0, 3)),
                ('o', Point::new(0, 4)),
            ]
        );
    }

    #[test]
    fn buffer_chars_at_multiline() {
        let mut display_map = create_display_map("ab\ncd");
        let snapshot = display_map.snapshot();
        let chars: Vec<(char, Point)> = snapshot.buffer_chars_at(Point::new(0, 0)).collect();
        assert_eq!(
            chars,
            vec![
                ('a', Point::new(0, 0)),
                ('b', Point::new(0, 1)),
                ('\n', Point::new(0, 2)),
                ('c', Point::new(1, 0)),
                ('d', Point::new(1, 1)),
            ]
        );
    }

    #[test]
    fn reverse_buffer_chars_at_simple() {
        let mut display_map = create_display_map("hello");
        let snapshot = display_map.snapshot();
        let chars: Vec<(char, Point)> =
            snapshot.reverse_buffer_chars_at(Point::new(0, 5)).collect();
        assert_eq!(
            chars,
            vec![
                ('o', Point::new(0, 4)),
                ('l', Point::new(0, 3)),
                ('l', Point::new(0, 2)),
                ('e', Point::new(0, 1)),
                ('h', Point::new(0, 0)),
            ]
        );
    }

    #[test]
    fn reverse_buffer_chars_at_multiline() {
        let mut display_map = create_display_map("ab\ncd");
        let snapshot = display_map.snapshot();
        let chars: Vec<(char, Point)> =
            snapshot.reverse_buffer_chars_at(Point::new(1, 2)).collect();
        assert_eq!(
            chars,
            vec![
                ('d', Point::new(1, 1)),
                ('c', Point::new(1, 0)),
                ('\n', Point::new(0, 2)),
                ('b', Point::new(0, 1)),
                ('a', Point::new(0, 0)),
            ]
        );
    }

    #[test]
    fn prev_line_boundary_test() {
        let mut display_map = create_display_map("hello\nworld");
        let snapshot = display_map.snapshot();
        let (buf, display) = snapshot.prev_line_boundary(Point::new(1, 3));
        assert_eq!(buf, Point::new(1, 0));
        assert_eq!(display, DisplayPoint::new(1, 0));
    }

    #[test]
    fn next_line_boundary_test() {
        let mut display_map = create_display_map("hello\nworld");
        let snapshot = display_map.snapshot();
        let (buf, display) = snapshot.next_line_boundary(Point::new(0, 2));
        assert_eq!(buf, Point::new(0, 5));
        assert_eq!(display, DisplayPoint::new(0, 5));
    }

    #[test]
    fn clip_at_line_end_test() {
        let mut display_map = create_display_map("hello\nhi");
        let snapshot = display_map.snapshot();
        let clipped = snapshot.clip_at_line_end(DisplayPoint::new(0, 100));
        assert_eq!(clipped, DisplayPoint::new(0, 5));
    }

    #[test]
    fn inlay_survives_compaction() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("hello world");
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared.clone());
        let mut display_map = DisplayMap::new(multi_buffer);

        let snap = display_map.multi_buffer.snapshot();
        let off = snap.rope.point_to_offset(Point::new(0, 5));
        let anchor = snap.anchor_at(off, stoat_text::Bias::Right);
        display_map
            .inlay_map
            .splice(Vec::new(), vec![(anchor, ": str".to_string())]);

        for i in 0..10 {
            {
                let mut buf = shared.write().unwrap();
                let prefix = format!("{i}");
                buf.edit(0..0, &prefix);
            }
            let _ = display_map.snapshot();
        }

        let snapshot = display_map.snapshot();
        let inlay_snap = snapshot.inlay_snapshot();
        assert_eq!(
            inlay_snap.to_inlay_point(Point::new(0, 15)),
            InlayPoint::new(0, 20)
        );
    }

    #[test]
    fn soft_wrap_indent_exposed() {
        let mut display_map = create_display_map("    hello world foo");
        display_map.set_wrap_width(Some(8));
        let snapshot = display_map.snapshot();
        assert_eq!(snapshot.soft_wrap_indent(0), 0);
        if snapshot.line_count() > 1 {
            assert_eq!(snapshot.soft_wrap_indent(1), 4);
        }
    }

    #[test]
    fn display_lines_empty_range() {
        let mut display_map = create_display_map("hello\nworld");
        let snapshot = display_map.snapshot();
        let lines: Vec<String> = snapshot.display_lines(0..0).collect();
        assert!(lines.is_empty());
    }

    #[test]
    fn display_lines_multi_line() {
        let mut display_map = create_display_map("hello\nworld\nfoo");
        let snapshot = display_map.snapshot();
        let lines: Vec<String> = snapshot.display_lines(0..3).collect();
        assert_eq!(lines, vec!["hello", "world", "foo"]);
    }

    #[test]
    fn cjk_wide_chars_display_width() {
        let mut display_map = create_display_map("ab\u{4f60}\u{597d}cd");
        let snapshot = display_map.snapshot();
        // "ab" = 2, "你" = 2, "好" = 2, "cd" = 2 => total 8
        assert_eq!(snapshot.line_len(0), 8);
    }

    #[test]
    fn cjk_wrap_at_correct_column() {
        let mut display_map = create_display_map("ab\u{4f60}\u{597d}cd");
        display_map.set_wrap_width(Some(5));
        let snapshot = display_map.snapshot();
        // "ab你" = 4 cols, "好cd" = 4 cols -> wraps after 你
        assert_eq!(snapshot.line_count(), 2);
    }

    #[test]
    fn write_display_line_matches_display_line() {
        let base = "line1\ndeleted\nline2";
        let diff = make_diff_with_deletion(0, base, 6..13, 1);
        let mut display_map = create_display_map_with_diff("line1\nline2", diff);
        let snapshot = display_map.snapshot();
        for row in 0..snapshot.line_count() {
            let expected = snapshot.display_line(row);
            let mut buf = String::new();
            snapshot.write_display_line(&mut buf, row);
            assert_eq!(buf, expected, "mismatch at row {row}");
        }
    }
}
