use crate::buffer::SharedBuffer;
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
    buffer: SharedBuffer,
}

impl DisplayMap {
    pub fn new(buffer: SharedBuffer) -> Self {
        Self { buffer }
    }

    pub fn snapshot(&self) -> DisplaySnapshot {
        let buffer = self.buffer.read().expect("buffer lock poisoned");
        DisplaySnapshot {
            line_count: buffer.line_count(),
            text: buffer.rope.to_string(),
        }
    }
}

pub struct DisplaySnapshot {
    line_count: u32,
    text: String,
}

impl DisplaySnapshot {
    pub fn buffer_to_display(&self, point: Point) -> DisplayPoint {
        DisplayPoint {
            row: point.row,
            column: point.column,
        }
    }

    pub fn display_to_buffer(&self, point: DisplayPoint) -> Point {
        Point::new(point.row, point.column)
    }

    pub fn max_point(&self) -> DisplayPoint {
        let row = self.line_count.saturating_sub(1);
        let column = self
            .text
            .lines()
            .nth(row as usize)
            .map(|line| line.len() as u32)
            .unwrap_or(0);
        DisplayPoint::new(row, column)
    }

    pub fn line_count(&self) -> u32 {
        self.line_count
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.lines()
    }
}

#[cfg(test)]
mod tests {
    use super::{DisplayMap, DisplayPoint, DisplayRow};
    use crate::buffer::TextBuffer;
    use std::sync::{Arc, RwLock};
    use stoat_text::Point;

    #[test]
    fn passthrough_coordinates() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("hello\nworld\n");
        let shared = Arc::new(RwLock::new(buffer));
        let display_map = DisplayMap::new(shared);
        let snapshot = display_map.snapshot();

        let buffer_point = Point::new(1, 3);
        let display_point = snapshot.buffer_to_display(buffer_point);
        assert_eq!(display_point, DisplayPoint::new(1, 3));

        let back = snapshot.display_to_buffer(display_point);
        assert_eq!(back, buffer_point);
    }

    #[test]
    fn line_count() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("line1\nline2\nline3");
        let shared = Arc::new(RwLock::new(buffer));
        let display_map = DisplayMap::new(shared);
        let snapshot = display_map.snapshot();

        assert_eq!(snapshot.line_count(), 3);
    }

    #[test]
    fn max_point() {
        let mut buffer = TextBuffer::new();
        buffer.rope.push("short\nlonger line\nx");
        let shared = Arc::new(RwLock::new(buffer));
        let display_map = DisplayMap::new(shared);
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
}
