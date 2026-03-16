use super::tab_map::{TabPoint, TabSnapshot};
use std::{cmp::Ordering, ops::Deref, sync::Arc};
use stoat_text::{Bias, ContextLessSummary, Dimension, Dimensions, Item, SeekTarget, SumTree};

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WrapPoint(pub TabPoint);

impl WrapPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self(TabPoint::new(row, column))
    }

    pub fn row(&self) -> u32 {
        self.0.row()
    }

    pub fn column(&self) -> u32 {
        self.0.column()
    }
}

impl From<TabPoint> for WrapPoint {
    fn from(point: TabPoint) -> Self {
        Self(point)
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WrapRow(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WrapRowKind {
    Primary,
    Continuation,
}

#[derive(Clone, Debug, Default)]
struct TransformSummary {
    input_rows: u32,
    output_rows: u32,
}

impl ContextLessSummary for TransformSummary {
    fn add_summary(&mut self, other: &Self) {
        self.input_rows += other.input_rows;
        self.output_rows += other.output_rows;
    }
}

#[derive(Clone, Debug)]
struct Transform {
    summary: TransformSummary,
    wrap_columns: Vec<u32>,
}

impl Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self, _cx: ()) -> TransformSummary {
        self.summary.clone()
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct InputRow(u32);

impl<'a> Dimension<'a, TransformSummary> for InputRow {
    fn zero(_cx: ()) -> Self {
        InputRow(0)
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _cx: ()) {
        self.0 += summary.input_rows;
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct OutputRow(u32);

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

pub struct WrapMap {
    wrap_width: Option<u32>,
    cached_snapshot: Option<Arc<WrapSnapshot>>,
    last_fold_version: usize,
    last_buffer_version: usize,
    last_inlay_version: usize,
    last_wrap_width: Option<u32>,
}

#[derive(Clone)]
pub struct WrapSnapshot {
    tab_snapshot: TabSnapshot,
    transforms: SumTree<Transform>,
    wrap_width: Option<u32>,
    total_rows: u32,
    longest_row: u32,
    longest_row_chars: u32,
}

impl Deref for WrapSnapshot {
    type Target = TabSnapshot;
    fn deref(&self) -> &TabSnapshot {
        &self.tab_snapshot
    }
}

impl WrapMap {
    pub fn new(tab_snapshot: TabSnapshot, wrap_width: Option<u32>) -> (Self, Arc<WrapSnapshot>) {
        let fold_version = tab_snapshot.fold_snapshot().version();
        let buffer_version = tab_snapshot.fold_snapshot().inlay_snapshot().version;
        let inlay_version = tab_snapshot.fold_snapshot().inlay_snapshot().inlay_version;
        let snapshot = Arc::new(build_snapshot(tab_snapshot, wrap_width));
        let map = WrapMap {
            wrap_width,
            cached_snapshot: Some(Arc::clone(&snapshot)),
            last_fold_version: fold_version,
            last_buffer_version: buffer_version,
            last_inlay_version: inlay_version,
            last_wrap_width: wrap_width,
        };
        (map, snapshot)
    }

    pub fn sync(&mut self, tab_snapshot: TabSnapshot) -> Arc<WrapSnapshot> {
        let fold_version = tab_snapshot.fold_snapshot().version();
        let buffer_version = tab_snapshot.fold_snapshot().inlay_snapshot().version;
        let inlay_version = tab_snapshot.fold_snapshot().inlay_snapshot().inlay_version;
        if fold_version == self.last_fold_version
            && buffer_version == self.last_buffer_version
            && inlay_version == self.last_inlay_version
            && self.wrap_width == self.last_wrap_width
        {
            if let Some(ref cached) = self.cached_snapshot {
                return Arc::clone(cached);
            }
        }

        let snapshot = if let Some(ref old) = self.cached_snapshot {
            if self.wrap_width.is_some()
                && old.tab_snapshot.line_count() == tab_snapshot.line_count()
            {
                Arc::new(rebuild_incremental(
                    old,
                    tab_snapshot,
                    self.wrap_width,
                    self.last_wrap_width,
                ))
            } else {
                Arc::new(build_snapshot(tab_snapshot, self.wrap_width))
            }
        } else {
            Arc::new(build_snapshot(tab_snapshot, self.wrap_width))
        };

        self.last_fold_version = fold_version;
        self.last_buffer_version = buffer_version;
        self.last_inlay_version = inlay_version;
        self.last_wrap_width = self.wrap_width;
        self.cached_snapshot = Some(Arc::clone(&snapshot));
        snapshot
    }

    pub fn set_wrap_width(&mut self, width: Option<u32>) {
        self.wrap_width = width;
    }

    pub fn wrap_width(&self) -> Option<u32> {
        self.wrap_width
    }
}

fn build_snapshot(tab_snapshot: TabSnapshot, wrap_width: Option<u32>) -> WrapSnapshot {
    let tab_line_count = tab_snapshot.line_count();
    let mut transforms = SumTree::new(());
    let mut total_rows = 0u32;
    let mut longest_row = 0u32;
    let mut longest_row_chars = 0u32;

    for tab_row in 0..tab_line_count {
        let tab_line_len = tab_snapshot.line_len(tab_row);

        let wrap_columns = match wrap_width {
            None => vec![0],
            Some(width) => {
                let chars = tab_snapshot.fold_snapshot().fold_line_chars(tab_row);
                compute_wrap_columns(
                    chars,
                    tab_line_len,
                    width,
                    tab_snapshot.tab_size(),
                    tab_snapshot.max_expansion_column(),
                )
            },
        };

        let output_rows = wrap_columns.len() as u32;

        for sub_idx in 0..wrap_columns.len() {
            let sub_len = if sub_idx + 1 < wrap_columns.len() {
                wrap_columns[sub_idx + 1] - wrap_columns[sub_idx]
            } else {
                tab_line_len - wrap_columns[sub_idx]
            };
            if sub_len > longest_row_chars {
                longest_row = total_rows + sub_idx as u32;
                longest_row_chars = sub_len;
            }
        }

        total_rows += output_rows;

        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: 1,
                    output_rows,
                },
                wrap_columns,
            },
            (),
        );
    }

    WrapSnapshot {
        tab_snapshot,
        transforms,
        wrap_width,
        total_rows,
        longest_row,
        longest_row_chars,
    }
}

fn rebuild_incremental(
    old: &WrapSnapshot,
    tab_snapshot: TabSnapshot,
    wrap_width: Option<u32>,
    old_wrap_width: Option<u32>,
) -> WrapSnapshot {
    let width = match wrap_width {
        Some(w) => w,
        None => return build_snapshot(tab_snapshot, wrap_width),
    };
    let width_unchanged = wrap_width == old_wrap_width;
    let line_count = tab_snapshot.line_count();
    let mut transforms = SumTree::new(());
    let mut total_rows = 0u32;
    let mut longest_row = 0u32;
    let mut longest_row_chars = 0u32;

    let mut old_cursor = old.transforms.cursor::<Dimensions<InputRow, OutputRow>>(());

    for tab_row in 0..line_count {
        let new_line_len = tab_snapshot.line_len(tab_row);

        old_cursor.seek_forward(&InputRow(tab_row + 1), Bias::Left);

        let wrap_columns = if let Some(old_transform) = old_cursor.item() {
            let old_line_len = old.tab_snapshot.line_len(tab_row);
            if new_line_len == old_line_len && width_unchanged {
                old_transform.wrap_columns.clone()
            } else if new_line_len <= width {
                vec![0]
            } else {
                let chars = tab_snapshot.fold_snapshot().fold_line_chars(tab_row);
                compute_wrap_columns(
                    chars,
                    new_line_len,
                    width,
                    tab_snapshot.tab_size(),
                    tab_snapshot.max_expansion_column(),
                )
            }
        } else {
            let chars = tab_snapshot.fold_snapshot().fold_line_chars(tab_row);
            compute_wrap_columns(
                chars,
                new_line_len,
                width,
                tab_snapshot.tab_size(),
                tab_snapshot.max_expansion_column(),
            )
        };

        let output_rows = wrap_columns.len() as u32;

        for sub_idx in 0..wrap_columns.len() {
            let sub_len = if sub_idx + 1 < wrap_columns.len() {
                wrap_columns[sub_idx + 1] - wrap_columns[sub_idx]
            } else {
                new_line_len - wrap_columns[sub_idx]
            };
            if sub_len > longest_row_chars {
                longest_row = total_rows + sub_idx as u32;
                longest_row_chars = sub_len;
            }
        }

        total_rows += output_rows;

        transforms.push(
            Transform {
                summary: TransformSummary {
                    input_rows: 1,
                    output_rows,
                },
                wrap_columns,
            },
            (),
        );
    }

    WrapSnapshot {
        tab_snapshot,
        transforms,
        wrap_width,
        total_rows,
        longest_row,
        longest_row_chars,
    }
}

fn compute_wrap_columns(
    chars: impl Iterator<Item = char>,
    tab_line_len: u32,
    width: u32,
    tab_size: u32,
    max_expansion_column: u32,
) -> Vec<u32> {
    if width == 0 || tab_line_len <= width {
        return vec![0];
    }

    let mut breaks = vec![0u32];
    let mut expanded_col = 0u32;
    let mut last_break_candidate: Option<u32> = None;

    for ch in chars {
        let char_width = if ch == '\t' {
            if expanded_col >= max_expansion_column {
                1
            } else {
                tab_size - (expanded_col % tab_size)
            }
        } else {
            super::display_width(ch)
        };

        if ch == ' ' || ch == '\t' {
            last_break_candidate = Some(expanded_col + char_width);
        }

        expanded_col += char_width;

        let segment_start = *breaks.last().unwrap();
        if expanded_col - segment_start >= width {
            let break_at = match last_break_candidate {
                Some(b) if b > segment_start => b,
                _ => expanded_col,
            };
            breaks.push(break_at);
            last_break_candidate = None;
        }
    }

    if breaks.len() > 1 && *breaks.last().unwrap() >= tab_line_len {
        breaks.pop();
    }

    breaks
}

impl WrapSnapshot {
    pub fn tab_snapshot(&self) -> &TabSnapshot {
        &self.tab_snapshot
    }

    pub fn wrap_width(&self) -> Option<u32> {
        self.wrap_width
    }

    pub fn to_tab_point(&self, wrap_point: WrapPoint) -> TabPoint {
        if self.wrap_width.is_none() {
            return TabPoint::new(wrap_point.row(), wrap_point.column());
        }

        let target = OutputRow(wrap_point.row() + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let sub_row = wrap_point.row() - output_start.0;

        if let Some(transform) = cursor.item() {
            let tab_col = transform.wrap_columns[sub_row as usize] + wrap_point.column();
            TabPoint::new(input_start.0, tab_col)
        } else {
            let last_tab_row = input_start.0.saturating_sub(1);
            TabPoint::new(last_tab_row, wrap_point.column())
        }
    }

    pub fn to_wrap_point(&self, tab_point: TabPoint) -> WrapPoint {
        if self.wrap_width.is_none() {
            return WrapPoint::new(tab_point.row(), tab_point.column());
        }

        let target = InputRow(tab_point.row() + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(_input_start, output_start, _) = cursor.start();

        if let Some(transform) = cursor.item() {
            let tab_col = tab_point.column();
            let sub_row = transform
                .wrap_columns
                .partition_point(|&c| c <= tab_col)
                .saturating_sub(1);
            let wrap_col = tab_col - transform.wrap_columns[sub_row];
            WrapPoint::new(output_start.0 + sub_row as u32, wrap_col)
        } else {
            WrapPoint::new(output_start.0, tab_point.column())
        }
    }

    pub fn classify_row(&self, wrap_row: u32) -> WrapRowKind {
        if self.wrap_width.is_none() {
            return WrapRowKind::Primary;
        }

        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let sub_row = wrap_row - cursor.start().1 .0;
        if sub_row == 0 {
            WrapRowKind::Primary
        } else {
            WrapRowKind::Continuation
        }
    }

    pub fn clip_point(&self, point: WrapPoint, _bias: Bias) -> WrapPoint {
        let max_row = self.total_rows.saturating_sub(1);
        let row = point.row().min(max_row);
        let max_col = self.line_len(row);
        let col = point.column().min(max_col);
        WrapPoint::new(row, col)
    }

    pub fn line_len(&self, wrap_row: u32) -> u32 {
        if self.wrap_width.is_none() {
            return self.tab_snapshot.line_len(wrap_row);
        }

        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let sub_row = wrap_row - output_start.0;

        if let Some(transform) = cursor.item() {
            let next_idx = sub_row as usize + 1;
            if next_idx < transform.wrap_columns.len() {
                transform.wrap_columns[next_idx] - transform.wrap_columns[sub_row as usize]
            } else {
                let tab_line_len = self.tab_snapshot.line_len(input_start.0);
                tab_line_len - transform.wrap_columns[sub_row as usize]
            }
        } else {
            0
        }
    }

    pub fn soft_wrap_indent(&self, wrap_row: u32) -> u32 {
        if self.wrap_width.is_none() {
            return 0;
        }

        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let sub_row = wrap_row - cursor.start().1 .0;
        if sub_row == 0 {
            return 0;
        }

        let tab_row = cursor.start().0 .0;
        self.tab_snapshot
            .fold_snapshot()
            .fold_line_chars(tab_row)
            .take_while(|c| c.is_whitespace())
            .count() as u32
    }

    pub fn write_display_line(&self, buf: &mut String, wrap_row: u32) {
        if self.wrap_width.is_none() {
            self.tab_snapshot.write_expand_line(buf, wrap_row);
            return;
        }

        let target = OutputRow(wrap_row + 1);
        let mut cursor = self
            .transforms
            .cursor::<Dimensions<InputRow, OutputRow>>(());
        cursor.seek(&target, Bias::Left);

        let Dimensions(input_start, output_start, _) = cursor.start();
        let sub_row = (wrap_row - output_start.0) as usize;
        let tab_row = input_start.0;

        if let Some(transform) = cursor.item() {
            let start_col = transform.wrap_columns[sub_row];
            let end_col = if sub_row + 1 < transform.wrap_columns.len() {
                Some(transform.wrap_columns[sub_row + 1])
            } else {
                None
            };
            self.tab_snapshot
                .write_expand_line_range(buf, tab_row, start_col, end_col);
        } else {
            self.tab_snapshot.write_expand_line(buf, tab_row);
        }
    }

    pub fn display_line(&self, wrap_row: u32) -> String {
        let mut result = String::new();
        self.write_display_line(&mut result, wrap_row);
        result
    }

    pub fn longest_line(&self) -> (u32, u32) {
        (self.longest_row, self.longest_row_chars)
    }

    pub fn line_count(&self) -> u32 {
        self.total_rows
    }
}

#[cfg(test)]
mod tests {
    use super::{WrapMap, WrapPoint, WrapRowKind};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::{
            fold_map::FoldMap,
            inlay_map::InlayMap,
            tab_map::{TabMap, TabPoint},
        },
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};

    fn make_snapshot(content: &str, wrap_width: Option<u32>) -> Arc<super::WrapSnapshot> {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let tab_snapshot = tab_map.sync(fold_snapshot);
        let (_, wrap_snapshot) = WrapMap::new(tab_snapshot, wrap_width);
        wrap_snapshot
    }

    #[test]
    fn no_wrap_passthrough() {
        let snap = make_snapshot("hello\nworld", None);
        assert_eq!(snap.line_count(), 2);
        let tp = TabPoint::new(1, 3);
        let wp = snap.to_wrap_point(tp);
        assert_eq!(wp, WrapPoint::new(1, 3));
        let back = snap.to_tab_point(wp);
        assert_eq!(back, tp);
    }

    #[test]
    fn short_lines_no_wrap() {
        let snap = make_snapshot("ab\ncd\nef", Some(10));
        assert_eq!(snap.line_count(), 3);
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(2, 1)),
            WrapPoint::new(2, 1)
        );
    }

    #[test]
    fn single_long_line_wraps() {
        let snap = make_snapshot("abcdefghij", Some(5));
        assert_eq!(snap.line_count(), 2);

        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 0)),
            WrapPoint::new(0, 0)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 3)),
            WrapPoint::new(0, 3)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 5)),
            WrapPoint::new(1, 0)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 8)),
            WrapPoint::new(1, 3)
        );

        assert_eq!(snap.to_tab_point(WrapPoint::new(0, 3)), TabPoint::new(0, 3));
        assert_eq!(snap.to_tab_point(WrapPoint::new(1, 0)), TabPoint::new(0, 5));
        assert_eq!(snap.to_tab_point(WrapPoint::new(1, 3)), TabPoint::new(0, 8));
    }

    #[test]
    fn multiple_wraps_one_line() {
        let snap = make_snapshot("abcdefghijklmno", Some(5));
        assert_eq!(snap.line_count(), 3);
    }

    #[test]
    fn mixed_lines() {
        let snap = make_snapshot("ab\nabcdefghij\ncd", Some(5));
        assert_eq!(snap.line_count(), 4);

        assert_eq!(
            snap.to_wrap_point(TabPoint::new(0, 1)),
            WrapPoint::new(0, 1)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(1, 7)),
            WrapPoint::new(2, 2)
        );
        assert_eq!(
            snap.to_wrap_point(TabPoint::new(2, 1)),
            WrapPoint::new(3, 1)
        );
    }

    #[test]
    fn classify_primary_and_continuation() {
        let snap = make_snapshot("abcdefghij", Some(5));
        assert_eq!(snap.classify_row(0), WrapRowKind::Primary);
        assert_eq!(snap.classify_row(1), WrapRowKind::Continuation);
    }

    #[test]
    fn line_len_no_wrap() {
        let snap = make_snapshot("hello\nhi", None);
        assert_eq!(snap.line_len(0), 5);
        assert_eq!(snap.line_len(1), 2);
    }

    #[test]
    fn line_len_wrapped() {
        let snap = make_snapshot("abcdefghij", Some(5));
        assert_eq!(snap.line_len(0), 5);
        assert_eq!(snap.line_len(1), 5);
    }

    #[test]
    fn line_len_wrapped_remainder() {
        let snap = make_snapshot("abcdefgh", Some(5));
        assert_eq!(snap.line_len(0), 5);
        assert_eq!(snap.line_len(1), 3);
    }

    #[test]
    fn word_boundary_wrap() {
        let snap = make_snapshot("hello world foo", Some(8));
        assert_eq!(snap.line_count(), 3);
        assert_eq!(snap.line_len(0), 6);
        assert_eq!(snap.line_len(1), 6);
        assert_eq!(snap.line_len(2), 3);
    }

    #[test]
    fn word_boundary_roundtrip() {
        let snap = make_snapshot("hello world foo", Some(8));

        let wp = snap.to_wrap_point(TabPoint::new(0, 0));
        assert_eq!(wp, WrapPoint::new(0, 0));
        assert_eq!(snap.to_tab_point(wp), TabPoint::new(0, 0));

        let wp = snap.to_wrap_point(TabPoint::new(0, 5));
        assert_eq!(wp, WrapPoint::new(0, 5));

        let wp = snap.to_wrap_point(TabPoint::new(0, 6));
        assert_eq!(wp, WrapPoint::new(1, 0));
        assert_eq!(snap.to_tab_point(wp), TabPoint::new(0, 6));

        let wp = snap.to_wrap_point(TabPoint::new(0, 12));
        assert_eq!(wp, WrapPoint::new(2, 0));
        assert_eq!(snap.to_tab_point(wp), TabPoint::new(0, 12));
    }

    #[test]
    fn long_word_hard_wraps() {
        let snap = make_snapshot("abcdefghijklmno", Some(8));
        assert_eq!(snap.line_count(), 2);
        assert_eq!(snap.line_len(0), 8);
        assert_eq!(snap.line_len(1), 7);
    }

    #[test]
    fn soft_wrap_indent_primary() {
        let snap = make_snapshot("    hello world foo", Some(8));
        assert_eq!(snap.soft_wrap_indent(0), 0);
    }

    #[test]
    fn soft_wrap_indent_continuation() {
        let snap = make_snapshot("    hello world foo", Some(8));
        assert!(snap.line_count() > 1);
        assert_eq!(snap.soft_wrap_indent(1), 4);
    }

    fn make_wrap_map(
        content: &str,
        wrap_width: Option<u32>,
    ) -> (WrapMap, Arc<super::WrapSnapshot>, MultiBuffer) {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let tab_snapshot = tab_map.sync(fold_snapshot);
        let (wrap_map, wrap_snapshot) = WrapMap::new(tab_snapshot, wrap_width);
        (wrap_map, wrap_snapshot, multi_buffer)
    }

    fn resync(multi_buffer: &MultiBuffer, wrap_map: &mut WrapMap) -> Arc<super::WrapSnapshot> {
        let snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let tab_snapshot = tab_map.sync(fold_snapshot);
        wrap_map.sync(tab_snapshot)
    }

    #[test]
    fn incremental_sync_matches_full_rebuild() {
        let (mut wrap_map, _, multi_buffer) = make_wrap_map("abcdefghij\nshort\nxy", Some(5));

        multi_buffer
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(0..1, "ZZ");

        let incremental = resync(&multi_buffer, &mut wrap_map);

        let full_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(full_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let tab_snapshot = tab_map.sync(fold_snapshot);
        let full = super::build_snapshot(tab_snapshot, Some(5));

        assert_eq!(incremental.line_count(), full.line_count());
        assert_eq!(incremental.longest_row, full.longest_row);
        assert_eq!(incremental.longest_row_chars, full.longest_row_chars);
        for row in 0..full.line_count() {
            assert_eq!(
                incremental.line_len(row),
                full.line_len(row),
                "line_len mismatch at row {row}"
            );
        }
    }

    #[test]
    fn incremental_sync_after_line_count_change() {
        let (mut wrap_map, _, multi_buffer) = make_wrap_map("abcdefghij\nshort", Some(5));

        multi_buffer
            .as_singleton()
            .unwrap()
            .write()
            .unwrap()
            .edit(5..5, "\nnewline");

        let result = resync(&multi_buffer, &mut wrap_map);

        let full_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(full_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let tab_snapshot = tab_map.sync(fold_snapshot);
        let full = super::build_snapshot(tab_snapshot, Some(5));

        assert_eq!(result.line_count(), full.line_count());
        for row in 0..full.line_count() {
            assert_eq!(
                result.line_len(row),
                full.line_len(row),
                "line_len mismatch at row {row}"
            );
        }
    }

    #[test]
    fn write_display_line_matches_display_line() {
        let snap = make_snapshot("abcdefghij\nshort\nxy", Some(5));
        for row in 0..snap.line_count() {
            let expected = snap.display_line(row);
            let mut buf = String::new();
            snap.write_display_line(&mut buf, row);
            assert_eq!(buf, expected, "mismatch at row {row}");
        }
    }

    fn assert_incremental_matches_full(content: &str, old_width: u32, new_width: u32) {
        let (mut wrap_map, _, multi_buffer) = make_wrap_map(content, Some(old_width));
        wrap_map.set_wrap_width(Some(new_width));
        let incremental = resync(&multi_buffer, &mut wrap_map);

        let full_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(full_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        let tab_snapshot = tab_map.sync(fold_snapshot);
        let full = super::build_snapshot(tab_snapshot, Some(new_width));

        assert_eq!(incremental.line_count(), full.line_count());
        assert_eq!(incremental.longest_row, full.longest_row);
        assert_eq!(incremental.longest_row_chars, full.longest_row_chars);
        for row in 0..full.line_count() {
            assert_eq!(
                incremental.line_len(row),
                full.line_len(row),
                "line_len mismatch at row {row}"
            );
            assert_eq!(
                incremental.display_line(row),
                full.display_line(row),
                "display_line mismatch at row {row}"
            );
        }
    }

    #[test]
    fn incremental_sync_on_width_increase() {
        assert_incremental_matches_full("abcdefghij\nshort\nxy", 5, 20);
    }

    #[test]
    fn incremental_sync_on_width_decrease() {
        assert_incremental_matches_full("abcdefghij\nshort\nxy", 20, 5);
    }

    #[test]
    fn wrap_respects_max_expansion_column() {
        let mut content = "x".repeat(260);
        content.push('\t');
        content.push_str("abcdef");
        // Tab at col 260 is past MAX_EXPANSION_COLUMN (256), so width = 1.
        // Total expanded length = 260 + 1 + 6 = 267, which fits in 270.
        let snap = make_snapshot(&content, Some(270));
        assert_eq!(snap.line_count(), 1);
    }
}
