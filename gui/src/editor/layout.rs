//! Layout calculations and coordinate conversions.
//!
//! This module handles all layout-related calculations including viewport management,
//! coordinate conversions, and scrollbar geometry.

use super::buffer::TextBuffer;
use cosmic_text::Metrics;
use iced::{Point, Rectangle, Size};

/// Layout state for the text editor
#[derive(Clone)]
pub struct EditorLayout {
    /// Widget bounds
    pub bounds: Rectangle,
    /// Padding around text
    pub padding: f32,
    /// Width of line number gutter (if enabled)
    pub gutter_width: f32,
    /// Scrollbar width
    pub scrollbar_width: f32,
    /// Current scroll position
    pub scroll_x: f32,
    pub scroll_y: f32,
    /// Tab width in spaces
    pub tab_width: usize,
}

impl EditorLayout {
    /// Creates a new layout with default values
    pub fn new(tab_width: usize) -> Self {
        Self {
            bounds: Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0)),
            padding: 8.0,
            gutter_width: 0.0,
            scrollbar_width: 8.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            tab_width,
        }
    }

    /// Updates the widget bounds
    pub fn set_bounds(&mut self, bounds: Rectangle) {
        self.bounds = bounds;
    }

    /// Calculates the text area (excluding padding, gutter, scrollbar)
    pub fn text_area(&self) -> Rectangle {
        Rectangle::new(
            Point::new(
                self.bounds.x + self.padding + self.gutter_width,
                self.bounds.y + self.padding,
            ),
            Size::new(
                self.bounds.width - (self.padding * 2.0) - self.gutter_width - self.scrollbar_width,
                self.bounds.height - (self.padding * 2.0) - self.scrollbar_width,
            ),
        )
    }

    /// Converts a screen point to buffer coordinates
    pub fn screen_to_buffer(&self, point: Point) -> (f32, f32) {
        let text_area = self.text_area();
        let buffer_x = point.x - text_area.x + self.scroll_x;
        let buffer_y = point.y - text_area.y + self.scroll_y;
        (buffer_x, buffer_y)
    }

    /// Converts buffer coordinates to screen point
    pub fn buffer_to_screen(&self, buffer_x: f32, buffer_y: f32) -> Point {
        let text_area = self.text_area();
        Point::new(
            text_area.x + buffer_x - self.scroll_x,
            text_area.y + buffer_y - self.scroll_y,
        )
    }

    /// Calculates the number of characters needed for line numbers
    pub fn calculate_line_number_width(&self, line_count: usize) -> usize {
        let mut width = 1;
        let mut count = line_count;
        while count >= 10 {
            count /= 10;
            width += 1;
        }
        width
    }

    /// Updates the gutter width based on line count and font metrics
    pub fn update_gutter_width(&mut self, line_count: usize, char_width: f32, enabled: bool) {
        if enabled {
            let line_num_chars = self.calculate_line_number_width(line_count);
            // Add 2 for padding, 1 for separator
            self.gutter_width = ((line_num_chars + 3) as f32) * char_width;
        } else {
            self.gutter_width = 0.0;
        }
    }

    /// Calculate vertical scrollbar bounds
    pub fn vertical_scrollbar_bounds(
        &self,
        visible_start: usize,
        visible_end: usize,
        total_lines: usize,
    ) -> Rectangle {
        if total_lines == 0 {
            return Rectangle::new(Point::ORIGIN, Size::ZERO);
        }

        let scrollbar_x = self.bounds.x + self.bounds.width - self.scrollbar_width;
        let scrollbar_height = self.bounds.height - self.scrollbar_width; // Leave room for horizontal scrollbar

        let start_ratio = visible_start as f32 / total_lines as f32;
        let end_ratio = (visible_end + 1) as f32 / total_lines as f32;

        let thumb_y = self.bounds.y + scrollbar_height * start_ratio;
        let thumb_height = scrollbar_height * (end_ratio - start_ratio);

        Rectangle::new(
            Point::new(scrollbar_x, thumb_y),
            Size::new(self.scrollbar_width, thumb_height.max(20.0)), // Minimum thumb size
        )
    }

    /// Calculate horizontal scrollbar bounds
    pub fn horizontal_scrollbar_bounds(&self, buffer: &TextBuffer) -> Option<Rectangle> {
        let (buffer_width, _) = buffer.size();
        let buffer_width = buffer_width?;

        let text_area = self.text_area();
        if buffer_width <= text_area.width {
            return None; // No horizontal scrollbar needed
        }

        let scrollbar_y = self.bounds.y + self.bounds.height - self.scrollbar_width;
        let scrollbar_width = self.bounds.width - self.scrollbar_width;

        let visible_ratio = text_area.width / buffer_width;
        let scroll_ratio = self.scroll_x / (buffer_width - text_area.width);

        let thumb_width = (scrollbar_width * visible_ratio).max(20.0);
        let thumb_x = self.bounds.x + (scrollbar_width - thumb_width) * scroll_ratio;

        Some(Rectangle::new(
            Point::new(thumb_x, scrollbar_y),
            Size::new(thumb_width, self.scrollbar_width),
        ))
    }

    /// Ensures scroll position is within valid bounds
    pub fn clamp_scroll(&mut self, buffer: &TextBuffer) {
        let (buffer_width, buffer_height) = buffer.size();
        let text_area = self.text_area();

        // Clamp horizontal scroll
        if let Some(width) = buffer_width {
            let max_scroll_x = (width - text_area.width).max(0.0);
            self.scroll_x = self.scroll_x.clamp(0.0, max_scroll_x);
        } else {
            self.scroll_x = 0.0;
        }

        // Clamp vertical scroll
        if let Some(height) = buffer_height {
            let max_scroll_y = (height - text_area.height).max(0.0);
            self.scroll_y = self.scroll_y.clamp(0.0, max_scroll_y);
        } else {
            self.scroll_y = 0.0;
        }
    }

    /// Scrolls to ensure a position is visible
    pub fn ensure_visible(&mut self, x: f32, y: f32, metrics: Metrics) {
        let text_area = self.text_area();

        // Horizontal scrolling
        if x < self.scroll_x {
            self.scroll_x = x;
        } else if x > self.scroll_x + text_area.width - metrics.font_size {
            self.scroll_x = x - text_area.width + metrics.font_size;
        }

        // Vertical scrolling
        if y < self.scroll_y {
            self.scroll_y = y;
        } else if y + metrics.line_height > self.scroll_y + text_area.height {
            self.scroll_y = y + metrics.line_height - text_area.height;
        }
    }

    /// Gets visible line range based on scroll and viewport
    pub fn visible_line_range(&self, metrics: Metrics) -> (usize, usize) {
        let text_area = self.text_area();
        let start_line = (self.scroll_y / metrics.line_height) as usize;
        let visible_lines = (text_area.height / metrics.line_height).ceil() as usize;
        (start_line, start_line + visible_lines)
    }
}
