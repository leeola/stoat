use super::fold_map::{FoldPoint, FoldSnapshot};
use std::{ops::Deref, sync::Arc};
use stoat_text::Bias;

const MAX_EXPANSION_COLUMN: u32 = 256;

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabPoint(pub FoldPoint);

impl TabPoint {
    pub fn zero() -> Self {
        Self(FoldPoint::new(0, 0))
    }

    pub fn new(row: u32, column: u32) -> Self {
        Self(FoldPoint::new(row, column))
    }

    pub fn row(&self) -> u32 {
        self.0.row()
    }

    pub fn column(&self) -> u32 {
        self.0.column()
    }
}

impl From<FoldPoint> for TabPoint {
    fn from(point: FoldPoint) -> Self {
        Self(point)
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabRow(pub u32);

pub struct TabMap {
    tab_size: u32,
}

impl TabMap {
    pub fn new(tab_size: u32) -> Self {
        Self {
            tab_size: tab_size.max(1),
        }
    }

    pub fn set_tab_size(&mut self, size: u32) {
        self.tab_size = size.max(1);
    }

    pub fn sync(&self, fold_snapshot: Arc<FoldSnapshot>) -> TabSnapshot {
        TabSnapshot {
            fold_snapshot,
            tab_size: self.tab_size,
            max_expansion_column: MAX_EXPANSION_COLUMN,
        }
    }
}

#[derive(Clone)]
pub struct TabSnapshot {
    fold_snapshot: Arc<FoldSnapshot>,
    tab_size: u32,
    max_expansion_column: u32,
}

impl Deref for TabSnapshot {
    type Target = FoldSnapshot;
    fn deref(&self) -> &FoldSnapshot {
        &self.fold_snapshot
    }
}

impl TabSnapshot {
    pub fn fold_snapshot(&self) -> &FoldSnapshot {
        &self.fold_snapshot
    }

    pub fn tab_size(&self) -> u32 {
        self.tab_size
    }

    pub fn max_expansion_column(&self) -> u32 {
        self.max_expansion_column
    }

    pub fn to_tab_point(&self, fold_point: FoldPoint) -> TabPoint {
        let chars = self.fold_snapshot.fold_line_chars(fold_point.row());
        let expanded_column = expand_column(
            chars,
            fold_point.column(),
            self.tab_size,
            self.max_expansion_column,
        );
        TabPoint::new(fold_point.row(), expanded_column)
    }

    pub fn to_fold_point(&self, tab_point: TabPoint, bias: Bias) -> FoldPoint {
        let chars = self.fold_snapshot.fold_line_chars(tab_point.row());
        let fold_column = collapse_column(
            chars,
            tab_point.column(),
            self.tab_size,
            bias,
            self.max_expansion_column,
        );
        FoldPoint::new(tab_point.row(), fold_column)
    }

    pub fn line_len(&self, fold_row: u32) -> u32 {
        let fold_line_len = self.fold_snapshot.line_len(fold_row);
        expand_column(
            self.fold_snapshot.fold_line_chars(fold_row),
            fold_line_len,
            self.tab_size,
            self.max_expansion_column,
        )
    }

    pub fn clip_point(&self, point: TabPoint, _bias: Bias) -> TabPoint {
        let max_row = self.line_count().saturating_sub(1);
        let row = point.row().min(max_row);
        let max_col = self.line_len(row);
        let col = point.column().min(max_col);
        TabPoint::new(row, col)
    }

    pub fn write_expand_line(&self, buf: &mut String, fold_row: u32) {
        let mut column = 0u32;
        for ch in self.fold_snapshot.fold_line_chars(fold_row) {
            if ch == '\t' {
                let width = if column >= self.max_expansion_column {
                    1
                } else {
                    self.tab_size - (column % self.tab_size)
                };
                for _ in 0..width {
                    buf.push(' ');
                }
                column += width;
            } else {
                buf.push(ch);
                column += super::display_width(ch);
            }
        }
    }

    pub fn expand_line(&self, fold_row: u32) -> String {
        let mut result = String::new();
        self.write_expand_line(&mut result, fold_row);
        result
    }

    pub fn write_expand_line_range(
        &self,
        buf: &mut String,
        fold_row: u32,
        start_col: u32,
        end_col: Option<u32>,
    ) {
        let mut column = 0u32;
        for ch in self.fold_snapshot.fold_line_chars(fold_row) {
            let width = if ch == '\t' {
                if column >= self.max_expansion_column {
                    1
                } else {
                    self.tab_size - (column % self.tab_size)
                }
            } else {
                super::display_width(ch)
            };

            let next_column = column + width;

            if next_column <= start_col {
                column = next_column;
                continue;
            }
            if let Some(end) = end_col {
                if column >= end {
                    break;
                }
            }

            if ch == '\t' {
                let visible_start = start_col.max(column);
                let visible_end = end_col.map_or(next_column, |e| e.min(next_column));
                for _ in 0..(visible_end - visible_start) {
                    buf.push(' ');
                }
            } else {
                buf.push(ch);
            }
            column = next_column;
        }
    }

    pub fn expand_line_range(&self, fold_row: u32, start_col: u32, end_col: Option<u32>) -> String {
        let mut result = String::new();
        self.write_expand_line_range(&mut result, fold_row, start_col, end_col);
        result
    }

    pub fn line_count(&self) -> u32 {
        self.fold_snapshot.line_count()
    }
}

fn expand_column(
    chars: impl Iterator<Item = char>,
    fold_column: u32,
    tab_size: u32,
    max_expansion_column: u32,
) -> u32 {
    let mut expanded = 0u32;
    let mut byte_idx = 0u32;
    for ch in chars {
        if byte_idx >= fold_column {
            break;
        }
        if ch == '\t' {
            if expanded >= max_expansion_column {
                expanded += 1;
            } else {
                expanded += tab_size - (expanded % tab_size);
            }
        } else {
            expanded += super::display_width(ch);
        }
        byte_idx += ch.len_utf8() as u32;
    }
    expanded
}

fn collapse_column(
    chars: impl Iterator<Item = char>,
    tab_column: u32,
    tab_size: u32,
    bias: Bias,
    max_expansion_column: u32,
) -> u32 {
    let mut expanded = 0u32;
    let mut fold_col = 0u32;
    for ch in chars {
        if expanded >= tab_column {
            break;
        }
        let char_width = if ch == '\t' {
            if expanded >= max_expansion_column {
                1
            } else {
                tab_size - (expanded % tab_size)
            }
        } else {
            super::display_width(ch)
        };
        expanded += char_width;
        fold_col += ch.len_utf8() as u32;
    }
    if bias == Bias::Left && expanded > tab_column {
        fold_col = fold_col.saturating_sub(1);
    }
    fold_col
}

#[cfg(test)]
mod tests {
    use super::{TabMap, TabPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        display_map::{
            fold_map::{FoldMap, FoldPoint},
            inlay_map::InlayMap,
        },
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::Bias;

    fn make_snapshot(content: &str) -> super::TabSnapshot {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let (_, inlay_snapshot) = InlayMap::new(buffer_snapshot);
        let (_, fold_snapshot) = FoldMap::new(inlay_snapshot);
        let tab_map = TabMap::new(4);
        tab_map.sync(fold_snapshot)
    }

    #[test]
    fn no_tabs_passthrough() {
        let snap = make_snapshot("hello\nworld");
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 3)), TabPoint::new(0, 3));
        assert_eq!(
            snap.to_fold_point(TabPoint::new(0, 3), Bias::Left),
            FoldPoint::new(0, 3)
        );
        assert_eq!(snap.line_len(0), 5);
    }

    #[test]
    fn single_tab_expansion() {
        let snap = make_snapshot("\thello");
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 0)), TabPoint::new(0, 0));
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 1)), TabPoint::new(0, 4));
        assert_eq!(snap.line_len(0), 9);
    }

    #[test]
    fn tab_after_text() {
        let snap = make_snapshot("ab\tcd");
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 2)), TabPoint::new(0, 2));
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 3)), TabPoint::new(0, 4));
        assert_eq!(snap.line_len(0), 6);
    }

    #[test]
    fn multiple_tabs() {
        let snap = make_snapshot("\t\tx");
        assert_eq!(snap.to_tab_point(FoldPoint::new(0, 2)), TabPoint::new(0, 8));
        assert_eq!(snap.line_len(0), 9);
    }

    #[test]
    fn column_roundtrip() {
        let snap = make_snapshot("a\tb\tc");
        for col in 0..5u32 {
            let tab = snap.to_tab_point(FoldPoint::new(0, col));
            let back = snap.to_fold_point(tab, Bias::Left);
            assert_eq!(
                back,
                FoldPoint::new(0, col),
                "roundtrip failed for col {col}"
            );
        }
    }

    #[test]
    fn multiline() {
        let snap = make_snapshot("no tabs\n\tindented");
        assert_eq!(snap.line_len(0), 7);
        assert_eq!(snap.line_len(1), 12);
        assert_eq!(snap.to_tab_point(FoldPoint::new(1, 1)), TabPoint::new(1, 4));
    }

    #[test]
    fn bias_inside_tab() {
        let snap = make_snapshot("\thello");
        assert_eq!(
            snap.to_fold_point(TabPoint::new(0, 2), Bias::Left),
            FoldPoint::new(0, 0)
        );
        assert_eq!(
            snap.to_fold_point(TabPoint::new(0, 2), Bias::Right),
            FoldPoint::new(0, 1)
        );
    }

    #[test]
    fn clip_point_clamps() {
        let snap = make_snapshot("hello\nhi");
        assert_eq!(
            snap.clip_point(TabPoint::new(5, 0), Bias::Left),
            TabPoint::new(1, 0)
        );
        assert_eq!(
            snap.clip_point(TabPoint::new(0, 100), Bias::Left),
            TabPoint::new(0, 5)
        );
    }

    #[test]
    fn expand_line_range_full_line() {
        let snap = make_snapshot("\thello\tworld");
        let full = snap.expand_line(0);
        let ranged = snap.expand_line_range(0, 0, None);
        assert_eq!(ranged, full);
    }

    #[test]
    fn expand_line_range_with_tabs() {
        let snap = make_snapshot("\thello");
        assert_eq!(snap.expand_line_range(0, 0, Some(4)), "    ");
        assert_eq!(snap.expand_line_range(0, 4, None), "hello");
    }

    #[test]
    fn expand_line_range_partial_tab() {
        let snap = make_snapshot("\thello");
        assert_eq!(snap.expand_line_range(0, 2, Some(4)), "  ");
    }

    #[test]
    fn expand_line_range_cjk() {
        let snap = make_snapshot("\u{4e16}\u{754c}hello");
        // Each CJK char is 2 display columns wide
        assert_eq!(snap.expand_line_range(0, 0, Some(4)), "\u{4e16}\u{754c}");
        assert_eq!(snap.expand_line_range(0, 4, None), "hello");
    }

    #[test]
    fn max_expansion_column_caps_tabs() {
        let mut content = "x".repeat(260);
        content.push('\t');
        content.push('y');
        let snap = make_snapshot(&content);
        assert_eq!(
            snap.to_tab_point(FoldPoint::new(0, 261)),
            TabPoint::new(0, 261)
        );
        assert_eq!(snap.line_len(0), 262);
    }

    #[test]
    fn write_expand_line_matches_expand_line() {
        let snap = make_snapshot("\thello\tworld\nno tabs\n\t\tx");
        for row in 0..snap.line_count() {
            let expected = snap.expand_line(row);
            let mut buf = String::new();
            snap.write_expand_line(&mut buf, row);
            assert_eq!(buf, expected, "mismatch at row {row}");
        }
    }
}
