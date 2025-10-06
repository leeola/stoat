use super::gutter::GutterLayout;
use gpui::{Bounds, Pixels, Point, ShapedLine};
use smallvec::SmallVec;

/// Layout state computed in prepaint
pub struct EditorLayout {
    /// The shaped lines ready to paint
    pub lines: SmallVec<[PositionedLine; 32]>,
    /// Buffer line lengths (actual character count, not shaped)
    pub line_lengths: SmallVec<[u32; 32]>,
    /// Total bounds of the editor
    pub bounds: Bounds<Pixels>,
    /// Content area (excluding padding and gutter)
    pub content_bounds: Bounds<Pixels>,
    /// Line height for positioning
    pub line_height: Pixels,
    /// Scroll position (in rows)
    pub scroll_position: Point<f32>,
    /// First visible row
    pub start_row: u32,
    /// Gutter layout (git diff indicators, line numbers, etc.)
    pub gutter: Option<GutterLayout>,
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
            let last_line_length = self.line_lengths.last().copied().unwrap_or(0);
            return Some(text::Point::new(last_row, last_line_length));
        }

        let positioned_line = &self.lines[line_index];
        let buffer_line_length = self.line_lengths[line_index];

        // Calculate column from X position using the shaped line
        let x_in_line = relative_pos.x;
        let shaped_column = if let Some(ix) = positioned_line.shaped.index_for_x(x_in_line) {
            ix as u32
        } else {
            // Click past end of line
            positioned_line.shaped.len as u32
        };

        // Clamp to actual buffer line length
        let column = shaped_column.min(buffer_line_length);

        Some(text::Point::new(row, column))
    }
}
