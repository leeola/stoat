use crate::multi_buffer::MultiBufferSnapshot;
use stoat_text::{Bias, Point};

const MAX_EXPANSION_COLUMN: u32 = 256;

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabPoint(pub Point);

impl TabPoint {
    pub fn zero() -> Self {
        Self(Point::zero())
    }

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

impl From<Point> for TabPoint {
    fn from(point: Point) -> Self {
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

    pub fn snapshot(&self, buffer_snapshot: &MultiBufferSnapshot) -> TabSnapshot {
        TabSnapshot {
            buffer_snapshot: buffer_snapshot.clone(),
            tab_size: self.tab_size,
            max_expansion_column: MAX_EXPANSION_COLUMN,
        }
    }
}

#[derive(Clone)]
pub struct TabSnapshot {
    buffer_snapshot: MultiBufferSnapshot,
    tab_size: u32,
    max_expansion_column: u32,
}

impl TabSnapshot {
    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        &self.buffer_snapshot
    }

    pub fn tab_size(&self) -> u32 {
        self.tab_size
    }

    pub fn to_tab_point(&self, buffer_point: Point) -> TabPoint {
        let line = self
            .buffer_snapshot
            .lines()
            .nth(buffer_point.row as usize)
            .unwrap_or("");
        let expanded_column = expand_column(
            line,
            buffer_point.column,
            self.tab_size,
            self.max_expansion_column,
        );
        TabPoint::new(buffer_point.row, expanded_column)
    }

    pub fn to_buffer_point(&self, tab_point: TabPoint, bias: Bias) -> Point {
        let line = self
            .buffer_snapshot
            .lines()
            .nth(tab_point.row() as usize)
            .unwrap_or("");
        let buffer_column = collapse_column(
            line,
            tab_point.column(),
            self.tab_size,
            bias,
            self.max_expansion_column,
        );
        Point::new(tab_point.row(), buffer_column)
    }

    pub fn line_len(&self, buffer_row: u32) -> u32 {
        let line = self
            .buffer_snapshot
            .lines()
            .nth(buffer_row as usize)
            .unwrap_or("");
        expand_column(
            line,
            line.len() as u32,
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

    pub fn line_count(&self) -> u32 {
        self.buffer_snapshot.line_count()
    }

    pub fn text(&self) -> &str {
        self.buffer_snapshot.text()
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.buffer_snapshot.lines()
    }
}

fn expand_column(line: &str, buffer_column: u32, tab_size: u32, max_expansion_column: u32) -> u32 {
    let mut expanded = 0u32;
    for (byte_idx, ch) in line.char_indices() {
        if byte_idx as u32 >= buffer_column {
            break;
        }
        if ch == '\t' {
            if expanded >= max_expansion_column {
                expanded += 1;
            } else {
                expanded += tab_size - (expanded % tab_size);
            }
        } else {
            expanded += 1;
        }
    }
    expanded
}

fn collapse_column(
    line: &str,
    tab_column: u32,
    tab_size: u32,
    bias: Bias,
    max_expansion_column: u32,
) -> u32 {
    let mut expanded = 0u32;
    let mut buffer_col = 0u32;
    for ch in line.chars() {
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
            1
        };
        expanded += char_width;
        buffer_col += ch.len_utf8() as u32;
    }
    if bias == Bias::Left && expanded > tab_column {
        buffer_col = buffer_col.saturating_sub(1);
    }
    buffer_col
}

#[cfg(test)]
mod tests {
    use super::{TabMap, TabPoint};
    use crate::{
        buffer::{BufferId, TextBuffer},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::{Bias, Point};

    fn make_snapshot(content: &str) -> super::TabSnapshot {
        let mut buffer = TextBuffer::new();
        buffer.rope.push(content);
        let shared = Arc::new(RwLock::new(buffer));
        let multi_buffer = MultiBuffer::singleton(BufferId::new(0), shared);
        let buffer_snapshot = multi_buffer.snapshot();
        let tab_map = TabMap::new(4);
        tab_map.snapshot(&buffer_snapshot)
    }

    #[test]
    fn no_tabs_passthrough() {
        let snap = make_snapshot("hello\nworld");
        assert_eq!(snap.to_tab_point(Point::new(0, 3)), TabPoint::new(0, 3));
        assert_eq!(
            snap.to_buffer_point(TabPoint::new(0, 3), Bias::Left),
            Point::new(0, 3)
        );
        assert_eq!(snap.line_len(0), 5);
    }

    #[test]
    fn single_tab_expansion() {
        let snap = make_snapshot("\thello");
        assert_eq!(snap.to_tab_point(Point::new(0, 0)), TabPoint::new(0, 0));
        assert_eq!(snap.to_tab_point(Point::new(0, 1)), TabPoint::new(0, 4));
        assert_eq!(snap.line_len(0), 9);
    }

    #[test]
    fn tab_after_text() {
        let snap = make_snapshot("ab\tcd");
        assert_eq!(snap.to_tab_point(Point::new(0, 2)), TabPoint::new(0, 2));
        assert_eq!(snap.to_tab_point(Point::new(0, 3)), TabPoint::new(0, 4));
        assert_eq!(snap.line_len(0), 6);
    }

    #[test]
    fn multiple_tabs() {
        let snap = make_snapshot("\t\tx");
        assert_eq!(snap.to_tab_point(Point::new(0, 2)), TabPoint::new(0, 8));
        assert_eq!(snap.line_len(0), 9);
    }

    #[test]
    fn column_roundtrip() {
        let snap = make_snapshot("a\tb\tc");
        for col in 0..5u32 {
            let tab = snap.to_tab_point(Point::new(0, col));
            let back = snap.to_buffer_point(tab, Bias::Left);
            assert_eq!(back, Point::new(0, col), "roundtrip failed for col {col}");
        }
    }

    #[test]
    fn multiline() {
        let snap = make_snapshot("no tabs\n\tindented");
        assert_eq!(snap.line_len(0), 7);
        assert_eq!(snap.line_len(1), 12);
        assert_eq!(snap.to_tab_point(Point::new(1, 1)), TabPoint::new(1, 4));
    }

    #[test]
    fn bias_inside_tab() {
        let snap = make_snapshot("\thello");
        // Tab column 2 is inside the tab expansion (0..4)
        assert_eq!(
            snap.to_buffer_point(TabPoint::new(0, 2), Bias::Left),
            Point::new(0, 0)
        );
        assert_eq!(
            snap.to_buffer_point(TabPoint::new(0, 2), Bias::Right),
            Point::new(0, 1)
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
    fn max_expansion_column_caps_tabs() {
        let mut content = "x".repeat(260);
        content.push('\t');
        content.push('y');
        let snap = make_snapshot(&content);
        // Tab at expanded col 260 >= 256, expands by 1 instead of 4
        assert_eq!(snap.to_tab_point(Point::new(0, 261)), TabPoint::new(0, 261));
        assert_eq!(snap.line_len(0), 262);
    }
}
