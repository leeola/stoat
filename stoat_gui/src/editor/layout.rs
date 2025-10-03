use gpui::{Bounds, Pixels, Point, ShapedLine};
use smallvec::SmallVec;

/// Layout state computed in prepaint
pub struct EditorLayout {
    /// The shaped lines ready to paint
    pub lines: SmallVec<[PositionedLine; 32]>,
    /// Total bounds of the editor
    pub bounds: Bounds<Pixels>,
    /// Content area (excluding padding)
    pub content_bounds: Bounds<Pixels>,
    /// Line height for positioning
    pub line_height: Pixels,
    /// Scroll position (in rows)
    pub scroll_position: Point<f32>,
    /// First visible row
    pub start_row: u32,
}

/// A shaped line with its rendering position
pub struct PositionedLine {
    pub shaped: ShapedLine,
    pub position: Point<Pixels>,
}

impl EditorLayout {
    /// Convert a pixel position to a text position (row, column).
    ///
    /// Returns `None` if the click is outside the text area.
    pub fn position_for_pixel(&self, pixel: Point<Pixels>) -> Option<text::Point> {
        // Check if pixel is within content bounds
        if !self.content_bounds.contains(&pixel) {
            return None;
        }

        // Calculate relative position within content area
        let relative_pos = pixel - self.content_bounds.origin;

        // Calculate row from Y position
        let row_f = (relative_pos.y / self.line_height) + self.scroll_position.y;
        let row = row_f.max(0.0) as u32;

        // Find the corresponding line layout
        let line_index = (row - self.start_row) as usize;
        if line_index >= self.lines.len() {
            // Click below last visible line - return end of last line
            let last_row = self.start_row + self.lines.len() as u32 - 1;
            let last_line = self.lines.last()?;
            return Some(text::Point::new(last_row, last_line.shaped.len as u32));
        }

        let positioned_line = &self.lines[line_index];

        // Calculate column from X position using the shaped line
        let x_in_line = relative_pos.x;
        let column = if let Some(ix) = positioned_line.shaped.index_for_x(x_in_line) {
            ix as u32
        } else {
            // Click past end of line
            positioned_line.shaped.len as u32
        };

        Some(text::Point::new(row, column))
    }
}
